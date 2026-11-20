//! What the GitHub Actions API reports about a commit's workflow runs, read
//! through the GitHub REST API.

use std::future::Future;
use std::time::Duration;

use serde::Deserialize;
use zenwave::{Client as _, Method, ResponseExt as _, header::LOCATION};

use super::CiError;
use crate::issue::github::{GitHubRest, GitHubTokenSource};

/// A workflow run.
#[derive(Debug, Clone, Deserialize)]
pub struct Run {
    /// Run id.
    pub id: u64,
    /// The workflow it belongs to.
    pub workflow_id: u64,
    /// The workflow's name.
    pub name: Option<String>,
    /// `completed` once it has finished.
    pub status: String,
    /// Trigger event (`pull_request`, `merge_group`, `push`, etc.).
    #[serde(default)]
    pub event: String,
}

/// One job of a run.
#[derive(Debug, Clone, Deserialize)]
pub struct Job {
    /// Job id.
    pub id: u64,
    /// Job name.
    pub name: String,
    /// How it ended; `None` while it runs.
    pub conclusion: Option<String>,
    /// When it started.
    pub started_at: Option<jiff::Timestamp>,
    /// When it finished.
    pub completed_at: Option<jiff::Timestamp>,
    /// Its steps, in order.
    #[serde(default)]
    pub steps: Vec<Step>,
}

/// One step of a job.
#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    /// Step name.
    pub name: String,
    /// When it started.
    pub started_at: Option<jiff::Timestamp>,
    /// When it finished.
    pub completed_at: Option<jiff::Timestamp>,
}

fn span(start: Option<jiff::Timestamp>, end: Option<jiff::Timestamp>) -> Option<Duration> {
    Duration::try_from(end?.duration_since(start?)).ok()
}

impl Job {
    /// Wall-clock duration of a finished job.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        span(self.started_at, self.completed_at)
    }
}

impl Step {
    /// Wall-clock duration of a finished step.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        span(self.started_at, self.completed_at)
    }
}

/// What the recorder reads from GitHub.
pub trait Actions: Sync {
    /// The base branch of the PR.
    fn base_branch(&self) -> impl Future<Output = Result<String, CiError>> + Send;
    /// Every workflow run on `sha`.
    fn runs_for_sha(&self, sha: &str) -> impl Future<Output = Result<Vec<Run>, CiError>> + Send;
    /// The latest attempt's jobs of a run.
    fn jobs(&self, run_id: u64) -> impl Future<Output = Result<Vec<Job>, CiError>> + Send;
    /// The most recent successful runs of a workflow for `event` (scoped to
    /// `branch` for branch-push events).
    fn successful_runs(
        &self,
        workflow_id: u64,
        branch: &str,
        event: &str,
        count: usize,
    ) -> impl Future<Output = Result<Vec<Run>, CiError>> + Send;
    /// The log of one job.
    fn log(&self, job_id: u64) -> impl Future<Output = Result<String, CiError>> + Send;
}

/// The Actions API of one repository through `api`'s REST client.
#[derive(Debug, Clone)]
pub struct GhActions<T> {
    api: GitHubRest<T>,
    owner_repo: String,
    pr_number: i64,
}

impl<T: GitHubTokenSource> GhActions<T> {
    /// The API of `owner_repo`, for GitHub PR `pr_number`.
    #[must_use]
    pub fn new(api: GitHubRest<T>, owner_repo: impl Into<String>, pr_number: i64) -> Self {
        Self {
            api,
            owner_repo: owner_repo.into(),
            pr_number,
        }
    }

    async fn get(&self, path: String) -> Result<String, CiError> {
        self.api
            .request(Method::GET, format!("{}/{path}", self.api.api_base()), None)
            .await
            .map_err(|e| CiError::Github(e.to_string()))?
            .into_string()
            .await
            .map_err(|e| CiError::Github(e.to_string()))
            .map(|text| text.to_string())
    }

    async fn get_json<R: serde::de::DeserializeOwned>(&self, path: String) -> Result<R, CiError> {
        let body = self.get(path).await?;
        serde_json::from_str(&body).map_err(|e| CiError::Github(e.to_string()))
    }

