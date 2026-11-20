//! GitLab issues backend over the REST API.
//!
//! Credentials come from `[forge."<host>"]` configuration — a static token
//! or the `glab` CLI login — resolved by [`crate::forge::ForgeAuth`]. The
//! token only leaves the process as an `Authorization` header — it is never
//! logged or put in model context.

use std::sync::Arc;

use zenwave::header::{ACCEPT, USER_AGENT};
use zenwave::{Client, Method, ResponseExt};

use super::{
    CommentedIssue, IssueBackendInfo, IssueComment, IssueDetail, IssueError, IssueListState,
    IssueRelationKind, IssueState, IssueStore, IssueSummary, RelatedIssue,
};
use crate::forge::ForgeRepo;

/// GitLab's maximum `per_page`.
const PAGE_SIZE: usize = 100;

/// The API base of the GitLab instance at `host` (`https://<host>/api/v4`).
fn api_base_for(host: &str) -> String {
    format!("https://{host}/api/v4")
}

/// Provides a GitLab API token for [`GitLabIssueStore`].
pub trait GitLabTokenSource: Send + Sync + std::fmt::Debug {
    /// Fetch a token; implementations must not log or persist it.
    fn token(&self) -> impl Future<Output = Result<String, IssueError>> + Send;
}

/// Token source reading the `[forge]` credential of `host` through a live
/// [`crate::forge::LiveForgeAuth`], so a `/login` or a Web Settings change
/// applies to a store built before it.
#[derive(Debug, Clone)]
pub struct ForgeGitLabAuth {
    forge: crate::forge::LiveForgeAuth,
    host: String,
}

impl ForgeGitLabAuth {
    /// The credential `forge` resolves for `host`.
    #[must_use]
    pub const fn new(forge: crate::forge::LiveForgeAuth, host: String) -> Self {
        Self { forge, host }
    }
}

impl GitLabTokenSource for ForgeGitLabAuth {
    async fn token(&self) -> Result<String, IssueError> {
        self.forge
            .token_for(&self.host)
            .await
            .map_err(|error| IssueError::GitLabAuth(error.to_string()))
    }
}

/// Authenticated GitLab REST client shared by the issue and merge request
/// backends: one API base, one token source, one request path.
#[derive(Debug, Clone)]
pub struct GitLabRest<T> {
    api_base: String,
    token_source: T,
    /// The token read from `token_source`, shared by clones of this client;
    /// replaced only after GitLab answers 401.
    token: Arc<async_lock::Mutex<Option<String>>>,
}

impl<T: GitLabTokenSource> GitLabRest<T> {
    /// Client targeting `https://<host>/api/v4`.
    #[must_use]
    pub fn new(token_source: T, host: &str) -> Self {
        Self::with_api_base(token_source, &api_base_for(host))
    }

    /// Client targeting a custom API base URL (tests).
    #[must_use]
    pub fn with_api_base(token_source: T, api_base: &str) -> Self {
        Self {
            api_base: api_base.trim_end_matches('/').to_string(),
            token_source,
            token: Arc::new(async_lock::Mutex::new(None)),
        }
    }

    /// The cached token, read from the source on first use or when `renew`
    /// is set (after an authentication failure).
    async fn token(&self, renew: bool) -> Result<String, IssueError> {
        let mut cached = self.token.lock().await;
        if !renew && let Some(token) = cached.as_ref() {
            return Ok(token.clone());
        }
        let token = self.token_source.token().await?;
        *cached = Some(token.clone());
        drop(cached);
        Ok(token)
    }

