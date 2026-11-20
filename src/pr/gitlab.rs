//! The GitLab merge-request backend of `pr`: the remote calls open, close
//! and merge an MR, and the [`merge_watch::Upstream`] the watch reads.
//!
//! Everything goes through [`GitLabRest`], the same authenticated client
//! the GitLab issue backend uses; the token only leaves the process as an
//! `Authorization` header.

use serde::Deserialize;
use zenwave::{Method, ResponseExt as _};

use super::merge_watch::{
    CheckConclusion, CheckContext, RunStatus, Snapshot, StatusState, Upstream,
};
use super::{PrError, RemoteBranch};
use crate::forge::ForgeRepo;
use crate::issue::gitlab::{GitLabRest, GitLabTokenSource};

/// GitLab's maximum `per_page`.
const PAGE_SIZE: usize = 100;

/// Successful pipeline runs read to derive a check's expected duration.
const RUNS_FOR_EXPECTED: u32 = 20;

/// One merge request through the GitLab REST API, for the watch.
pub struct GitLabMr<'a, T> {
    /// The REST client of the MR's host.
    pub(crate) rest: &'a GitLabRest<T>,
    /// The project the MR belongs to.
    pub(crate) repo: &'a ForgeRepo,
    /// The MR's `iid` inside the project.
    pub(crate) iid: i64,
}

impl<T: GitLabTokenSource + Sync> GitLabMr<'_, T> {
    fn project(&self) -> String {
        self.repo.gitlab_encoded_path()
    }

    fn mr_url(&self) -> String {
        format!(
            "{}/projects/{}/merge_requests/{}",
            self.rest.api_base(),
            self.project(),
            self.iid
        )
    }

    async fn get<R: serde::de::DeserializeOwned>(&self, url: String) -> Result<R, PrError> {
        self.rest
            .request(Method::GET, url, None)
            .await
            .map_err(|e| PrError::Gl(e.to_string()))?
            .into_json::<R>()
            .await
            .map_err(|e| PrError::Gl(e.to_string()))
    }

    /// Every item of a list endpoint, following `x-next-page` to the end.
    async fn get_list<R: serde::de::DeserializeOwned>(&self, url: &str) -> Result<Vec<R>, PrError> {
        let separator = if url.contains('?') { '&' } else { '?' };
        let mut items = Vec::new();
        let mut page = 1_u32;
        loop {
            let response = self
                .rest
                .request(
                    Method::GET,
                    format!("{url}{separator}page={page}&per_page={PAGE_SIZE}"),
                    None,
                )
                .await
                .map_err(|e| PrError::Gl(e.to_string()))?;
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
                    .map_err(|e| PrError::Gl(e.to_string()))?,
            );
            match next {
                Some(next) => page = next,
                None => return Ok(items),
            }
        }
    }
}

/// The MR fields the watch decides on.
#[derive(Debug, Deserialize)]
struct GlMergeRequest {
    /// `opened`, `closed`, `merged` or `locked` (mid-merge).
    state: String,
    /// The source branch's head commit.
    sha: Option<String>,
    /// The newest pipeline of the source branch, when one exists.
    head_pipeline: Option<GlPipelineRef>,
    /// Whether every blocking discussion is resolved.
    blocking_discussions_resolved: Option<bool>,
    /// Whether the MR waits on its pipeline to merge.
    #[serde(default)]
    merge_when_pipeline_succeeds: bool,
    /// Whether an auto-merge (merge train included) is armed.
    #[serde(default)]
    auto_merge_enabled: bool,
}

