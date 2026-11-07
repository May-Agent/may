//! What the GitHub Actions API reports about a commit's workflow runs, read
//! through the `gh` CLI.

use std::future::Future;
use std::time::Duration;

use serde::Deserialize;

use super::CiError;

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
    /// The most recent successful runs of a workflow on `branch`.
    fn successful_runs(
        &self,
        workflow_id: u64,
        branch: &str,
        count: usize,
    ) -> impl Future<Output = Result<Vec<Run>, CiError>> + Send;
    /// The log of one job.
    fn log(&self, job_id: u64) -> impl Future<Output = Result<String, CiError>> + Send;
}

/// The Actions API of one repository through `gh api`.
#[derive(Debug, Clone)]
pub struct GhActions {
    owner_repo: String,
    pr_number: i64,
}

impl GhActions {
    /// The API of `owner_repo`, for GitHub PR `pr_number`.
    #[must_use]
    pub fn new(owner_repo: impl Into<String>, pr_number: i64) -> Self {
        Self {
            owner_repo: owner_repo.into(),
            pr_number,
        }
    }

    async fn get(&self, path: String) -> Result<String, CiError> {
        blocking::unblock(move || crate::pr::gh(&["api", &path]))
            .await
            .map_err(|e| CiError::Github(e.to_string()))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: String) -> Result<T, CiError> {
        let body = self.get(path).await?;
        serde_json::from_str(&body).map_err(|e| CiError::Github(e.to_string()))
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

impl Actions for GhActions {
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
        count: usize,
    ) -> Result<Vec<Run>, CiError> {
        let runs: Runs = self
            .get_json(format!(
                "repos/{}/actions/workflows/{workflow_id}/runs?branch={branch}&status=success&per_page={count}",
                self.owner_repo
            ))
            .await?;
        Ok(runs.workflow_runs)
    }

    async fn log(&self, job_id: u64) -> Result<String, CiError> {
        self.get(format!(
            "repos/{}/actions/jobs/{job_id}/logs",
            self.owner_repo
        ))
        .await
    }
}
