//! What the GitLab CI API reports about a commit's pipelines, read through
//! the GitLab REST API.
//!
//! GitLab has no workflow/run split: a pipeline is the run, so every
//! pipeline reads as one workflow whose jobs the recorder groups under.

use serde::Deserialize;
use zenwave::{Method, ResponseExt as _};

use super::CiError;
use super::github::{Actions, Job, Run};
use may_client::issue::gitlab::{GitLabRest, GitLabTokenSource};
use may_config::forge::ForgeRepo;

/// GitLab's maximum `per_page`.
const PAGE_SIZE: usize = 100;

/// The one workflow id GitLab pipelines read under — GitLab pipelines are
/// not named workflows, so jobs compare across pipelines by name alone.
const PIPELINE_WORKFLOW: u64 = 0;

/// The GitLab CI API of one repository through `rest`'s client, for the
/// merge request `mr_iid`.
#[derive(Debug, Clone)]
pub struct GitLabCi<T> {
    rest: GitLabRest<T>,
    repo: ForgeRepo,
    mr_iid: i64,
}

/// A pipeline as GitLab's API reports it.
#[derive(Debug, Deserialize)]
struct GlPipeline {
    id: u64,
    /// `success`, `failed`, `canceled`, `skipped` or still running.
    status: String,
    /// Pipeline name; often absent.
    name: Option<String>,
    /// What triggered it (`push`, `merge_request_event`, `schedule`, ...).
    #[serde(default)]
    source: String,
}

/// A job as GitLab's API reports it.
#[derive(Debug, Deserialize)]
struct GlJob {
    id: u64,
    name: String,
    status: String,
    started_at: Option<jiff::Timestamp>,
    finished_at: Option<jiff::Timestamp>,
}

/// The MR's target branch.
#[derive(Debug, Deserialize)]
struct GlMergeRequest {
    target_branch: String,
}

/// Whether a GitLab pipeline or job state is finished.
fn finished(status: &str) -> bool {
    matches!(status, "success" | "failed" | "canceled" | "skipped")
}

/// The `ci` run a pipeline reads as: finished pipelines are `completed`.
fn run(pipeline: GlPipeline) -> Run {
    Run {
        id: pipeline.id,
        workflow_id: PIPELINE_WORKFLOW,
        name: Some(pipeline.name.unwrap_or_else(|| "pipeline".to_string())),
        status: if finished(&pipeline.status) {
            "completed".to_string()
        } else {
            pipeline.status
        },
        event: pipeline.source,
    }
}

/// The `ci` job a GitLab job reads as: finished statuses map to a
/// conclusion, running ones leave it `None`.
fn job(job: GlJob) -> Job {
    let conclusion = match job.status.as_str() {
        "success" => Some("success".to_string()),
        "failed" => Some("failure".to_string()),
        "canceled" => Some("cancelled".to_string()),
        "skipped" | "manual" => Some("skipped".to_string()),
        _ => None,
    };
    Job {
        id: job.id,
        name: job.name,
        conclusion,
        started_at: job.started_at,
        completed_at: job.finished_at,
        // GitLab jobs carry no step breakdown.
        steps: Vec::new(),
    }
}

impl<T: GitLabTokenSource> GitLabCi<T> {
    /// The CI API of `repo`'s project, for merge request `mr_iid`.
    #[must_use]
    pub const fn new(rest: GitLabRest<T>, repo: ForgeRepo, mr_iid: i64) -> Self {
        Self { rest, repo, mr_iid }
    }

    fn project(&self) -> String {
        self.repo.gitlab_encoded_path()
    }

    async fn get_json<R: serde::de::DeserializeOwned>(&self, path: String) -> Result<R, CiError> {
        self.rest
            .request(
                Method::GET,
                format!("{}/{path}", self.rest.api_base()),
                None,
            )
            .await
            .map_err(|e| CiError::GitLab(e.to_string()))?
            .into_json::<R>()
            .await
            .map_err(|e| CiError::GitLab(e.to_string()))
    }

    /// Every page of a list endpoint.
    async fn get_list<R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<Vec<R>, CiError> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let mut items = Vec::new();
        let mut page = 1_u32;
        loop {
            let response = self
                .rest
                .request(
                    Method::GET,
                    format!(
                        "{}/{path}{separator}page={page}&per_page={PAGE_SIZE}",
                        self.rest.api_base()
                    ),
                    None,
                )
                .await
                .map_err(|e| CiError::GitLab(e.to_string()))?;
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
                    .map_err(|e| CiError::GitLab(e.to_string()))?,
            );
            match next {
                Some(next) => page = next,
                None => return Ok(items),
            }
        }
    }
}

impl<T: GitLabTokenSource + Sync> Actions for GitLabCi<T> {
    const SOURCE: &'static str = "gitlab";

    async fn base_branch(&self) -> Result<String, CiError> {
        let mr: GlMergeRequest = self
            .get_json(format!(
                "projects/{}/merge_requests/{}",
                self.project(),
                self.mr_iid
            ))
            .await?;
        Ok(mr.target_branch)
    }