#[derive(Debug, Deserialize)]
struct GlPipelineRef {
    id: i64,
    /// The commit the pipeline runs on.
    sha: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GlApprovals {
    approvals_left: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct GlJob {
    name: String,
    /// `created`, `pending`, `running`, `success`, `failed`, `canceled`,
    /// `skipped`, `manual` and friends.
    status: String,
    /// A job allowed to fail is not a required check.
    #[serde(default)]
    allow_failure: bool,
    web_url: Option<String>,
    started_at: Option<jiff::Timestamp>,
    finished_at: Option<jiff::Timestamp>,
}

#[derive(Debug, Deserialize)]
struct GlPipeline {
    created_at: jiff::Timestamp,
    updated_at: jiff::Timestamp,
}

/// The GitLab job states GitLab reports as finished.
fn job_check(job: GlJob) -> CheckContext {
    let (status, conclusion) = match job.status.as_str() {
        "success" => (RunStatus::Completed, Some(CheckConclusion::Success)),
        "failed" => (RunStatus::Completed, Some(CheckConclusion::Failure)),
        "canceled" => (RunStatus::Completed, Some(CheckConclusion::Cancelled)),
        // Skipped and manual jobs do not hold a merge back.
        "skipped" | "manual" => (RunStatus::Completed, Some(CheckConclusion::Other)),
        "running" => (RunStatus::InProgress, None),
        _ => (RunStatus::Queued, None),
    };
    CheckContext::CheckRun {
        name: job.name,
        details_url: job.web_url,
        status: Some(status),
        started_at: job.started_at,
        completed_at: job.finished_at,
        conclusion,
        is_required: !job.allow_failure,
    }
}

/// A pending required status the MR itself reports (approvals and
/// unresolved blocking discussions read like checks that never conclude).
fn pending_status(context: &str) -> CheckContext {
    CheckContext::StatusContext {
        context: context.to_string(),
        target_url: None,
        state: StatusState::Pending,
        created_at: None,
        is_required: true,
    }
}

impl<T: GitLabTokenSource + Sync> Upstream for GitLabMr<'_, T> {
    async fn snapshot(&self) -> Result<Snapshot, PrError> {
        let mr: GlMergeRequest = self.get(self.mr_url()).await?;
        let state = match mr.state.as_str() {
            "merged" => super::merge_watch::PullRequestState::Merged,
            "closed" => super::merge_watch::PullRequestState::Closed,
            // `locked` is the transient state while GitLab merges.
            _ => super::merge_watch::PullRequestState::Open,
        };
        let head_sha = mr.sha.unwrap_or_default();
        let mut checks = Vec::new();
        if let Some(pipeline) = mr
            .head_pipeline
            .filter(|pipeline| pipeline.sha.as_deref().is_none_or(|sha| sha == head_sha))
        {
            // The head pipeline belongs to the reported head commit.
            let jobs: Vec<GlJob> = self
                .get_list(&format!(
                    "{}/projects/{}/pipelines/{}/jobs",
                    self.rest.api_base(),
                    self.project(),
                    pipeline.id
                ))
                .await?;
            checks.extend(jobs.into_iter().map(job_check));
        }
        if mr.blocking_discussions_resolved == Some(false) {
            checks.push(pending_status("unresolved discussions"));
        }
        let approvals: GlApprovals = self
            .get(format!("{}/approvals", self.mr_url()))
            .await
            .unwrap_or(GlApprovals {
                approvals_left: Some(0),
            });
        if approvals.approvals_left.is_some_and(|left| left > 0) {
            checks.push(pending_status("required approvals"));
        }
        // GitLab has no merge-queue removal events; an armed auto-merge
        // reads as queue position one so the bound stretches for it.
        let queue_position =
            (mr.merge_when_pipeline_succeeds || mr.auto_merge_enabled).then_some(1);
        Ok(Snapshot {
            state,
            queue_position,
            removals: 0,
            last_removal: None,
            head_sha,
            checks,
        })
    }

    async fn expected_checks(&self) -> Result<std::time::Duration, PrError> {
        let pipelines: Vec<GlPipeline> = self
            .get(format!(
                "{}/projects/{}/pipelines?status=success&per_page={RUNS_FOR_EXPECTED}",
                self.rest.api_base(),
                self.project(),
            ))
            .await?;
        pipelines
            .iter()
            .filter_map(|pipeline| {
                std::time::Duration::try_from(
                    pipeline.updated_at.duration_since(pipeline.created_at),
                )
                .ok()
                .filter(|duration| !duration.is_zero())
            })
            .max()
            .ok_or_else(|| {
                PrError::State(format!(
                    "no successful pipeline of {} yet, so there is no way to tell how \
                     long MR !{}'s checks take",
                    self.repo.host_and_path(),
                    self.iid
                ))
            })
    }
}

/// The `default_branch` of `repo`'s project.
#[derive(Debug, Deserialize)]
struct GlProjectRef {
    default_branch: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GlCreatedMr {
    iid: i64,
}

/// Open the merge request of `branch` on `repo` over the REST API: the
/// project's `default_branch` is the target. Returns its `iid`.
///
/// # Errors
/// Fails when the project or the MR cannot be read or created.
pub async fn create_mr<T: GitLabTokenSource + Sync>(
    rest: &GitLabRest<T>,
    repo: &ForgeRepo,
    branch: &str,
    title: &str,
    body: &str,
) -> Result<i64, PrError> {
    let project = repo.gitlab_encoded_path();
    let project_ref: GlProjectRef = rest
        .request(
            Method::GET,
            format!("{}/projects/{project}", rest.api_base()),
            None,
        )
        .await
        .map_err(|e| PrError::Gl(e.to_string()))?
        .into_json()
        .await
        .map_err(|e| PrError::Gl(e.to_string()))?;
    let target = project_ref
        .default_branch
        .ok_or_else(|| PrError::Gl(format!("{} names no default branch", repo.host_and_path())))?;
    let created: GlCreatedMr = rest
        .request(
            Method::POST,
            format!("{}/projects/{project}/merge_requests", rest.api_base()),
            Some(serde_json::json!({
                "source_branch": branch,
                "target_branch": target,
                "title": title,
                "description": body,
                "remove_source_branch": true,
            })),
        )
        .await
        .map_err(|e| PrError::Gl(e.to_string()))?
        .into_json()
        .await
        .map_err(|e| PrError::Gl(e.to_string()))?;
    Ok(created.iid)
}

/// Close the merge request `iid` without merging.
///
/// # Errors
/// Fails when the MR does not exist or GitLab refuses the close.
pub async fn close_mr<T: GitLabTokenSource + Sync>(
    rest: &GitLabRest<T>,
    repo: &ForgeRepo,
    iid: i64,
) -> Result<(), PrError> {
    rest.request(
        Method::PUT,
        format!(
            "{}/projects/{}/merge_requests/{iid}",
            rest.api_base(),
            repo.gitlab_encoded_path()
        ),
        Some(serde_json::json!({ "state_event": "close" })),
    )
    .await
    .map_err(|error| match error {
        crate::issue::IssueError::NotFound(_) => PrError::Gl(format!(
            "close MR !{iid}: not found on {}",
            repo.host_and_path()
        )),
        other => PrError::Gl(format!("close MR !{iid}: {other}")),
    })?;
    Ok(())
}

/// Queue `iid` for GitLab to merge — the pinned squash merge with the
/// source branch removed. `pipeline_running` arms
/// `merge_when_pipeline_succeeds` so GitLab lands the MR when the pipeline
/// finishes; with the pipeline already done or absent the merge runs at
/// once. `auto_merge` covers projects whose merge train owns the landing.
///
/// # Errors
/// A 409 answers GitLab holding a different head than `head_sha` — the
/// head-moved refusal [`super::ensure_head_pinned`] states. Other failures
/// are [`PrError::Gl`].
pub async fn queue_merge<T: GitLabTokenSource + Sync>(
    rest: &GitLabRest<T>,
    repo: &ForgeRepo,
    iid: i64,
    head_sha: &str,
    pipeline_running: bool,
) -> Result<(), PrError> {
    let response = rest
        .request_unchecked(
            Method::PUT,
            format!(
                "{}/projects/{}/merge_requests/{iid}/merge",
                rest.api_base(),
                repo.gitlab_encoded_path()
            ),
            Some(serde_json::json!({
                "sha": head_sha,
                "squash": true,
                "should_remove_source_branch": true,
                "auto_merge": true,
                "merge_when_pipeline_succeeds": pipeline_running,
            })),
        )
        .await
        .map_err(|e| PrError::Gl(e.to_string()))?;
    let status = response.status().as_u16();
    match status {
        _ if response.status().is_success() => Ok(()),
        // GitLab refuses the merge when the expected `sha` is not the MR's
        // head: the pinned commit is not what would land.
        409 => Err(PrError::State(format!(
            "MR !{iid}'s head moved: the pinned commit is {head_sha} — \
             run `pr update {iid}` to pin the current head and check it again"
        ))),
        _ => {
            let message = response.into_string().await.map_or_else(
                |_| "<unreadable error body>".to_string(),
                |text| text.to_string(),
            );
            Err(PrError::Gl(format!(
                "{status} merging MR !{iid}: {message}"
            )))
        }
    }
}

/// Delete the merged MR's `may/*` branch on GitLab. A branch already gone —
/// `remove_source_branch` deletes it on merge — counts as deleted; any
/// other failure is reported, since the merge itself has landed.
pub async fn delete_branch<T: GitLabTokenSource + Sync>(
    rest: &GitLabRest<T>,
    repo: &ForgeRepo,
    branch: &str,
) -> RemoteBranch {
    if !branch.starts_with("may/") {
        return RemoteBranch::NotDeleted(
            branch.to_string(),
            "only may/* branches are the session's to delete".to_string(),
        );
    }
    let encoded = crate::forge::url_path_encode(branch);
    match rest
        .request_unchecked(
            Method::DELETE,
            format!(
                "{}/projects/{}/repository/branches/{encoded}",
                rest.api_base(),
                repo.gitlab_encoded_path()
            ),
            None,
        )
        .await
    {
        Ok(response) if response.status().is_success() => RemoteBranch::Deleted(branch.to_string()),
        Ok(response) if response.status().as_u16() == 404 => {
            RemoteBranch::Deleted(branch.to_string())
        }
        Ok(response) => RemoteBranch::NotDeleted(
            branch.to_string(),
            format!("GitLab answered {}", response.status()),
        ),
        Err(error) => RemoteBranch::NotDeleted(branch.to_string(), error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issue::IssueError;
    use crate::test_support::{ApiReply, serve_api};

    #[derive(Debug)]
    struct StaticToken;

    impl GitLabTokenSource for StaticToken {
        fn token(&self) -> impl Future<Output = Result<String, IssueError>> + Send {
            std::future::ready(Ok("tok".to_string()))
        }
    }

    fn repo() -> ForgeRepo {
        ForgeRepo::parse_binding("gitlab:gitlab.com/group/sub/repo").expect("repository")
    }

    #[tokio::test]
    async fn create_posts_an_mr_to_the_encoded_project() {
        let (api_base, requests) = serve_api(|request, _| {
            let path = request.path.as_str();
            if request.method == "GET" && path.ends_with("/projects/group%2Fsub%2Frepo") {
                ApiReply::json(200, r#"{"default_branch": "main"}"#)
            } else if request.method == "POST" && path.ends_with("/merge_requests") {
                ApiReply::json(201, r#"{"iid": 42}"#)
            } else {
                ApiReply::json(404, "{}")
            }
        })
        .await;
        let rest = GitLabRest::with_api_base(StaticToken, &api_base);
        let iid = create_mr(&rest, &repo(), "may/s1-ab", "title", "body")
            .await
            .unwrap();
        assert_eq!(iid, 42);
        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].authorization.contains("Bearer tok"));
        let post = &requests[1];
        assert_eq!(post.authorization, "Bearer tok");
        let body: serde_json::Value = serde_json::from_str(&post.body).unwrap();
        assert_eq!(body["source_branch"], "may/s1-ab");
        assert_eq!(body["target_branch"], "main");
        assert_eq!(body["remove_source_branch"], true);
    }

    #[tokio::test]
    async fn close_puts_state_event_close() {
        let (api_base, requests) = serve_api(|request, _| {
            if request.method == "PUT"
                && request
                    .path
                    .ends_with("/projects/group%2Fsub%2Frepo/merge_requests/7")
            {
                ApiReply::json(200, r#"{"iid": 7, "state": "closed"}"#)
            } else {
                ApiReply::json(404, "{}")
            }
        })
        .await;
        let rest = GitLabRest::with_api_base(StaticToken, &api_base);
        close_mr(&rest, &repo(), 7).await.unwrap();
        let requests = requests.lock().unwrap().clone();
        let body: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
        assert_eq!(body["state_event"], "close");
    }

    #[tokio::test]
    async fn queue_merge_sends_the_pinned_squash_merge() {
        let (api_base, requests) = serve_api(|request, _| {
            if request.method == "PUT" && request.path.ends_with("/merge") {
                ApiReply::json(200, r#"{"iid": 7, "state": "merged"}"#)
            } else {
                ApiReply::json(404, "{}")
            }
        })
        .await;
        let rest = GitLabRest::with_api_base(StaticToken, &api_base);
        queue_merge(&rest, &repo(), 7, "head1", true).await.unwrap();
        let requests = requests.lock().unwrap().clone();
        let body: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
        assert_eq!(body["sha"], "head1");
        assert_eq!(body["squash"], true);
        assert_eq!(body["auto_merge"], true);
        assert_eq!(body["merge_when_pipeline_succeeds"], true);
    }

    #[tokio::test]
    async fn a_409_merge_says_the_head_moved() {
        let (api_base, _) = serve_api(|request, _| {
            if request.method == "PUT" && request.path.ends_with("/merge") {
                ApiReply::json(409, r#"{"message": "conflict"}"#)
            } else {
                ApiReply::json(404, "{}")
            }
        })
        .await;
        let rest = GitLabRest::with_api_base(StaticToken, &api_base);
        let error = queue_merge(&rest, &repo(), 7, "head1", false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("head moved"), "{error}");
    }

    #[tokio::test]
    async fn delete_branch_calls_the_branch_endpoint() {
        let (api_base, requests) = serve_api(|request, _| {
            if request.method == "DELETE"
                && request.path.ends_with("/repository/branches/may%2Fs1-ab")
            {
                ApiReply::json(204, "")
            } else {
                ApiReply::json(404, "{}")
            }
        })
        .await;
        let rest = GitLabRest::with_api_base(StaticToken, &api_base);
        assert_eq!(
            delete_branch(&rest, &repo(), "may/s1-ab").await,
            RemoteBranch::Deleted("may/s1-ab".to_string())
        );
        assert!(matches!(
            delete_branch(&rest, &repo(), "main").await,
            RemoteBranch::NotDeleted(_, _)
        ));
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_snapshot_maps_the_pipeline_jobs_to_checks() {
        let (api_base, _) = serve_api(|request, _| {
            let path = request.path.as_str();
            if path.ends_with("/merge_requests/7") {
                ApiReply::json(
                    200,
                    r#"{"state": "opened", "sha": "head1", "head_pipeline": {"id": 9, "sha": "head1"}, "blocking_discussions_resolved": true}"#,
                )
            } else if path.contains("/pipelines/9/jobs") {
                ApiReply::json(
                    200,
                    r#"[
                        {"id": 1, "name": "test", "status": "success", "allow_failure": false, "web_url": "https://gitlab.com/group/sub/repo/-/jobs/1", "started_at": "2024-01-01T00:00:00Z", "finished_at": "2024-01-01T00:01:00Z"},
                        {"id": 2, "name": "lint", "status": "failed", "allow_failure": false, "web_url": "https://gitlab.com/group/sub/repo/-/jobs/2", "started_at": "2024-01-01T00:00:00Z", "finished_at": "2024-01-01T00:02:00Z"},
                        {"id": 3, "name": "deploy", "status": "running", "allow_failure": true, "web_url": null, "started_at": "2024-01-01T00:00:00Z", "finished_at": null}
                    ]"#,
                )
            } else if path.ends_with("/approvals") {
                ApiReply::json(200, r#"{"approvals_left": 0}"#)
            } else {
                ApiReply::json(404, "{}")
            }
        })
        .await;
        let rest = GitLabRest::with_api_base(StaticToken, &api_base);
        let mr = GitLabMr {
            rest: &rest,
            repo: &repo(),
            iid: 7,
        };
        let snapshot = mr.snapshot().await.unwrap();
        assert_eq!(snapshot.head_sha, "head1");
        assert_eq!(snapshot.checks.len(), 3);
        assert!(
            matches!(
                &snapshot.checks[1],
                CheckContext::CheckRun {
                    name,
                    conclusion: Some(CheckConclusion::Failure),
                    is_required: true,
                    ..
                } if name == "lint"
            ),
            "{:?}",
            snapshot.checks
        );
        assert!(snapshot.failed_check().is_some());
    }

    #[tokio::test]
    async fn unresolved_discussions_and_approvals_read_as_pending_checks() {
        let (api_base, _) = serve_api(|request, _| {
            let path = request.path.as_str();
            if path.ends_with("/merge_requests/7") {
                ApiReply::json(
                    200,
                    r#"{"state": "opened", "sha": "head1", "head_pipeline": null, "blocking_discussions_resolved": false}"#,
                )
            } else if path.ends_with("/approvals") {
                ApiReply::json(200, r#"{"approvals_left": 2}"#)
            } else {
                ApiReply::json(404, "{}")
            }
        })
        .await;
        let rest = GitLabRest::with_api_base(StaticToken, &api_base);
        let mr = GitLabMr {
            rest: &rest,
            repo: &repo(),
            iid: 7,
        };
        let snapshot = mr.snapshot().await.unwrap();
        assert_eq!(snapshot.checks.len(), 2);
    }

    #[tokio::test]
    async fn a_pipeline_of_an_old_head_adds_no_checks() {
        let (api_base, _) = serve_api(|request, _| {
            let path = request.path.as_str();
            if path.ends_with("/merge_requests/7") {
                ApiReply::json(
                    200,
                    r#"{"state": "opened", "sha": "head2", "head_pipeline": {"id": 9, "sha": "head1"}, "blocking_discussions_resolved": true}"#,
                )
            } else if path.ends_with("/approvals") {
                ApiReply::json(200, r#"{"approvals_left": 0}"#)
            } else if path.contains("/pipelines/9/jobs") {
                ApiReply::json(200, "[]")
            } else {
                ApiReply::json(404, "{}")
            }
        })
        .await;
        let rest = GitLabRest::with_api_base(StaticToken, &api_base);
        let mr = GitLabMr {
            rest: &rest,
            repo: &repo(),
            iid: 7,
        };
        let snapshot = mr.snapshot().await.unwrap();
        assert_eq!(snapshot.head_sha, "head2");
        assert!(snapshot.checks.is_empty());
    }
}