    /// The job log's text: GitHub answers the logs endpoint with a 302 to a
    /// signed third-party URL. The signed URL embeds its own credential, so
    /// the follow-up request goes out with no `Authorization` header at all
    /// — GitHub's token never leaves for another origin.
    async fn log_text(&self, path: String) -> Result<String, CiError> {
        let response = self
            .api
            .request_unchecked(Method::GET, format!("{}/{path}", self.api.api_base()), None)
            .await
            .map_err(|e| CiError::Github(e.to_string()))?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| CiError::Github("a log redirect named no location".to_string()))?
                .to_string();
            let mut client = zenwave::client().follow_redirect();
            let response = client
                .method(Method::GET, location.as_str())
                .map_err(|e| CiError::Github(e.to_string()))?
                .await
                .map_err(|e| CiError::Github(e.to_string()))?;
            if !response.status().is_success() {
                return Err(CiError::Github(format!(
                    "{} fetching the job log",
                    response.status()
                )));
            }
            return response
                .into_string()
                .await
                .map_err(|e| CiError::Github(e.to_string()))
                .map(|text| text.to_string());
        }
        if !response.status().is_success() {
            return Err(CiError::Github(format!(
                "{} fetching the job log",
                response.status()
            )));
        }
        response
            .into_string()
            .await
            .map_err(|e| CiError::Github(e.to_string()))
            .map(|text| (*text).to_owned())
    }
}

#[derive(Deserialize)]
struct Runs {
    workflow_runs: Vec<Run>,
}

#[derive(Deserialize)]
struct Jobs {
    jobs: Vec<Job>,
}

#[derive(Deserialize)]
struct PullRequest {
    base: Base,
}

#[derive(Deserialize)]
struct Base {
    #[serde(rename = "ref")]
    branch: String,
}

impl<T: GitHubTokenSource + Sync> Actions for GhActions<T> {
    async fn base_branch(&self) -> Result<String, CiError> {
        let pr: PullRequest = self
            .get_json(format!(
                "repos/{}/pulls/{}",
                self.owner_repo, self.pr_number
            ))
            .await?;
        Ok(pr.base.branch)
    }

    async fn runs_for_sha(&self, sha: &str) -> Result<Vec<Run>, CiError> {
        let runs: Runs = self
            .get_json(format!(
                "repos/{}/actions/runs?head_sha={sha}&per_page=100",
                self.owner_repo
            ))
            .await?;
        Ok(runs.workflow_runs)
    }

    async fn jobs(&self, run_id: u64) -> Result<Vec<Job>, CiError> {
        let jobs: Jobs = self
            .get_json(format!(
                "repos/{}/actions/runs/{run_id}/jobs?filter=latest&per_page=100",
                self.owner_repo
            ))
            .await?;
        Ok(jobs.jobs)
    }

    async fn successful_runs(
        &self,
        workflow_id: u64,
        branch: &str,
        event: &str,
        count: usize,
    ) -> Result<Vec<Run>, CiError> {
        let query = match event {
            // `branch=` filters `head_branch`, which is the PR or merge-group
            // ref rather than `branch`; scope PR/merge-queue baselines by
            // `event` so they never mix with `push` runs that skip tests.
            "pull_request" | "merge_group" => format!(
                "repos/{}/actions/workflows/{workflow_id}/runs?status=success&event={event}&per_page={count}",
                self.owner_repo
            ),
            "" => format!(
                "repos/{}/actions/workflows/{workflow_id}/runs?branch={branch}&status=success&per_page={count}",
                self.owner_repo
            ),
            other => format!(
                "repos/{}/actions/workflows/{workflow_id}/runs?branch={branch}&status=success&event={other}&per_page={count}",
                self.owner_repo
            ),
        };
        let runs: Runs = self.get_json(query).await?;
        if event == "merge_group" && runs.workflow_runs.is_empty() {
            let fallback: Runs = self
                .get_json(format!(
                    "repos/{}/actions/workflows/{workflow_id}/runs?status=success&event=pull_request&per_page={count}",
                    self.owner_repo
                ))
                .await?;
            return Ok(fallback.workflow_runs);
        }
        Ok(runs.workflow_runs)
    }

    async fn log(&self, job_id: u64) -> Result<String, CiError> {
        self.log_text(format!(
            "repos/{}/actions/jobs/{job_id}/logs",
            self.owner_repo
        ))
        .await
    }
}