    async fn runs_for_sha(&self, sha: &str) -> Result<Vec<Run>, CiError> {
        let pipelines: Vec<GlPipeline> = self
            .get_json(format!(
                "projects/{}/pipelines?sha={sha}&per_page={PAGE_SIZE}",
                self.project()
            ))
            .await?;
        Ok(pipelines.into_iter().map(run).collect())
    }

    async fn jobs(&self, run_id: u64) -> Result<Vec<Job>, CiError> {
        let jobs: Vec<GlJob> = self
            .get_list(&format!(
                "projects/{}/pipelines/{run_id}/jobs",
                self.project()
            ))
            .await?;
        Ok(jobs.into_iter().map(job).collect())
    }

    async fn successful_runs(
        &self,
        _workflow_id: u64,
        branch: &str,
        event: &str,
        count: usize,
    ) -> Result<Vec<Run>, CiError> {
        // GitLab `source` names the trigger; an empty event means the base
        // branch's own pushes, the baseline a PR compares against.
        let source = match event {
            "pull_request" | "merge_group" => "merge_request_event",
            "" => "push",
            other => other,
        };
        let pipelines: Vec<GlPipeline> = self
            .get_json(format!(
                "projects/{}/pipelines?status=success&ref={branch}&source={source}&per_page={count}",
                self.project()
            ))
            .await?;
        Ok(pipelines.into_iter().map(run).collect())
    }

    async fn log(&self, job_id: u64) -> Result<String, CiError> {
        self.rest
            .request(
                Method::GET,
                format!(
                    "{}/projects/{}/jobs/{job_id}/trace",
                    self.rest.api_base(),
                    self.project()
                ),
                None,
            )
            .await
            .map_err(|e| CiError::GitLab(e.to_string()))?
            .into_string()
            .await
            .map_err(|e| CiError::GitLab(e.to_string()))
            .map(|text| text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use may_client::issue::IssueError;
    use may_protocol::test_support::{ApiReply, serve_api};

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

    fn ci(api_base: &str) -> GitLabCi<StaticToken> {
        GitLabCi::new(GitLabRest::with_api_base(StaticToken, api_base), repo(), 7)
    }

    #[tokio::test]
    async fn the_base_branch_is_the_mrs_target() {
        let (api_base, _) = serve_api(|request, _| {
            if request
                .path
                .ends_with("/projects/group%2Fsub%2Frepo/merge_requests/7")
            {
                ApiReply::json(200, r#"{"target_branch": "main"}"#)
            } else {
                ApiReply::json(404, "{}")
            }
        })
        .await;
        assert_eq!(ci(&api_base).base_branch().await.unwrap(), "main");
    }

    #[tokio::test]
    async fn a_sha_finds_its_pipelines_as_runs() {
        let (api_base, _) = serve_api(|request, _| {
            if request.path.contains("/pipelines?sha=head1") {
                ApiReply::json(
                    200,
                    r#"[{"id": 9, "status": "success", "source": "merge_request_event"}]"#,
                )
            } else {
                ApiReply::json(404, "{}")
            }
        })
        .await;
        let runs = ci(&api_base).runs_for_sha("head1").await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, 9);
        assert_eq!(runs[0].event, "merge_request_event");
        assert_eq!(runs[0].status, "completed");
    }

    #[tokio::test]
    async fn jobs_map_statuses_to_conclusions() {
        let (api_base, _) = serve_api(|request, _| {
            if request.path.contains("/pipelines/9/jobs") {
                ApiReply::json(
                    200,
                    r#"[
                        {"id": 1, "name": "test", "status": "success"},
                        {"id": 2, "name": "lint", "status": "running"}
                    ]"#,
                )
            } else {
                ApiReply::json(404, "{}")
            }
        })
        .await;
        let jobs = ci(&api_base).jobs(9).await.unwrap();
        assert_eq!(jobs[0].conclusion.as_deref(), Some("success"));
        assert!(jobs[1].conclusion.is_none());
    }

    #[tokio::test]
    async fn successful_runs_filter_on_the_mapped_source() {
        let (api_base, requests) = serve_api(|request, _| {
            if request.path.contains("/pipelines?status=success") {
                ApiReply::json(200, r#"[{"id": 9, "status": "success", "source": "push"}]"#)
            } else {
                ApiReply::json(404, "{}")
            }
        })
        .await;
        let runs = ci(&api_base)
            .successful_runs(0, "main", "pull_request", 10)
            .await
            .unwrap();
        assert_eq!(runs.len(), 1);
        assert!(
            requests.lock().unwrap()[0]
                .path
                .contains("source=merge_request_event")
        );
    }

    #[tokio::test]
    async fn the_trace_is_the_jobs_log() {
        let (api_base, requests) = serve_api(|request, _| {
            if request.path.ends_with("/jobs/5/trace") {
                ApiReply::json(200, "line one\nline two")
            } else {
                ApiReply::json(404, "{}")
            }
        })
        .await;
        assert_eq!(ci(&api_base).log(5).await.unwrap(), "line one\nline two");
        assert_eq!(requests.lock().unwrap()[0].authorization, "Bearer tok");
    }
}
