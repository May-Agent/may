//! GitHub issues backend over the REST API.
//!
//! Credentials come from the GitHub CLI login: `gh auth token` is run by the
//! harness (never inside the sandbox) and the token only leaves the process
//! as an `Authorization` header — it is never logged or put in model
//! context.

use zenwave::header::{ACCEPT, USER_AGENT};
use zenwave::{Client, Method, ResponseExt};

use super::{
    IssueBackendInfo, IssueComment, IssueDetail, IssueError, IssueListState, IssueRelationKind,
    IssueState, IssueStore, IssueSummary, RelatedIssue,
};
use crate::workspace::GitHubRepository;

const DEFAULT_API_BASE: &str = "https://api.github.com";

/// Provides a GitHub API token for [`GitHubIssueStore`].
pub trait GitHubTokenSource: Send + Sync + std::fmt::Debug {
    /// Fetch a token; implementations must not log or persist it.
    fn token(&self) -> impl Future<Output = Result<String, IssueError>> + Send;
}

/// Token source running `gh auth token` on the host.
#[derive(Debug, Clone, Copy)]
pub struct GhCliTokenSource;

impl GitHubTokenSource for GhCliTokenSource {
    async fn token(&self) -> Result<String, IssueError> {
        let output = blocking::unblock(|| {
            std::process::Command::new("gh")
                .arg("auth")
                .arg("token")
                .output()
        })
        .await
        .map_err(|error| IssueError::GitHubAuth(format!("failed to run gh auth token: {error}")))?;
        if !output.status.success() {
            return Err(IssueError::GitHubAuth(format!(
                "gh auth token failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            return Err(IssueError::GitHubAuth(
                "gh auth token returned an empty token".to_string(),
            ));
        }
        Ok(token)
    }
}

/// GitHub issues backend for one repository.
#[derive(Debug)]
pub struct GitHubIssueStore<T> {
    repository: GitHubRepository,
    api_base: String,
    token_source: T,
}

impl<T: GitHubTokenSource + Clone> Clone for GitHubIssueStore<T> {
    fn clone(&self) -> Self {
        Self {
            repository: self.repository.clone(),
            api_base: self.api_base.clone(),
            token_source: self.token_source.clone(),
        }
    }
}

impl<T: GitHubTokenSource> GitHubIssueStore<T> {
    /// Store targeting the repository's issues on `api.github.com`.
    #[must_use]
    pub fn new(repository: GitHubRepository, token_source: T) -> Self {
        Self::with_api_base(repository, token_source, DEFAULT_API_BASE)
    }

    /// Store targeting a custom API base URL (tests, GitHub Enterprise).
    #[must_use]
    pub fn with_api_base(repository: GitHubRepository, token_source: T, api_base: &str) -> Self {
        Self {
            repository,
            api_base: api_base.trim_end_matches('/').to_string(),
            token_source,
        }
    }

    /// `owner/repo` of the configured repository.
    #[must_use]
    pub const fn repository(&self) -> &GitHubRepository {
        &self.repository
    }

    fn issue_url(&self, number: i64) -> String {
        format!(
            "{}/repos/{}/issues/{number}",
            self.api_base, self.repository
        )
    }

    async fn request(
        &self,
        method: Method,
        url: String,
        body: Option<serde_json::Value>,
    ) -> Result<zenwave::Response, IssueError> {
        let response = self.request_unchecked(method, url, body).await?;
        if response.status().as_u16() == 404 {
            return Err(IssueError::NotFound(-1));
        }
        if !response.status().is_success() {
            let status = response.status();
            let message = response.into_string().await.map_or_else(
                |_| "<unreadable error body>".to_string(),
                |text| text.to_string(),
            );
            return Err(IssueError::GitHub(format!(
                "{status} from GitHub API: {message}"
            )));
        }
        Ok(response)
    }

