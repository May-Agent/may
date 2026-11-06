//! GitHub issues backend over the REST API.
//!
//! Credentials come from the GitHub CLI login: `gh auth token` is run by the
//! harness (never inside the sandbox) and the token only leaves the process
//! as an `Authorization` header — it is never logged or put in model
//! context.

use zenwave::header::{ACCEPT, LINK, USER_AGENT};
use zenwave::{Client, Method, ResponseExt};

use super::{
    CommentedIssue, IssueBackendInfo, IssueComment, IssueDetail, IssueError, IssueListState,
    IssueRelationKind, IssueState, IssueStore, IssueSummary, RelatedIssue,
};
use crate::workspace::GitHubRepository;

/// Public GitHub REST API base.
const DEFAULT_API_BASE: &str = "https://api.github.com";
/// GitHub's maximum `per_page`.
const PAGE_SIZE: usize = 100;

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

/// Authenticated GitHub REST client shared by the issue and pull request
/// backends: one API base, one token source, one request path.
#[derive(Debug, Clone)]
pub struct GitHubRest<T> {
    api_base: String,
    token_source: T,
}

impl<T: GitHubTokenSource> GitHubRest<T> {
    /// Client targeting `api.github.com`.
    #[must_use]
    pub fn new(token_source: T) -> Self {
        Self::with_api_base(token_source, DEFAULT_API_BASE)
    }

    /// Client targeting a custom API base URL (tests, GitHub Enterprise).
    #[must_use]
    pub fn with_api_base(token_source: T, api_base: &str) -> Self {
        Self {
            api_base: api_base.trim_end_matches('/').to_string(),
            token_source,
        }
    }