    /// The API base URL, without a trailing slash.
    #[must_use]
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// Sends an authenticated request; a 404 is [`IssueError::NotFound`]
    /// with the placeholder number `-1` for the caller to replace, any
    /// other non-success status is [`IssueError::GitLab`].
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
            return Err(IssueError::GitLab(format!(
                "{status} from GitLab API: {message}"
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
        let response = self
            .send(
                method.clone(),
                &url,
                body.as_ref(),
                self.token(false).await?,
            )
            .await?;
        if response.status().as_u16() != 401 {
            return Ok(response);
        }
        // The cached token was rejected: read the credential again, once.
        self.send(method, &url, body.as_ref(), self.token(true).await?)
            .await
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<&serde_json::Value>,
        token: String,
    ) -> Result<zenwave::Response, IssueError> {
        let mut client = zenwave::client();
        let mut request = client
            .method(method, url)
            .map_err(|error| IssueError::GitLab(error.to_string()))?
            .bearer_auth(token)
            .header(ACCEPT, "application/json")
            .and_then(|request| request.header(USER_AGENT, "may-agent"))
            .map_err(|error| IssueError::GitLab(error.to_string()))?;
        if let Some(body) = body {
            request = request
                .json_body(body)
                .map_err(|error| IssueError::GitLab(error.to_string()))?;
        }
        match request.await {
            Ok(response) => Ok(response),
            Err(zenwave::Error::Http { response, .. }) => Ok(response.response),
            Err(error) => Err(IssueError::GitLab(error.to_string())),
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
            .into_json::<GlUser>()
            .await
            .map_err(|error| IssueError::GitLab(error.to_string()))?;
        Ok(user.username)
    }

    /// Whether the authenticated user owns `repository`: a personal project
    /// (`namespace.kind == "user"`) whose owner is the user, or a project
    /// or group membership with at least Developer access (`>= 30`).
    ///
    /// # Errors
    /// Fails on token, transport or status errors, a missing repository
    /// included: a failed lookup never counts as owned.
    pub async fn repository_owned(&self, repository: &ForgeRepo) -> Result<bool, IssueError> {
        self.repository_access(repository)
            .await
            .map(|access| access.owned)
    }

    /// The authenticated user's access to `repository`, in the shape
    /// [`crate::issue::github::RepositoryAccess`] gives GitHub's:
    /// ownership or membership with at least Developer access (`>= 30`),
    /// which is also GitLab's push threshold, and whether Issues are
    /// enabled on the project.
    ///
    /// # Errors
    /// Fails on token, transport or status errors, a missing repository
    /// included.
    pub async fn repository_access(
        &self,
        repository: &ForgeRepo,
    ) -> Result<crate::issue::github::RepositoryAccess, IssueError> {
        let url = format!(
            "{}/projects/{}",
            self.api_base,
            repository.gitlab_encoded_path()
        );
        let (viewer, project) = futures_lite::future::try_zip(self.viewer_login(), async {
            self.request(Method::GET, url, None)
                .await?
                .into_json::<GlProject>()
                .await
                .map_err(|error| IssueError::GitLab(error.to_string()))
        })
        .await?;
        let personal = project.namespace.kind == "user"
            && project.namespace.path.eq_ignore_ascii_case(&viewer);
        let member = project.permissions.is_some_and(|permissions| {
            permissions
                .project_access
                .iter()
                .chain(permissions.group_access.iter())
                .filter_map(|access| access.access_level)
                .max()
                .is_some_and(|level| level >= 30)
        });
        let owned = personal || member;
        Ok(crate::issue::github::RepositoryAccess {
            owned,
            can_push: owned,
            has_issues: project.issues_access_level.as_deref() != Some("disabled")
                && project.issues_enabled != Some(false),
        })
    }

    /// Whether the authenticated user's account opened issue `number` of
    /// `repository`. The agent publishes through that account, so what the
    /// agent opened counts too.
    ///
    /// # Errors
    /// Fails on token, transport or status errors.
    pub async fn opened_by_viewer(
        &self,
        repository: &ForgeRepo,
        number: i64,
    ) -> Result<bool, IssueError> {
        let url = format!(
            "{}/projects/{}/issues/{number}",
            self.api_base,
            repository.gitlab_encoded_path()
        );
        let (viewer, issue) = futures_lite::future::try_zip(self.viewer_login(), async {
            self.request(Method::GET, url, None)
                .await
                .map_err(|error| match error {
                    IssueError::NotFound(_) => IssueError::NotFound(number),
                    other => other,
                })?
                .into_json::<GlIssue>()
                .await
                .map_err(|error| IssueError::GitLab(error.to_string()))
        })
        .await?;
        Ok(issue
            .author
            .is_some_and(|author| author.username.eq_ignore_ascii_case(&viewer)))
    }
}

#[derive(Debug, serde::Deserialize)]
struct GlUser {
    username: String,
}

#[derive(Debug, serde::Deserialize)]
struct GlProject {
    namespace: GlNamespace,
    permissions: Option<GlPermissions>,
    /// `enabled`, `private` or `disabled`; Issues exist unless disabled.
    issues_access_level: Option<String>,
    /// The pre-`issues_access_level` flag some GitLab versions still send.
    issues_enabled: Option<bool>,
}

#[derive(Debug, serde::Deserialize)]
struct GlNamespace {
    kind: String,
    path: String,
}

#[derive(Debug, serde::Deserialize)]
struct GlPermissions {
    project_access: Option<GlAccess>,
    group_access: Option<GlAccess>,
}

#[derive(Debug, serde::Deserialize)]
struct GlAccess {
    access_level: Option<i64>,
}

/// GitLab issues backend for one project.
#[derive(Debug)]
pub struct GitLabIssueStore<T> {
    repository: ForgeRepo,
    rest: GitLabRest<T>,
}

impl<T: GitLabTokenSource + Clone> Clone for GitLabIssueStore<T> {
    fn clone(&self) -> Self {
        Self {
            repository: self.repository.clone(),
            rest: self.rest.clone(),
        }
    }
}

impl<T: GitLabTokenSource> GitLabIssueStore<T> {
    /// Store targeting `repository`'s issues on its host's `/api/v4`.
    /// `repository` must be a GitLab project.
    #[must_use]
    pub fn new(repository: ForgeRepo, token_source: T) -> Self {
        let rest = GitLabRest::new(token_source, repository.host().as_str());
        Self::with_rest(repository, rest)
    }

    /// Store targeting a custom API base URL (tests).
    #[must_use]
    pub fn with_api_base(repository: ForgeRepo, token_source: T, api_base: &str) -> Self {
        Self::with_rest(
            repository,
            GitLabRest::with_api_base(token_source, api_base),
        )
    }

    /// Store sharing an existing REST client.
    #[must_use]
    pub const fn with_rest(repository: ForgeRepo, rest: GitLabRest<T>) -> Self {
        Self { repository, rest }
    }

    /// The configured project.
    #[must_use]
    pub const fn repository(&self) -> &ForgeRepo {
        &self.repository
    }

    /// The project's `%2F`-encoded namespace path.
    fn project(&self) -> String {
        self.repository.gitlab_encoded_path()
    }

    fn api_base(&self) -> &str {
        self.rest.api_base()
    }

    fn issue_url(&self, number: i64) -> String {
        format!(
            "{}/projects/{}/issues/{number}",
            self.api_base(),
            self.project()
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

    async fn get_issue(&self, number: i64) -> Result<GlIssue, IssueError> {
        let response = self
            .request(Method::GET, self.issue_url(number), None)
            .await
            .map_err(|error| match error {
                IssueError::NotFound(_) => IssueError::NotFound(number),
                other => other,
            })?;
        response
            .into_json::<GlIssue>()
            .await
            .map_err(|error| IssueError::GitLab(error.to_string()))
    }

    /// Every item of a list endpoint, following `x-next-page` to the end.
    async fn request_json_list<R: serde::de::DeserializeOwned>(
        &self,
        url: &str,
    ) -> Result<Vec<R>, IssueError> {
        let mut items = Vec::new();
        let separator = if url.contains('?') { '&' } else { '?' };
        let mut page = 1_u32;
        loop {
            let paged = format!("{url}{separator}page={page}&per_page={PAGE_SIZE}");
            let response = self.request(Method::GET, paged, None).await?;
            let next = response
                .headers()
                .get("x-next-page")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|next| *next > 0);
            items.extend(
                response
                    .into_json::<Vec<R>>()
                    .await
                    .map_err(|error| IssueError::GitLab(error.to_string()))?,
            );
            match next {
                Some(next) => page = next,
                None => return Ok(items),
            }
        }
    }

    async fn ensure_label(&self, name: &str) -> Result<(), IssueError> {
        let response = self
            .rest
            .request_unchecked(
                Method::POST,
                format!("{}/projects/{}/labels", self.api_base(), self.project()),
                Some(serde_json::json!({"name": name})),
            )
            .await?;
        // 409: the label already exists.
        if response.status().is_success() || response.status().as_u16() == 409 {
            return Ok(());
        }
        Err(IssueError::GitLab(format!(
            "{} creating label {name}",
            response.status()
        )))
    }

    /// The links of issue `number`, newest first.
    async fn links(&self, number: i64) -> Result<Vec<GlLink>, IssueError> {
        self.request_json_list(&format!("{}/links", self.issue_url(number)))
            .await
    }

    async fn detail(&self, issue: GlIssue) -> Result<IssueDetail, IssueError> {
        let number = issue.iid;
        let notes: Vec<GlNote> = self
            .request_json_list(&format!(
                "{}/notes?sort=asc&order_by=created_at",
                self.issue_url(number)
            ))
            .await?;
        let links = self.links(number).await?;
        let mut sub_issues = Vec::new();
        let mut blocked_by = Vec::new();
        for link in links {
            let related = RelatedIssue {
                repository: None,
                number: link.iid,
                title: link.title,
                state: link.state.issue_state(),
            };
            match link.link_type.as_str() {
                "is_blocked_by" => blocked_by.push(related),
                // `blocks` means this issue blocks the target, the inverse of
                // `sub_issues`; `relates_to` fills the sub-issue slot, the
                // loosest association GitLab links express.
                "relates_to" => sub_issues.push(related),
                _ => {}
            }
        }
        Ok(IssueDetail {
            number,
            title: issue.title,
            body: issue.description.unwrap_or_default(),
            state: issue.state.issue_state(),
            labels: issue.labels,
            comments: notes
                .into_iter()
                .filter(|note| !note.system)
                .map(|note| IssueComment {
                    id: note.id,
                    author: note.author.map(|author| author.username),
                    body: note.body,
                    created_at: note.created_at,
                })
                .collect(),
            sub_issues,
            blocked_by,
            created_at: issue.created_at,
            updated_at: issue.updated_at,
        })
    }
}

impl<T: GitLabTokenSource> IssueStore for GitLabIssueStore<T> {
    fn info(&self) -> IssueBackendInfo {
        IssueBackendInfo {
            name: "gitlab",
            repository: Some(self.repository.host_and_path()),
        }
    }

    fn search_key(&self) -> String {
        self.repository.canonical_binding()
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
            "{}/projects/{}/issues?order_by=updated_at&sort=asc&per_page={PAGE_SIZE}",
            self.api_base(),
            self.project(),
        );
        if let Some(since) = since {
            url.push_str("&updated_after=");
            url.push_str(since);
        }
        let limit = usize::try_from(limit).unwrap_or(0);
        let mut summaries = Vec::new();
        let mut separator = '&';
        let mut page = 1_u32;
        loop {
            let response = self
                .request(Method::GET, format!("{url}{separator}page={page}"), None)
                .await?;
            separator = '&';
            let next = response
                .headers()
                .get("x-next-page")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|next| *next > 0);
            let issues = response
                .into_json::<Vec<GlIssue>>()
                .await
                .map_err(|error| IssueError::GitLab(error.to_string()))?;
            let mut newer = 0_usize;
            for issue in issues {
                if let Some(since) = since
                    && issue.updated_at.as_str() < since
                {
                    continue;
                }
                let is_newer = since.is_some_and(|since| issue.updated_at.as_str() > since);
                let summary = IssueSummary {
                    number: issue.iid,
                    title: issue.title,
                    state: issue.state.issue_state(),
                    labels: issue.labels,
                    updated_at: issue.updated_at,
                };
                // The `since` boundary group always goes in whole; after it
                // at least `limit` newer issues, never cutting a group that
                // shares one update time.
                if let Some(last) = summaries
                    .last()
                    .map(|last: &IssueSummary| last.updated_at.clone())
                    && last != summary.updated_at
                    && newer >= limit
                {
                    return Ok(summaries);
                }
                if is_newer {
                    newer += 1;
                }
                summaries.push(summary);
            }
            match next {
                Some(next) => page = next,
                None => return Ok(summaries),
            }
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
        let issue = self
            .request(
                Method::POST,
                format!("{}/projects/{}/issues", self.api_base(), self.project()),
                Some(serde_json::json!({
                    "title": title,
                    "description": body,
                    "labels": labels.join(","),
                })),
            )
            .await?
            .into_json::<GlIssue>()
            .await
            .map_err(|error| IssueError::GitLab(error.to_string()))?;
        self.detail(issue).await
    }

    async fn get(&self, number: i64) -> Result<IssueDetail, IssueError> {
        let issue = self.get_issue(number).await?;
        self.detail(issue).await
    }

    async fn status(&self, number: i64) -> Result<super::IssueStatus, IssueError> {
        let issue = self.get_issue(number).await?;
        let links = self.links(number).await?;
        Ok(super::IssueStatus {
            state: issue.state.issue_state(),
            blocked_by: links
                .into_iter()
                .filter(|link| link.link_type == "is_blocked_by")
                .map(|link| RelatedIssue {
                    repository: None,
                    number: link.iid,
                    title: link.title,
                    state: link.state.issue_state(),
                })
                .collect(),
        })
    }

    async fn list(
        &self,
        state: IssueListState,
        labels: &[String],
        limit: i64,
    ) -> Result<Vec<IssueSummary>, IssueError> {
        let mut url = format!(
            "{}/projects/{}/issues?order_by=updated_at&sort=desc&per_page={PAGE_SIZE}",
            self.api_base(),
            self.project(),
        );
        match state {
            IssueListState::Open => url.push_str("&state=opened"),
            IssueListState::Closed => url.push_str("&state=closed"),
            IssueListState::All => {}
        }
        if !labels.is_empty() {
            url.push_str("&labels=");
            url.push_str(&labels.join(","));
        }
        let mut summaries = Vec::new();
        let want = usize::try_from(limit).unwrap_or(0);
        let mut page = 1_u32;
        while summaries.len() < want {
            let response = self
                .request(Method::GET, format!("{url}&page={page}"), None)
                .await?;
            let next = response
                .headers()
                .get("x-next-page")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|next| *next > 0);
            let issues = response
                .into_json::<Vec<GlIssue>>()
                .await
                .map_err(|error| IssueError::GitLab(error.to_string()))?;
            if issues.is_empty() {
                break;
            }
            summaries.extend(issues.into_iter().map(|issue| IssueSummary {
                number: issue.iid,
                title: issue.title,
                state: issue.state.issue_state(),
                labels: issue.labels,
                updated_at: issue.updated_at,
            }));
            match next {
                Some(next) => page = next,
                None => break,
            }
        }
        summaries.truncate(want);
        Ok(summaries)
    }

    async fn edit(
        &self,
        number: i64,
        title: Option<&str>,
        body: Option<&str>,
        state: Option<IssueState>,
    ) -> Result<IssueDetail, IssueError> {
        let mut fields = serde_json::Map::new();
        if let Some(title) = title {
            fields.insert("title".to_string(), serde_json::json!(title));
        }
        if let Some(body) = body {
            fields.insert("description".to_string(), serde_json::json!(body));
        }
        if let Some(state) = state {
            let event = match state {
                IssueState::Open => "reopen",
                IssueState::Closed => "close",
            };
            fields.insert("state_event".to_string(), serde_json::json!(event));
        }
        if fields.is_empty() {
            return self.get(number).await;
        }
        self.request(
            Method::PUT,
            self.issue_url(number),
            Some(serde_json::Value::Object(fields)),
        )
        .await
        .map_err(|error| match error {
            IssueError::NotFound(_) => IssueError::NotFound(number),
            other => other,
        })?;
        self.get(number).await
    }

    async fn comment(&self, number: i64, body: &str) -> Result<CommentedIssue, IssueError> {
        let note = self
            .request(
                Method::POST,
                format!("{}/notes", self.issue_url(number)),
                Some(serde_json::json!({"body": body})),
            )
            .await
            .map_err(|error| match error {
                IssueError::NotFound(_) => IssueError::NotFound(number),
                other => other,
            })?
            .into_json::<GlNote>()
            .await
            .map_err(|error| IssueError::GitLab(error.to_string()))?;
        Ok(CommentedIssue {
            detail: self.get(number).await?,
            comment_id: note.id,
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
        let mut fields = serde_json::Map::new();
        if !add.is_empty() {
            fields.insert("add_labels".to_string(), serde_json::json!(add.join(",")));
        }
        if !remove.is_empty() {
            fields.insert(
                "remove_labels".to_string(),
                serde_json::json!(remove.join(",")),
            );
        }
        self.request(
            Method::PUT,
            self.issue_url(number),
            Some(serde_json::Value::Object(fields)),
        )
        .await
        .map_err(|error| match error {
            IssueError::NotFound(_) => IssueError::NotFound(number),
            other => other,
        })?;
        self.get(number).await
    }

    async fn edit_relation(
        &self,
        number: i64,
        kind: IssueRelationKind,
        target_number: i64,
        add: bool,
    ) -> Result<IssueDetail, IssueError> {
        let link_type = match kind {
            IssueRelationKind::SubIssue => "relates_to",
            IssueRelationKind::BlockedBy => "is_blocked_by",
        };
        if add {
            self.request(
                Method::POST,
                format!("{}/links", self.issue_url(number)),
                Some(serde_json::json!({
                    "target_project_id": self.project(),
                    "target_issue_iid": target_number,
                    "link_type": link_type,
                })),
            )
            .await
            .map_err(|error| match error {
                IssueError::NotFound(_) => IssueError::NotFound(target_number),
                other => other,
            })?;
        } else {
            let link = self
                .links(number)
                .await?
                .into_iter()
                .find(|link| link.iid == target_number && link.link_type == link_type)
                .ok_or(IssueError::NotFound(target_number))?;
            self.request(
                Method::DELETE,
                format!("{}/links/{}", self.issue_url(number), link.id),
                None,
            )
            .await?;
        }
        self.get(number).await
    }
}

/// GitLab's `opened`/`closed` states.
#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum GlState {
    Opened,
    Closed,
}

impl GlState {
    const fn issue_state(self) -> IssueState {
        match self {
            Self::Opened => IssueState::Open,
            Self::Closed => IssueState::Closed,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct GlIssue {
    iid: i64,
    title: String,
    #[serde(default)]
    description: Option<String>,
    state: GlState,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    author: Option<GlUser>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, serde::Deserialize)]
struct GlNote {
    id: i64,
    body: String,
    #[serde(default)]
    author: Option<GlUser>,
    created_at: String,
    #[serde(default)]
    system: bool,
}

#[derive(Debug, serde::Deserialize)]
struct GlLink {
    /// The link's own id, used to delete it.
    id: i64,
    /// The linked issue's iid.
    iid: i64,
    title: String,
    state: GlState,
    /// `relates_to`, `blocks` or `is_blocked_by`, from this issue's side.
    link_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct StaticToken;

    impl GitLabTokenSource for StaticToken {
        fn token(&self) -> impl Future<Output = Result<String, IssueError>> + Send {
            std::future::ready(Ok("tok".to_string()))
        }
    }

    fn repo() -> ForgeRepo {
        ForgeRepo::parse_binding("gitlab:gitlab.com/acme/app").expect("repository")
    }

    fn store(api_base: &str) -> GitLabIssueStore<StaticToken> {
        GitLabIssueStore::with_api_base(repo(), StaticToken, api_base)
    }

    fn issue_json(iid: i64, title: &str) -> serde_json::Value {
        serde_json::json!({
            "iid": iid,
            "title": title,
            "description": "説明",
            "state": "opened",
            "labels": ["bug"],
            "author": {"username": "ko"},
            "created_at": "2024-01-01T00:00:00.000Z",
            "updated_at": "2024-01-02T00:00:00.000Z",
        })
    }

    #[tokio::test]
    async fn get_reads_issue_notes_and_links() {
        let (api_base, _requests) = crate::test_support::serve_api(move |request, _base| {
            let path = request.path.as_str();
            match request.method.as_str() {
                "GET" if path.ends_with("/issues/7") && !path.contains("/links") => {
                    crate::test_support::ApiReply::json(
                        200,
                        serde_json::to_string(&issue_json(7, "取得")).unwrap(),
                    )
                }
                "GET" if path.contains("/notes") => crate::test_support::ApiReply::json(
                    200,
                    r#"[{"id": 5, "body": "メモ", "author": {"username": "ko"}, "created_at": "2024-01-03T00:00:00.000Z", "system": false}]"#,
                ),
                "GET" if path.contains("/links") => crate::test_support::ApiReply::json(
                    200,
                    r#"[{"id": 9, "iid": 3, "title": "blocker", "state": "opened", "link_type": "is_blocked_by"}]"#,
                ),
                _ => crate::test_support::ApiReply::json(404, "not found"),
            }
        })
        .await;
        let detail = store(&api_base).get(7).await.expect("issue");
        assert_eq!(detail.number, 7);
        assert_eq!(detail.title, "取得");
        assert_eq!(detail.body, "説明");
        assert_eq!(detail.comments.len(), 1);
        assert_eq!(detail.comments[0].author.as_deref(), Some("ko"));
        assert_eq!(detail.blocked_by.len(), 1);
        assert_eq!(detail.blocked_by[0].number, 3);
    }

    #[tokio::test]
    async fn get_encodes_a_nested_project_path() {
        let (api_base, requests) = crate::test_support::serve_api(move |request, _base| {
            let path = request.path.as_str();
            if path.ends_with("/issues/1") && !path.contains("/links") && !path.contains("/notes") {
                crate::test_support::ApiReply::json(
                    200,
                    serde_json::to_string(&issue_json(1, "one")).unwrap(),
                )
            } else {
                crate::test_support::ApiReply::json(200, "[]")
            }
        })
        .await;
        let nested = ForgeRepo::parse_binding("gitlab:gitlab.com/group/sub/repo").expect("repo");
        let store = GitLabIssueStore::with_api_base(nested, StaticToken, &api_base);
        store.get(1).await.expect("issue");
        let encoded = {
            let requests = requests.lock().expect("requests");
            requests
                .iter()
                .any(|request| request.path.contains("group%2Fsub%2Frepo"))
        };
        assert!(encoded, "the project path is encoded");
    }

    #[tokio::test]
    async fn owned_by_user_recognizes_a_personal_project() {
        let (api_base, _requests) = crate::test_support::serve_api(move |request, _base| {
            if request.path == "/user" {
                crate::test_support::ApiReply::json(200, r#"{"username": "may"}"#)
            } else {
                crate::test_support::ApiReply::json(
                    200,
                    r#"{"namespace": {"kind": "user", "path": "may"}, "permissions": {"project_access": null, "group_access": null}}"#,
                )
            }
        })
        .await;
        assert!(store(&api_base).owned_by_user().await.expect("owned"));
    }

    #[tokio::test]
    async fn owned_by_user_recognizes_group_developer_access() {
        let (api_base, _requests) = crate::test_support::serve_api(move |request, _base| {
            if request.path == "/user" {
                crate::test_support::ApiReply::json(200, r#"{"username": "may"}"#)
            } else {
                crate::test_support::ApiReply::json(
                    200,
                    r#"{"namespace": {"kind": "group", "path": "acme"}, "permissions": {"project_access": null, "group_access": {"access_level": 30}}}"#,
                )
            }
        })
        .await;
        assert!(store(&api_base).owned_by_user().await.expect("owned"));
    }

    #[tokio::test]
    async fn owned_by_user_rejects_guest_access() {
        let (api_base, _requests) = crate::test_support::serve_api(move |request, _base| {
            if request.path == "/user" {
                crate::test_support::ApiReply::json(200, r#"{"username": "may"}"#)
            } else {
                crate::test_support::ApiReply::json(
                    200,
                    r#"{"namespace": {"kind": "group", "path": "acme"}, "permissions": {"project_access": {"access_level": 10}, "group_access": null}}"#,
                )
            }
        })
        .await;
        assert!(!store(&api_base).owned_by_user().await.expect("owned"));
    }

    #[tokio::test]
    async fn list_paginates_with_next_page() {
        let (api_base, requests) = crate::test_support::serve_api(move |request, _base| {
            if request.path.contains("&page=1") {
                return crate::test_support::ApiReply::json(
                    200,
                    format!(
                        "[{}]",
                        serde_json::to_string(&issue_json(1, "one")).unwrap()
                    ),
                )
                .header("x-next-page", "2");
            }
            crate::test_support::ApiReply::json(
                200,
                format!(
                    "[{}]",
                    serde_json::to_string(&issue_json(2, "two")).unwrap()
                ),
            )
        })
        .await;
        let listed = store(&api_base)
            .list(IssueListState::Open, &[], 10)
            .await
            .expect("list");
        assert_eq!(listed.len(), 2);
        let filtered = {
            let requests = requests.lock().expect("requests");
            requests
                .iter()
                .any(|request| request.path.contains("state=opened"))
        };
        assert!(filtered);
    }

    #[tokio::test]
    async fn edit_relation_posts_and_deletes_a_link() {
        let (api_base, requests) = crate::test_support::serve_api(move |request, _base| {
            let path = request.path.as_str();
            match request.method.as_str() {
                "POST" if path.contains("/links") => {
                    crate::test_support::ApiReply::json(201, "{}")
                }
                "GET" if path.contains("/links") => crate::test_support::ApiReply::json(
                    200,
                    r#"[{"id": 11, "iid": 2, "title": "t", "state": "opened", "link_type": "is_blocked_by"}]"#,
                ),
                "DELETE" if path.contains("/links/11") => {
                    crate::test_support::ApiReply::json(204, "")
                }
                "GET" if path.ends_with("/issues/1") => crate::test_support::ApiReply::json(
                    200,
                    serde_json::to_string(&issue_json(1, "one")).unwrap(),
                ),
                "GET" => crate::test_support::ApiReply::json(200, "[]"),
                _ => crate::test_support::ApiReply::json(404, "not found"),
            }
        })
        .await;
        let store = store(&api_base);
        store
            .edit_relation(1, IssueRelationKind::BlockedBy, 2, true)
            .await
            .expect("add");
        store
            .edit_relation(1, IssueRelationKind::BlockedBy, 2, false)
            .await
            .expect("remove");
        let (posted, deleted) = {
            let requests = requests.lock().expect("requests");
            (
                requests
                    .iter()
                    .any(|request| request.method == "POST" && request.path.contains("/links")),
                requests.iter().any(|request| {
                    request.method == "DELETE" && request.path.contains("/links/11")
                }),
            )
        };
        assert!(posted);
        assert!(deleted);
    }
}