    /// Sends an authenticated request and returns the raw response without
    /// the status check, for endpoints where a documented error status
    /// carries meaning (label already exists).
    async fn request_unchecked(
        &self,
        method: Method,
        url: String,
        body: Option<serde_json::Value>,
    ) -> Result<zenwave::Response, IssueError> {
        let token = self.token_source.token().await?;
        let mut client = zenwave::client();
        let mut request = client
            .method(method, url.as_str())
            .map_err(|error| IssueError::GitHub(error.to_string()))?
            .bearer_auth(token)
            .header(ACCEPT, "application/vnd.github+json")
            .and_then(|request| request.header(USER_AGENT, "may-agent"))
            .map_err(|error| IssueError::GitHub(error.to_string()))?;
        if let Some(body) = body {
            request = request
                .json_body(&body)
                .map_err(|error| IssueError::GitHub(error.to_string()))?;
        }
        match request.await {
            Ok(response) => Ok(response),
            Err(zenwave::Error::Http { response, .. }) => Ok(response.response),
            Err(error) => Err(IssueError::GitHub(error.to_string())),
        }
    }

    async fn get_issue(&self, number: i64) -> Result<GhIssue, IssueError> {
        let response = self
            .request(Method::GET, self.issue_url(number), None)
            .await
            .map_err(|error| match error {
                IssueError::NotFound(_) => IssueError::NotFound(number),
                other => other,
            })?;
        response
            .into_json::<GhIssue>()
            .await
            .map_err(|error| IssueError::GitHub(error.to_string()))
    }

    async fn request_json_list<R: serde::de::DeserializeOwned>(
        &self,
        url: String,
    ) -> Result<Vec<R>, IssueError> {
        let response = self.request(Method::GET, url, None).await?;
        response
            .into_json::<Vec<R>>()
            .await
            .map_err(|error| IssueError::GitHub(error.to_string()))
    }

    async fn ensure_label(&self, name: &str) -> Result<(), IssueError> {
        let response = self
            .request_unchecked(
                Method::POST,
                format!("{}/repos/{}/labels", self.api_base, self.repository),
                Some(serde_json::json!({"name": name})),
            )
            .await?;
        if response.status().is_success() || response.status().as_u16() == 422 {
            return Ok(());
        }
        Err(IssueError::GitHub(format!(
            "{} creating label {name}",
            response.status()
        )))
    }
}

impl<T: GitHubTokenSource> IssueStore for GitHubIssueStore<T> {
    fn info(&self) -> IssueBackendInfo {
        IssueBackendInfo {
            name: "github",
            repository: Some(self.repository.to_string()),
        }
    }