    /// The API base URL, without a trailing slash.
    #[must_use]
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// Sends an authenticated request; a 404 is [`IssueError::NotFound`]
    /// with the placeholder number `-1` for the caller to replace, any
    /// other non-success status is [`IssueError::GitHub`].
    ///
    /// # Errors
    /// Fails on token, transport or status errors.
    pub async fn request(
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
    ///
    /// # Errors
    /// Fails on token or transport errors.
    pub async fn request_unchecked(
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

    /// The login the token authenticates as.
    ///
    /// # Errors
    /// Fails on token, transport or status errors.
    pub async fn viewer_login(&self) -> Result<String, IssueError> {
        let user = self
            .request(Method::GET, format!("{}/user", self.api_base), None)
            .await?
            .into_json::<GhUser>()
            .await
            .map_err(|error| IssueError::GitHub(error.to_string()))?;
        Ok(user.login)
    }

    /// Whether the authenticated user owns `repository`: the owner's login
    /// is theirs, or they hold admin permission on it.
    ///
    /// # Errors
    /// Fails on token, transport or status errors, a missing repository
    /// included: a failed lookup never counts as owned.
    pub async fn repository_owned(
        &self,
        repository: &GitHubRepository,
    ) -> Result<bool, IssueError> {
        let url = format!("{}/repos/{repository}", self.api_base);
        let (viewer, details) = futures_lite::future::try_zip(self.viewer_login(), async {
            self.request(Method::GET, url, None)
                .await?
                .into_json::<GhRepository>()
                .await
                .map_err(|error| IssueError::GitHub(error.to_string()))
        })
        .await?;
        Ok(details.owner.login.eq_ignore_ascii_case(&viewer)
            || details.permissions.is_some_and(|p| p.admin))
    }

    /// Whether the authenticated user's account opened issue or pull
    /// request `number` of `repository`. The agent publishes through that
    /// account, so what the agent opened counts too.
    ///
    /// # Errors
    /// Fails on token, transport or status errors.
    pub async fn opened_by_viewer(
        &self,
        repository: &GitHubRepository,
        number: i64,
    ) -> Result<bool, IssueError> {
        let url = format!("{}/repos/{repository}/issues/{number}", self.api_base);
        let (viewer, issue) = futures_lite::future::try_zip(self.viewer_login(), async {
            self.request(Method::GET, url, None)
                .await
                .map_err(|error| match error {
                    IssueError::NotFound(_) => IssueError::NotFound(number),
                    other => other,
                })?
                .into_json::<GhAuthored>()
                .await
                .map_err(|error| IssueError::GitHub(error.to_string()))
        })
        .await?;
        Ok(issue
            .user
            .is_some_and(|user| user.login.eq_ignore_ascii_case(&viewer)))
    }
}

#[derive(Debug, serde::Deserialize)]
struct GhRepository {
    owner: GhUser,
    permissions: Option<GhPermissions>,
}

#[derive(Debug, serde::Deserialize)]
struct GhPermissions {
    #[serde(default)]
    admin: bool,
}

#[derive(Debug, serde::Deserialize)]
struct GhAuthored {
    user: Option<GhUser>,
}

/// GitHub issues backend for one repository.
#[derive(Debug)]
pub struct GitHubIssueStore<T> {
    repository: GitHubRepository,
    rest: GitHubRest<T>,
}

impl<T: GitHubTokenSource + Clone> Clone for GitHubIssueStore<T> {
    fn clone(&self) -> Self {
        Self {
            repository: self.repository.clone(),
            rest: self.rest.clone(),
        }
    }
}

impl<T: GitHubTokenSource> GitHubIssueStore<T> {
    /// Store targeting the repository's issues on `api.github.com`.
    #[must_use]
    pub fn new(repository: GitHubRepository, token_source: T) -> Self {
        Self::with_rest(repository, GitHubRest::new(token_source))
    }

    /// Store targeting a custom API base URL (tests, GitHub Enterprise).
    #[must_use]
    pub fn with_api_base(repository: GitHubRepository, token_source: T, api_base: &str) -> Self {
        Self::with_rest(
            repository,
            GitHubRest::with_api_base(token_source, api_base),
        )
    }

    /// Store sharing an existing REST client.
    #[must_use]
    pub const fn with_rest(repository: GitHubRepository, rest: GitHubRest<T>) -> Self {
        Self { repository, rest }
    }

    /// `owner/repo` of the configured repository.
    #[must_use]
    pub const fn repository(&self) -> &GitHubRepository {
        &self.repository
    }

    fn api_base(&self) -> &str {
        self.rest.api_base()
    }

    fn issue_url(&self, number: i64) -> String {
        format!(
            "{}/repos/{}/issues/{number}",
            self.api_base(),
            self.repository
        )
    }

    async fn request(
        &self,
        method: Method,
        url: String,
        body: Option<serde_json::Value>,
    ) -> Result<zenwave::Response, IssueError> {
        self.rest.request(method, url, body).await
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

    /// One page of a list endpoint and the URL of the next page, if any.
    async fn request_page<R: serde::de::DeserializeOwned>(
        &self,
        url: String,
    ) -> Result<(Vec<R>, Option<String>), IssueError> {
        let response = self.request(Method::GET, url, None).await?;
        let next = response
            .headers()
            .get_all(LINK)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(next_page_url);
        let items = response
            .into_json::<Vec<R>>()
            .await
            .map_err(|error| IssueError::GitHub(error.to_string()))?;
        Ok((items, next))
    }

    /// Every item of a list endpoint, following `Link: rel="next"` to the end.
    async fn request_json_list<R: serde::de::DeserializeOwned>(
        &self,
        url: String,
    ) -> Result<Vec<R>, IssueError> {
        let mut items = Vec::new();
        let mut next = Some(with_page_size(&url));
        while let Some(url) = next {
            let (page, following) = self.request_page::<R>(url).await?;
            items.extend(page);
            next = following;
        }
        Ok(items)
    }

    async fn ensure_label(&self, name: &str) -> Result<(), IssueError> {
        let response = self
            .rest
            .request_unchecked(
                Method::POST,
                format!("{}/repos/{}/labels", self.api_base(), self.repository),
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

    fn search_key(&self) -> String {
        format!("github:{}", self.repository)
    }

    async fn owned_by_user(&self) -> Result<bool, IssueError> {
        self.rest.repository_owned(&self.repository).await
    }

    async fn opened_by_user(&self, number: i64) -> Result<bool, IssueError> {
        self.rest.opened_by_viewer(&self.repository, number).await
    }

    async fn updated_since(
        &self,
        since: Option<&str>,
        limit: i64,
    ) -> Result<Vec<IssueSummary>, IssueError> {
        let mut url = format!(
            "{}/repos/{}/issues?state=all&sort=updated&direction=asc&per_page={PAGE_SIZE}",
            self.api_base(),
            self.repository,
        );
        if let Some(since) = since {
            url.push_str("&since=");
            url.push_str(since);
        }
        let limit = usize::try_from(limit).unwrap_or(0);
        let mut issues: Vec<GhIssue> = Vec::new();
        let mut newer = 0_usize;
        let mut next = Some(url);
        // Pages are followed past pull requests until `limit` issues newer
        // than `since` are held; the last timestamp group is completed so a
        // tie never straddles the cut. GitHub's `since` is inclusive, so the
        // issues at the watermark come back too.
        'pages: while let Some(url) = next {
            let (page, following) = self.request_page::<GhIssue>(url).await?;
            for issue in page {
                if issue.pull_request.is_some() {
                    continue;
                }
                if newer >= limit
                    && issues
                        .last()
                        .is_none_or(|last| last.updated_at != issue.updated_at)
                {
                    break 'pages;
                }
                if since.is_none_or(|since| issue.updated_at.as_str() > since) {
                    newer += 1;
                }
                issues.push(issue);
            }
            next = following;
        }
        issues
            .into_iter()
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
                format!("{}/repos/{}/issues", self.api_base(), self.repository),
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
                    id: comment.id,
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
            self.api_base(),
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

    async fn comment(&self, number: i64, body: &str) -> Result<CommentedIssue, IssueError> {
        let created = self
            .request(
                Method::POST,
                format!("{}/comments", self.issue_url(number)),
                Some(serde_json::json!({"body": body})),
            )
            .await
            .map_err(|error| match error {
                IssueError::NotFound(_) => IssueError::NotFound(number),
                other => other,
            })?
            .into_json::<GhComment>()
            .await
            .map_err(|error| IssueError::GitHub(error.to_string()))?;
        Ok(CommentedIssue {
            detail: self.get(number).await?,
            comment_id: created.id,
        })
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
            // Sub-issues are removed by `DELETE …/sub_issue` with the id in the
            // body; blocked-by dependencies carry the id in the path.
            let (url, body) = match kind {
                IssueRelationKind::SubIssue => (
                    format!("{}/sub_issue", self.issue_url(number)),
                    Some(serde_json::json!({"sub_issue_id": target.id})),
                ),
                IssueRelationKind::BlockedBy => (
                    format!("{}/{relation_path}/{}", self.issue_url(number), target.id),
                    None,
                ),
            };
            self.request(Method::DELETE, url, body)
                .await
                .map_err(|error| match error {
                    IssueError::NotFound(_) => IssueError::NotFound(number),
                    other => other,
                })?;
        }
        self.get(number).await
    }
}

/// `url` asking for GitHub's maximum page size.
fn with_page_size(url: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}per_page={PAGE_SIZE}")
}

/// The `rel="next"` target of an RFC 8288 `Link` header value.
fn next_page_url(link: &str) -> Option<String> {
    link.split('<').skip(1).find_map(|entry| {
        let (target, params) = entry.split_once('>')?;
        params.contains("rel=\"next\"").then(|| target.to_string())
    })
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
    id: i64,
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
    use crate::issue::{IssueKind, IssueListState};

    #[derive(Debug, Clone)]
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
        mock_paged_api(move |method, path, body| {
            let (status, json) = handler(method, path, body);
            (status, json, None)
        })
        .await
    }

    /// Like [`mock_api`]; the handler may also name the path of the next
    /// page, sent as a `Link: <…>; rel="next"` header.
    async fn mock_paged_api(
        handler: impl Fn(&str, &str, &str) -> (u16, serde_json::Value, Option<String>)
        + Send
        + Sync
        + 'static,
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
                        let (status, json, next) = handler(&method, &path, &body);
                        let mut response = Response::builder()
                            .status(status)
                            .header("content-type", "application/json");
                        if let Some(next) = next {
                            response = response
                                .header("link", format!("<http://{addr}{next}>; rel=\"next\""));
                        }
                        Ok::<_, Infallible>(
                            response
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
    async fn a_subagent_reads_github_issues_and_sends_no_write_request() {
        use crate::agent::issue_tool::{SubagentIssueTool, subagent_checks};

        let (api_base, requests) = mock_api(|method, path, _body| match (method, path) {
            ("GET", "/repos/o/r/issues/1") => (200, gh_issue_json(1, "seed", "open")),
            ("GET", path) if path.starts_with("/repos/o/r/issues?") => {
                (200, serde_json::json!([gh_issue_json(1, "seed", "open")]))
            }
            (
                "GET",
                "/repos/o/r/issues/1/comments?per_page=100"
                | "/repos/o/r/issues/1/sub_issues?per_page=100"
                | "/repos/o/r/issues/1/dependencies/blocked_by?per_page=100",
            ) => (200, serde_json::json!([])),
            _ => (500, serde_json::json!({"message": "unexpected request"})),
        })
        .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::db::ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("chat db should open");
        let tool = SubagentIssueTool::new(
            store(&api_base),
            crate::issue::provenance::IssueProvenance::new(db, "test".to_string()),
        );

        subagent_checks::assert_reads_work(&tool, 1).await;
        let reads = requests.lock().expect("requests").len();
        assert!(reads > 0, "the reads reached the API");
        subagent_checks::assert_writes_refused(&tool).await;

        let sent = requests.lock().expect("requests").clone();
        assert_eq!(sent.len(), reads, "refused writes sent no request");
        assert!(sent.iter().all(|(method, ..)| method == "GET"));
    }

    /// `--repo OWNER/REPO` reads another GitHub repository's issues while the
    /// session's own backend (a may store) is untouched, and a write ignores it.
    #[tokio::test]
    async fn view_and_list_read_another_github_repository_through_repo() {
        use crate::agent::issue_tool::{IssueArgs, IssueTool};
        use crate::issue::target::IssueTargets;
        use aither_core::llm::{Tool, ToolContext};

        let (api_base, requests) = mock_api(|method, path, _body| match (method, path) {
            ("GET", "/repos/x/y/issues/7") => (200, gh_issue_json(7, "elsewhere", "open")),
            ("GET", path) if path.starts_with("/repos/x/y/issues?") => (
                200,
                serde_json::json!([gh_issue_json(7, "elsewhere", "open")]),
            ),
            (
                "GET",
                "/repos/x/y/issues/7/comments?per_page=100"
                | "/repos/x/y/issues/7/sub_issues?per_page=100"
                | "/repos/x/y/issues/7/dependencies/blocked_by?per_page=100",
            ) => (200, serde_json::json!([])),
            _ => (500, serde_json::json!({"message": "unexpected request"})),
        })
        .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::db::ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("chat db should open");
        let session_store = crate::issue::MayIssueStore::new(db.clone(), "session".to_string());
        session_store
            .create("local only", "", &[])
            .await
            .expect("local issue");
        let targets = IssueTargets::new(
            db.clone(),
            dir.path().to_path_buf(),
            dir.path().join("workspace.toml"),
            GitHubRest::with_api_base(StaticToken, &api_base),
        );
        let tool = IssueTool::new(
            session_store,
            crate::issue::provenance::IssueProvenance::new(db, "test".to_string()),
        )
        .with_targets(targets);

        let viewed = tool
            .call(
                IssueArgs::View {
                    number: 7,
                    repo: Some("x/y".to_string()),
                },
                ToolContext::new(),
            )
            .await
            .expect("view another repository's issue");
        assert!(format!("{viewed:?}").contains("elsewhere"), "{viewed:?}");
        let listed = tool
            .call(
                IssueArgs::List {
                    state: IssueListState::Open,
                    labels: Vec::new(),
                    limit: 10,
                    repo: Some("x/y".to_string()),
                },
                ToolContext::new(),
            )
            .await
            .expect("list another repository's issues");
        let listed = format!("{listed:?}");
        assert!(listed.contains("elsewhere") && !listed.contains("local only"));
        let local = tool
            .call(
                IssueArgs::List {
                    state: IssueListState::Open,
                    labels: Vec::new(),
                    limit: 10,
                    repo: None,
                },
                ToolContext::new(),
            )
            .await
            .expect("list the session's issues");
        let local = format!("{local:?}");
        assert!(local.contains("local only") && !local.contains("elsewhere"));
        let sent = requests.lock().expect("requests").clone();
        assert!(sent.iter().all(|(method, ..)| method == "GET"));
        assert!(
            sent.iter()
                .all(|(_, path, ..)| path.starts_with("/repos/x/y/"))
        );

        let error = tool
            .call(
                IssueArgs::View {
                    number: 7,
                    repo: Some("not a repository".to_string()),
                },
                ToolContext::new(),
            )
            .await
            .expect_err("a malformed target is refused");
        assert!(error.to_string().contains("--repo"), "{error}");
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
                "/repos/o/r/issues/1/comments?per_page=100"
                | "/repos/o/r/issues/1/sub_issues?per_page=100"
                | "/repos/o/r/issues/1/dependencies/blocked_by?per_page=100",
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
    async fn changes_to_github_issues_are_recorded_with_their_session_agent_and_call() {
        #[derive(Default)]
        struct Remote {
            title: String,
            state: String,
            comments: Vec<serde_json::Value>,
        }
        let remote = Arc::new(Mutex::new(Remote::default()));
        let handler_remote = remote.clone();
        let (api_base, _requests) = mock_api(move |method, path, body| {
            let mut remote = handler_remote.lock().expect("remote");
            let issue = |remote: &Remote| gh_issue_json(2, &remote.title, &remote.state);
            match (method, path) {
                ("POST", "/repos/o/r/issues") => {
                    remote.title = "first".to_string();
                    remote.state = "open".to_string();
                    (201, issue(&remote))
                }
                ("PATCH", "/repos/o/r/issues/2") => {
                    let patch: serde_json::Value = serde_json::from_str(body).expect("patch");
                    if let Some(title) = patch["title"].as_str() {
                        remote.title = title.to_string();
                    }
                    if let Some(state) = patch["state"].as_str() {
                        remote.state = state.to_string();
                    }
                    (200, issue(&remote))
                }
                ("GET", "/repos/o/r/issues/2") => (200, issue(&remote)),
                ("GET", "/repos/o/r/issues/1") => (200, gh_issue_json(1, "outside", "open")),
                ("POST", "/repos/o/r/issues/2/comments") => {
                    let comment = serde_json::json!({
                        "id": 7001,
                        "body": "hello",
                        "created_at": "2026-10-03T00:00:00Z",
                        "user": null,
                    });
                    remote.comments.push(comment.clone());
                    (201, comment)
                }
                ("GET", "/repos/o/r/issues/2/comments?per_page=100") => {
                    (200, serde_json::Value::Array(remote.comments.clone()))
                }
                (
                    "GET",
                    "/repos/o/r/issues/1/comments?per_page=100"
                    | "/repos/o/r/issues/1/sub_issues?per_page=100"
                    | "/repos/o/r/issues/1/dependencies/blocked_by?per_page=100"
                    | "/repos/o/r/issues/2/sub_issues?per_page=100"
                    | "/repos/o/r/issues/2/dependencies/blocked_by?per_page=100",
                ) => (200, serde_json::json!([])),
                _ => (500, serde_json::json!({"message": "unexpected request"})),
            }
        })
        .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::db::ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("chat db should open");
        crate::agent::issue_tool::provenance_checks::assert_changes_are_recorded(
            &db,
            store(&api_base),
            2,
            1,
        )
        .await;
    }

    #[tokio::test]
    async fn removing_a_sub_issue_uses_githubs_sub_issue_endpoint() {
        let (api_base, requests) = mock_api(|method, path, body| match (method, path) {
            ("GET", "/repos/o/r/issues/1") => (200, gh_issue_json(1, "parent", "open")),
            ("GET", "/repos/o/r/issues/2") => (200, gh_issue_json(2, "child", "open")),
            (
                "GET",
                "/repos/o/r/issues/1/comments?per_page=100"
                | "/repos/o/r/issues/1/sub_issues?per_page=100"
                | "/repos/o/r/issues/1/dependencies/blocked_by?per_page=100",
            ) => (200, serde_json::json!([])),
            ("DELETE", "/repos/o/r/issues/1/sub_issue") => {
                assert!(body.contains("\"sub_issue_id\":200"), "{body}");
                (200, gh_issue_json(1, "parent", "open"))
            }
            _ => (404, serde_json::json!({"message": "Not Found"})),
        })
        .await;

        store(&api_base)
            .edit_relation(1, IssueRelationKind::SubIssue, 2, false)
            .await
            .expect("sub-issue removal");

        let sent = requests.lock().expect("requests").clone();
        assert!(
            sent.iter()
                .any(|(method, path, ..)| method == "DELETE"
                    && path == "/repos/o/r/issues/1/sub_issue"),
            "{sent:?}"
        );
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

    fn issue_at(number: i64, updated_at: &str) -> serde_json::Value {
        let mut issue = gh_issue_json(number, &format!("issue {number}"), "open");
        issue["updated_at"] = serde_json::json!(updated_at);
        issue
    }

    fn pull_request_at(number: i64, updated_at: &str) -> serde_json::Value {
        let mut pr = issue_at(number, updated_at);
        pr["pull_request"] = serde_json::json!({"url": "https://example/pr"});
        pr
    }

    #[tokio::test]
    async fn comments_are_read_across_pages() {
        let comment = |body: &str| {
            serde_json::json!({
                "id": 1,
                "user": {"login": "someone"},
                "body": body,
                "created_at": "2026-10-01T00:00:00Z",
            })
        };
        let (api_base, requests) =
            mock_paged_api(move |method, path, _body| match (method, path) {
                ("GET", "/repos/o/r/issues/1") => (200, gh_issue_json(1, "seed", "open"), None),
                ("GET", "/repos/o/r/issues/1/comments?per_page=100") => (
                    200,
                    serde_json::json!([comment("first"), comment("second")]),
                    Some("/comments-2".to_string()),
                ),
                ("GET", "/comments-2") => (200, serde_json::json!([comment("third")]), None),
                (
                    "GET",
                    "/repos/o/r/issues/1/sub_issues?per_page=100"
                    | "/repos/o/r/issues/1/dependencies/blocked_by?per_page=100",
                ) => (200, serde_json::json!([]), None),
                _ => panic!("unexpected request {method} {path}"),
            })
            .await;

        let detail = store(&api_base).get(1).await.expect("get");

        let bodies: Vec<_> = detail.comments.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, ["first", "second", "third"]);
        let paths: Vec<_> = requests
            .lock()
            .expect("requests")
            .iter()
            .map(|(_, path, ..)| path.clone())
            .collect();
        assert!(paths.contains(&"/comments-2".to_string()), "{paths:?}");
    }

    /// Pages of `/issues` as the sync sees them: a page of only pull
    /// requests, then two issues sharing an update time, then a later one.
    fn sync_pages(path: &str) -> (u16, serde_json::Value, Option<String>) {
        const T1: &str = "2026-10-01T00:00:00Z";
        const T2: &str = "2026-10-02T00:00:00Z";
        const T3: &str = "2026-10-03T00:00:00Z";
        if path.starts_with("/repos/o/r/issues?") {
            (
                200,
                serde_json::json!([pull_request_at(10, T1), pull_request_at(11, T1)]),
                Some("/page-2".to_string()),
            )
        } else if path == "/page-2" {
            (
                200,
                serde_json::json!([issue_at(1, T2), issue_at(2, T2)]),
                Some("/page-3".to_string()),
            )
        } else if path == "/page-3" {
            (200, serde_json::json!([issue_at(3, T3)]), None)
        } else {
            (404, serde_json::json!({"message": "Not Found"}), None)
        }
    }

    #[tokio::test]
    async fn the_sync_listing_follows_pages_past_pull_requests_and_keeps_ties_whole() {
        let (api_base, _requests) = mock_paged_api(|_method, path, _body| sync_pages(path)).await;

        let updated = store(&api_base)
            .updated_since(None, 1)
            .await
            .expect("updated_since");

        // The pull-request-only first page is passed over, and issue 2 comes
        // with issue 1 although the limit of 1 is reached at issue 1.
        let numbers: Vec<_> = updated.iter().map(|issue| issue.number).collect();
        assert_eq!(numbers, [1, 2]);
    }

    #[tokio::test]
    async fn the_search_sync_indexes_issues_behind_a_pull_request_page() {
        use crate::db::ChatDb;
        use crate::issue::search::{IssueSearch, collection_name};
        use crate::repo_search::backend::testing::MemoryCollections;

        let (api_base, _requests) = mock_paged_api(|method, path, _body| {
            if let Some(number) = path
                .strip_prefix("/repos/o/r/issues/")
                .and_then(|rest| rest.parse::<i64>().ok())
            {
                let updated_at = match number {
                    1 | 2 => "2026-10-02T00:00:00Z",
                    _ => "2026-10-03T00:00:00Z",
                };
                return (200, issue_at(number, updated_at), None);
            }
            if path.contains("/comments")
                || path.contains("/sub_issues")
                || path.contains("/blocked_by")
            {
                return (200, serde_json::json!([]), None);
            }
            assert_eq!(method, "GET");
            if path.contains("since=") {
                // Inclusive: the newest issue comes back and is skipped.
                return (
                    200,
                    serde_json::json!([issue_at(3, "2026-10-03T00:00:00Z")]),
                    None,
                );
            }
            sync_pages(path)
        })
        .await;
        let dir = tempfile::tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("open db");
        let store = store(&api_base);
        let search = IssueSearch::new(db.clone(), MemoryCollections::default());

        search.sync(&store).await.expect("sync");

        let collection = collection_name(&store.search_key());
        for number in [1, 2, 3] {
            assert!(
                db.issue_search_doc(&collection, number)
                    .await
                    .expect("doc")
                    .is_some(),
                "issue {number} is indexed"
            );
        }
        assert_eq!(
            db.issue_search_watermark(&collection)
                .await
                .expect("watermark")
                .as_deref(),
            Some("2026-10-03T00:00:00Z")
        );

        search.sync(&store).await.expect("second sync");
    }

    #[test]
    fn the_next_page_is_read_from_the_link_header() {
        let link = "<https://api.github.com/x?labels=a,b&page=2>; rel=\"next\", \
                    <https://api.github.com/x?page=9>; rel=\"last\"";
        assert_eq!(
            next_page_url(link).as_deref(),
            Some("https://api.github.com/x?labels=a,b&page=2")
        );
        assert_eq!(
            next_page_url("<https://api.github.com/x?page=1>; rel=\"prev\""),
            None
        );
    }

    /// A mock API where the authenticated user is `me` and repository `o/r`
    /// has the given owner and admin permission; issue 5 was opened by
    /// `thread_author`.
    async fn ownership_api(
        owner: &'static str,
        admin: bool,
        thread_author: &'static str,
    ) -> String {
        mock_api(move |method, path, _body| match (method, path) {
            ("GET", "/user") => (200, serde_json::json!({"login": "me"})),
            ("GET", "/repos/o/r") => (
                200,
                serde_json::json!({"owner": {"login": owner}, "permissions": {"admin": admin}}),
            ),
            ("GET", "/repos/o/r/issues/5") => {
                let mut issue = gh_issue_json(5, "thread", "open");
                issue["user"] = serde_json::json!({"login": thread_author});
                (200, issue)
            }
            _ => panic!("unexpected request {method} {path}"),
        })
        .await
        .0
    }

    #[tokio::test]
    async fn a_repository_is_owned_by_its_owner_login_or_admin_permission() {
        for (owner, admin, owned) in [
            ("me", false, true),
            ("ME", false, true),
            ("org", true, true),
            ("org", false, false),
        ] {
            let api_base = ownership_api(owner, admin, "x").await;
            assert_eq!(
                store(&api_base).owned_by_user().await.expect("lookup"),
                owned,
                "owner {owner} admin {admin}"
            );
        }
    }

    #[tokio::test]
    async fn a_failed_ownership_lookup_is_an_error_never_owned() {
        let (api_base, _requests) = mock_api(|_method, path, _body| match path {
            "/user" => (200, serde_json::json!({"login": "me"})),
            _ => (500, serde_json::json!({"message": "boom"})),
        })
        .await;
        assert!(store(&api_base).owned_by_user().await.is_err());
        let (api_base, _requests) =
            mock_api(|_method, _path, _body| (404, serde_json::json!({"message": "Not Found"})))
                .await;
        assert!(store(&api_base).owned_by_user().await.is_err());
    }

    #[tokio::test]
    async fn a_thread_is_the_users_when_their_account_opened_it() {
        let api_base = ownership_api("org", false, "me").await;
        assert!(store(&api_base).opened_by_user(5).await.expect("lookup"));
        let api_base = ownership_api("org", false, "someone").await;
        assert!(!store(&api_base).opened_by_user(5).await.expect("lookup"));
    }

    fn gate(timeout: std::time::Duration) -> crate::agent::publish_approval::PublishGate {
        crate::agent::publish_approval::PublishGate::new(
            Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::config::MayConfig::default(),
            )),
            crate::agent::permission::PermissionApprover::new()
                .with_approval_timeout(timeout)
                .for_session("session-1"),
        )
    }

    #[tokio::test]
    async fn the_gate_holds_publishing_on_an_external_repository() {
        let short = std::time::Duration::from_millis(60);
        let external = store(&ownership_api("org", false, "someone").await);
        let gate = gate(short);

        for kind in [IssueKind::Bug, IssueKind::Feature, IssueKind::Performance] {
            let error = gate
                .approve_issue(&external, kind, "t", &[], "b")
                .await
                .expect_err("an unanswered external issue is denied");
            assert!(matches!(error, IssueError::ApprovalDenied(_)), "{error:?}");
        }
        gate.approve_comment(&external, 5, "hi")
            .await
            .expect_err("an unanswered external comment is denied");
        let repository: GitHubRepository = "o/r".parse().expect("repository");
        gate.approve_pull_request(&external.rest, &repository, "t", "b")
            .await
            .expect_err("an unanswered external pull request is denied");
    }

    #[tokio::test]
    async fn the_gate_lets_the_user_comment_on_their_own_thread_of_an_external_repository() {
        let own_thread = store(&ownership_api("org", false, "me").await);
        gate(std::time::Duration::from_secs(30))
            .approve_comment(&own_thread, 5, "hi")
            .await
            .expect("no prompt for a thread the user's account opened");
    }

    #[tokio::test]
    async fn the_gate_asks_nothing_for_an_owned_repository_beyond_the_kind_entry() {
        let owned = store(&ownership_api("me", false, "someone").await);
        let slow = gate(std::time::Duration::from_secs(30));
        let repository: GitHubRepository = "o/r".parse().expect("repository");
        let quick = std::time::Duration::from_millis(500);
        tokio::time::timeout(quick, async {
            slow.approve_issue(&owned, IssueKind::Bug, "t", &[], "b")
                .await
                .expect("bug");
            slow.approve_issue(&owned, IssueKind::Performance, "t", &[], "b")
                .await
                .expect("performance");
            slow.approve_comment(&owned, 5, "hi")
                .await
                .expect("comment");
            slow.approve_pull_request(&owned.rest, &repository, "t", "b")
                .await
                .expect("pull request");
        })
        .await
        .expect("nothing was asked");
        // A feature issue on an owned repository is approved when nobody answers.
        gate(std::time::Duration::from_millis(60))
            .approve_issue(&owned, IssueKind::Feature, "t", &[], "b")
            .await
            .expect("auto approves");
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