    async fn create(
        &self,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<IssueDetail, IssueError> {
        for label in labels {
            self.ensure_label(label).await?;
        }
        let response = self
            .request(
                Method::POST,
                format!("{}/repos/{}/issues", self.api_base, self.repository),
                Some(serde_json::json!({
                    "title": title,
                    "body": body,
                    "labels": labels,
                })),
            )
            .await?;
        let issue = response
            .into_json::<GhIssue>()
            .await
            .map_err(|error| IssueError::GitHub(error.to_string()))?;
        self.get(issue.number).await
    }

    async fn get(&self, number: i64) -> Result<IssueDetail, IssueError> {
        let issue_url = self.issue_url(number);
        let comments_url = format!("{issue_url}/comments");
        let sub_issues_url = format!("{issue_url}/sub_issues");
        let blocked_by_url = format!("{issue_url}/dependencies/blocked_by");
        let (issue, comments, sub_issues, blocked_by) = futures_util::try_join!(
            async {
                self.request(Method::GET, issue_url, None)
                    .await?
                    .into_json::<GhIssue>()
                    .await
                    .map_err(|error| IssueError::GitHub(error.to_string()))
            },
            self.request_json_list(comments_url),
            self.request_json_list(sub_issues_url),
            self.request_json_list(blocked_by_url),
        )
        .map_err(|error| match error {
            IssueError::NotFound(_) => IssueError::NotFound(number),
            other => other,
        })?;
        let labels = issue.label_names();
        Ok(IssueDetail {
            number: issue.number,
            title: issue.title,
            body: issue.body.unwrap_or_default(),
            state: parse_state(&issue.state, number)?,
            labels,
            comments: comments
                .into_iter()
                .map(|comment: GhComment| IssueComment {
                    author: comment.user.map(|user| user.login),
                    body: comment.body,
                    created_at: comment.created_at,
                })
                .collect(),
            sub_issues: sub_issues
                .into_iter()
                .map(related_issue)
                .collect::<Result<_, _>>()?,
            blocked_by: blocked_by
                .into_iter()
                .map(related_issue)
                .collect::<Result<_, _>>()?,
            created_at: issue.created_at,
            updated_at: issue.updated_at,
        })
    }

    async fn list(
        &self,
        state: IssueListState,
        labels: &[String],
        limit: i64,
    ) -> Result<Vec<IssueSummary>, IssueError> {
        let mut url = format!(
            "{}/repos/{}/issues?state={}&per_page={}",
            self.api_base,
            self.repository,
            state.as_str(),
            limit.clamp(1, 100)
        );
        if !labels.is_empty() {
            url.push_str("&labels=");
            url.push_str(&labels.join(","));
        }
        let response = self.request(Method::GET, url, None).await?;
        let issues = response
            .into_json::<Vec<GhIssue>>()
            .await
            .map_err(|error| IssueError::GitHub(error.to_string()))?;
        issues
            .into_iter()
            .filter(|issue| issue.pull_request.is_none())
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
            .map(|issue| {
                let labels = issue.label_names();
                Ok(IssueSummary {
                    number: issue.number,
                    title: issue.title,
                    state: parse_state(&issue.state, issue.number)?,
                    labels,
                    updated_at: issue.updated_at,
                })
            })
            .collect()
    }

    async fn edit(
        &self,
        number: i64,
        title: Option<&str>,
        body: Option<&str>,
        state: Option<IssueState>,
    ) -> Result<IssueDetail, IssueError> {
        let mut patch = serde_json::Map::new();
        if let Some(title) = title {
            patch.insert("title".to_string(), title.into());
        }
        if let Some(body) = body {
            patch.insert("body".to_string(), body.into());
        }
        if let Some(state) = state {
            patch.insert("state".to_string(), state.as_str().into());
        }
        if patch.is_empty() {
            return self.get(number).await;
        }
        self.request(
            Method::PATCH,
            self.issue_url(number),
            Some(serde_json::Value::Object(patch)),
        )
        .await
        .map_err(|error| match error {
            IssueError::NotFound(_) => IssueError::NotFound(number),
            other => other,
        })?;
        self.get(number).await
    }

    async fn comment(&self, number: i64, body: &str) -> Result<IssueDetail, IssueError> {
        self.request(
            Method::POST,
            format!("{}/comments", self.issue_url(number)),
            Some(serde_json::json!({"body": body})),
        )
        .await
        .map_err(|error| match error {
            IssueError::NotFound(_) => IssueError::NotFound(number),
            other => other,
        })?;
        self.get(number).await
    }

    async fn edit_labels(
        &self,
        number: i64,
        add: &[String],
        remove: &[String],
    ) -> Result<IssueDetail, IssueError> {
        for label in add {
            self.ensure_label(label).await?;
        }
        if !add.is_empty() {
            self.request(
                Method::POST,
                format!("{}/labels", self.issue_url(number)),
                Some(serde_json::json!({"labels": add})),
            )
            .await
            .map_err(|error| match error {
                IssueError::NotFound(_) => IssueError::NotFound(number),
                other => other,
            })?;
        }
        for label in remove {
            self.request(
                Method::DELETE,
                format!("{}/labels/{label}", self.issue_url(number)),
                None,
            )
            .await
            .map_err(|error| match error {
                IssueError::NotFound(_) => IssueError::NotFound(number),
                other => other,
            })?;
        }
        self.get(number).await
    }

    async fn edit_relation(
        &self,
        number: i64,
        kind: IssueRelationKind,
        target_number: i64,
        add: bool,
    ) -> Result<IssueDetail, IssueError> {
        if number == target_number {
            return Err(IssueError::Invalid(
                "an issue cannot relate to itself".to_string(),
            ));
        }
        let target = self.get_issue(target_number).await?;
        let relation_path = match kind {
            IssueRelationKind::SubIssue => "sub_issues",
            IssueRelationKind::BlockedBy => "dependencies/blocked_by",
        };
        if add {
            let body_key = match kind {
                IssueRelationKind::SubIssue => "sub_issue_id",
                IssueRelationKind::BlockedBy => "issue_id",
            };
            self.request(
                Method::POST,
                format!("{}/{relation_path}", self.issue_url(number)),
                Some(serde_json::json!({body_key: target.id})),
            )
            .await
            .map_err(|error| match error {
                IssueError::NotFound(_) => IssueError::NotFound(number),
                other => other,
            })?;
        } else {
            self.request(
                Method::DELETE,
                format!("{}/{relation_path}/{}", self.issue_url(number), target.id),
                None,
            )
            .await
            .map_err(|error| match error {
                IssueError::NotFound(_) => IssueError::NotFound(number),
                other => other,
            })?;
        }
        self.get(number).await
    }
}

fn related_issue(issue: GhIssueRef) -> Result<RelatedIssue, IssueError> {
    Ok(RelatedIssue {
        number: issue.number,
        title: issue.title,
        state: parse_state(&issue.state, issue.number)?,
    })
}

fn parse_state(state: &str, number: i64) -> Result<IssueState, IssueError> {
    state
        .parse()
        .map_err(|error: String| IssueError::GitHub(format!("issue #{number}: {error}")))
}

/// Label objects can arrive as plain strings or objects on some API shapes.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum GhLabel {
    Name(String),
    Object { name: String },
}

#[derive(Debug, serde::Deserialize)]
struct GhUser {
    login: String,
}

#[derive(Debug, serde::Deserialize)]
struct GhComment {
    body: String,
    created_at: String,
    user: Option<GhUser>,
}

#[derive(Debug, serde::Deserialize)]
struct GhIssueRef {
    number: i64,
    title: String,
    state: String,
}

#[derive(Debug, serde::Deserialize)]
struct GhIssue {
    id: i64,
    number: i64,
    title: String,
    body: Option<String>,
    state: String,
    #[serde(default)]
    labels: Vec<GhLabel>,
    created_at: String,
    updated_at: String,
    pull_request: Option<serde_json::Value>,
}

impl GhIssue {
    fn label_names(&self) -> Vec<String> {
        self.labels
            .iter()
            .map(|label| match label {
                GhLabel::Name(name) | GhLabel::Object { name } => name.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};

    use hyper::body::Bytes;
    use hyper::header::AUTHORIZATION;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;

    use super::*;
    use crate::issue::IssueListState;

    #[derive(Debug)]
    struct StaticToken;

    impl GitHubTokenSource for StaticToken {
        fn token(&self) -> impl Future<Output = Result<String, IssueError>> + Send {
            std::future::ready(Ok("test-token".to_string()))
        }
    }

    type Requests = Arc<Mutex<Vec<(String, String, String, String)>>>;

    /// Spin up a mock GitHub API on a loopback port. `handler` receives the
    /// request method, path and body and returns `(status, json_body)`.
    async fn mock_api(
        handler: impl Fn(&str, &str, &str) -> (u16, serde_json::Value) + Send + Sync + 'static,
    ) -> (String, Requests) {
        let handler = Arc::new(handler);
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        let requests_in = requests.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock api");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept");
                let requests = requests_in.clone();
                let handler = handler.clone();
                let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                    let requests = requests.clone();
                    let handler = handler.clone();
                    async move {
                        let (parts, body) = request.into_parts();
                        let auth = parts
                            .headers
                            .get(AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let body = http_body_util::BodyExt::collect(body)
                            .await
                            .map(http_body_util::Collected::to_bytes)
                            .unwrap_or_default();
                        let body = String::from_utf8_lossy(&body).to_string();
                        let path = parts
                            .uri
                            .path_and_query()
                            .map_or("", |pq| pq.as_str())
                            .to_string();
                        let method = parts.method.to_string();
                        requests.lock().expect("requests").push((
                            method.clone(),
                            path.clone(),
                            auth,
                            body.clone(),
                        ));
                        let (status, json) = handler(&method, &path, &body);
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(status)
                                .header("content-type", "application/json")
                                .body(http_body_util::Full::new(Bytes::from(json.to_string())))
                                .expect("response"),
                        )
                    }
                });
                tokio::spawn(async move {
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        (format!("http://{addr}"), requests)
    }

    fn gh_issue_json(number: i64, title: &str, state: &str) -> serde_json::Value {
        serde_json::json!({
            "id": number * 100,
            "number": number,
            "title": title,
            "body": "body",
            "state": state,
            "labels": [],
            "created_at": "2026-10-01T00:00:00Z",
            "updated_at": "2026-10-02T00:00:00Z",
        })
    }

    fn store(api_base: &str) -> GitHubIssueStore<StaticToken> {
        GitHubIssueStore::with_api_base("o/r".parse().expect("repository"), StaticToken, api_base)
    }

    #[tokio::test]
    async fn create_get_and_relations_hit_github_api() {
        let (api_base, requests) = mock_api(|method, path, body| match (method, path) {
            ("POST", "/repos/o/r/labels") => (201, serde_json::json!({"name": "bug"})),
            ("POST", "/repos/o/r/issues") => (201, gh_issue_json(1, "created", "open")),
            ("GET", "/repos/o/r/issues/1") => (200, gh_issue_json(1, "created", "open")),
            ("GET", "/repos/o/r/issues/2") => (200, gh_issue_json(2, "blocker", "open")),
            (
                "GET",
                "/repos/o/r/issues/1/comments"
                | "/repos/o/r/issues/1/sub_issues"
                | "/repos/o/r/issues/1/dependencies/blocked_by",
            ) => (200, serde_json::json!([])),
            ("POST", "/repos/o/r/issues/1/dependencies/blocked_by") => {
                assert!(body.contains("\"issue_id\":200"), "{body}");
                (201, gh_issue_json(1, "created", "open"))
            }
            _ => panic!("unexpected request {method} {path}"),
        })
        .await;
        let store = store(&api_base);

        let created = store
            .create("created", "body", &["bug".to_string()])
            .await
            .expect("create");
        assert_eq!(created.number, 1);

        let detail = store
            .edit_relation(1, IssueRelationKind::BlockedBy, 2, true)
            .await
            .expect("blocked-by add");
        assert_eq!(detail.number, 1);

        {
            let requests = requests.lock().expect("requests");
            assert!(
                requests
                    .iter()
                    .all(|(_, _, auth, _)| auth == "Bearer test-token"),
                "every request must carry the token: {requests:?}"
            );
            assert!(
                requests
                    .iter()
                    .all(|(_, _, _, body)| !body.contains("test-token")),
                "token must never appear in a request body"
            );
            assert!(requests.iter().any(|(method, path, _, _)| {
                method == "POST" && path == "/repos/o/r/issues/1/dependencies/blocked_by"
            }));
            drop(requests);
        }
    }

    #[tokio::test]
    async fn list_filters_pull_requests() {
        let mut pr = gh_issue_json(4, "a pr", "open");
        pr["pull_request"] = serde_json::json!({"url": "https://example/pr"});
        let issues_json = serde_json::json!([gh_issue_json(3, "real issue", "open"), pr]);
        let (api_base, _requests) = mock_api(move |method, path, _body| match (method, path) {
            ("GET", path) if path.starts_with("/repos/o/r/issues?") => (200, issues_json.clone()),
            _ => panic!("unexpected request {method} {path}"),
        })
        .await;
        let issues = store(&api_base)
            .list(IssueListState::All, &[], 50)
            .await
            .expect("list");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 3);
    }

    #[tokio::test]
    async fn missing_issue_maps_to_not_found() {
        let (api_base, _requests) =
            mock_api(|_method, _path, _body| (404, serde_json::json!({"message": "Not Found"})))
                .await;
        assert!(matches!(
            store(&api_base).get(99).await,
            Err(IssueError::NotFound(99))
        ));
    }
}
