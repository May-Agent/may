//! A session's background work as it crosses the socket: terminal jobs and
//! subagent runs.
//!
//! Both live in the daemon's agent; [`WireJob`] and [`WireSubagentRun`]
//! carry them to the TUI and turn back into the registry's own types, so the
//! jobs panel renders the same whether the agent runs in-process or in the
//! daemon. Elapsed time replaces the [`Instant`]s, which mean nothing in
//! another process.

use std::time::{Duration, Instant};

use aither_sandbox::{JobInfo, JobStatus, TerminalMode};
use serde::{Deserialize, Serialize};

use crate::agent::SubagentRunSnapshot;

/// A job's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WireJobStatus {
    /// Still running.
    Running,
    /// Exited with `exit_code`.
    Completed {
        /// Exit code.
        exit_code: i32,
    },
    /// Failed to run.
    Failed {
        /// Why.
        error: String,
    },
    /// Killed by the user.
    Killed,
}

impl From<&JobStatus> for WireJobStatus {
    fn from(status: &JobStatus) -> Self {
        match status {
            JobStatus::Running => Self::Running,
            JobStatus::Completed { exit_code } => Self::Completed {
                exit_code: *exit_code,
            },
            JobStatus::Failed { error } => Self::Failed {
                error: error.clone(),
            },
            JobStatus::Killed => Self::Killed,
        }
    }
}

impl From<WireJobStatus> for JobStatus {
    fn from(status: WireJobStatus) -> Self {
        match status {
            WireJobStatus::Running => Self::Running,
            WireJobStatus::Completed { exit_code } => Self::Completed { exit_code },
            WireJobStatus::Failed { error } => Self::Failed { error },
            WireJobStatus::Killed => Self::Killed,
        }
    }
}

/// One background terminal job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WireJob {
    /// Process id.
    pub pid: u32,
    /// Task id the model sees.
    pub task_id: String,
    /// Execution key that owns the job.
    pub execution_key: String,
    /// The script it runs.
    pub script: String,
    /// Permission mode.
    #[schema(value_type = String)]
    pub mode: TerminalMode,
    /// Milliseconds since it started.
    pub elapsed_ms: u64,
    /// Its state.
    pub status: WireJobStatus,
    /// Its output file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub output_path: Option<std::path::PathBuf>,
}

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The instant `elapsed_ms` ago, clamped to this process's clock.
fn started_at(elapsed_ms: u64) -> Instant {
    let now = Instant::now();
    now.checked_sub(Duration::from_millis(elapsed_ms))
        .unwrap_or(now)
}

impl From<&JobInfo> for WireJob {
    fn from(job: &JobInfo) -> Self {
        Self {
            pid: job.pid,
            task_id: job.task_id.clone(),
            execution_key: job.execution_key.clone(),
            script: job.script.clone(),
            mode: job.mode,
            elapsed_ms: elapsed_ms(job.started_at),
            status: (&job.status).into(),
            output_path: job.output_path.clone(),
        }
    }
}

impl From<WireJob> for JobInfo {
    fn from(job: WireJob) -> Self {
        Self {
            pid: job.pid,
            task_id: job.task_id,
            execution_key: job.execution_key,
            script: job.script,
            mode: job.mode,
            started_at: started_at(job.elapsed_ms),
            status: job.status.into(),
            output_path: job.output_path,
        }
    }
}

/// One subagent run in flight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WireSubagentRun {
    /// Run id within the session.
    pub run_id: u64,
    /// Subagent name or file.
    pub agent: String,
    /// The prompt it was given.
    pub prompt: String,
    /// Milliseconds since it started.
    pub elapsed_ms: u64,
    /// Prompt tokens so far.
    pub prompt_tokens: u64,
    /// Completion tokens so far.
    pub completion_tokens: u64,
    /// Model responses so far.
    pub requests: u64,
}

impl From<&SubagentRunSnapshot> for WireSubagentRun {
    fn from(run: &SubagentRunSnapshot) -> Self {
        Self {
            run_id: run.run_id,
            agent: run.agent.clone(),
            prompt: run.prompt.clone(),
            elapsed_ms: elapsed_ms(run.started_at),
            prompt_tokens: run.prompt_tokens,
            completion_tokens: run.completion_tokens,
            requests: run.requests,
        }
    }
}

impl From<WireSubagentRun> for SubagentRunSnapshot {
    fn from(run: WireSubagentRun) -> Self {
        Self {
            run_id: run.run_id,
            agent: run.agent,
            prompt: run.prompt,
            started_at: started_at(run.elapsed_ms),
            prompt_tokens: run.prompt_tokens,
            completion_tokens: run.completion_tokens,
            requests: run.requests,
        }
    }
}

/// A session's background work: the `session_background` event payload and
/// the answer of `GET /api/sessions/{id}/background`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionBackgroundSnapshot {
    /// The session.
    pub session_id: String,
    /// Its terminal jobs, the agent's current ones and earlier turns' that
    /// still run.
    pub jobs: Vec<WireJob>,
    /// Its subagent runs in flight.
    pub subagents: Vec<WireSubagentRun>,
}

/// Bytes as base64 text, for output that need not be UTF-8.
pub mod base64_bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serializes `bytes` as base64.
    ///
    /// # Errors
    /// Fails when the serializer does.
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    /// Reads base64 back into bytes.
    ///
    /// # Errors
    /// Fails on invalid base64.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

/// Answer of `GET /api/sessions/{id}/jobs/{pid}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WireJobDetail {
    /// The job, while it is known.
    pub job: Option<WireJob>,
    /// Its buffered stdout, while the registry serves it.
    #[serde(default, with = "base64_opt")]
    #[schema(value_type = Option<String>)]
    pub stdout: Option<Vec<u8>>,
    /// Its buffered stderr, while the registry serves it.
    #[serde(default, with = "base64_opt")]
    #[schema(value_type = Option<String>)]
    pub stderr: Option<Vec<u8>>,
}

/// New terminal output of a job: answer of
/// `GET /api/sessions/{id}/jobs/read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WireTerminalDelta {
    /// Cursor for the next read.
    pub cursor: usize,
    /// The bytes since the previous cursor.
    #[serde(with = "base64_bytes")]
    #[schema(value_type = String)]
    pub bytes: Vec<u8>,
    /// The job's state at the read.
    pub status: WireJobStatus,
}

mod base64_opt {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    #[allow(clippy::ref_option)]
    pub fn serialize<S: Serializer>(
        bytes: &Option<Vec<u8>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(bytes) => serializer.serialize_some(&STANDARD.encode(bytes)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<u8>>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|text| STANDARD.decode(text).map_err(serde::de::Error::custom))
            .transpose()
    }
}

/// A session's background work in the daemon, reached over REST.
#[derive(Debug, Clone)]
pub struct RemoteBackground {
    client: super::DaemonClient,
    session_id: String,
}

/// A job and its buffered `(stdout, stderr)`.
pub type JobDetail = (Option<JobInfo>, Option<(Vec<u8>, Vec<u8>)>);

#[derive(Debug, Deserialize)]
struct KillAnswer {
    ok: bool,
}

#[derive(Debug, Deserialize)]
struct CancelAnswer {
    stopped: usize,
}

impl RemoteBackground {
    /// Background work of `session_id` behind `client`.
    #[must_use]
    pub const fn new(client: super::DaemonClient, session_id: String) -> Self {
        Self { client, session_id }
    }

    fn path(&self, tail: &str) -> String {
        super::session_path(&self.session_id, tail)
    }

    /// The session's jobs and subagent runs.
    ///
    /// # Errors
    /// Fails when the daemon is unreachable or refuses.
    pub async fn snapshot(
        &self,
    ) -> Result<(Vec<JobInfo>, Vec<SubagentRunSnapshot>), super::DaemonClientError> {
        let snapshot: SessionBackgroundSnapshot =
            self.client.get(&self.path("/background")).await?;
        Ok(snapshot.into_parts())
    }

    /// Job `pid` and its buffered output.
    ///
    /// # Errors
    /// Fails when the daemon is unreachable or refuses.
    pub async fn job(&self, pid: u32) -> Result<JobDetail, super::DaemonClientError> {
        let detail: WireJobDetail = self.client.get(&self.path(&format!("/jobs/{pid}"))).await?;
        let output = detail.stdout.zip(detail.stderr);
        Ok((detail.job.map(JobInfo::from), output))
    }

    /// Kills job `pid`; `true` when it was running.
    ///
    /// # Errors
    /// Fails when the daemon is unreachable or refuses.
    pub async fn kill(&self, pid: u32) -> Result<bool, super::DaemonClientError> {
        let answer: KillAnswer = self
            .client
            .post(
                &self.path(&format!("/jobs/{pid}/kill")),
                &serde_json::json!({}),
            )
            .await?;
        Ok(answer.ok)
    }

    /// Output of task `task_id` since `cursor`.
    ///
    /// # Errors
    /// Fails when the daemon is unreachable or the task is unknown.
    pub async fn read(
        &self,
        task_id: &str,
        cursor: usize,
    ) -> Result<WireTerminalDelta, super::DaemonClientError> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("taskId", task_id)
            .append_pair("cursor", &cursor.to_string())
            .finish();
        self.client
            .get(&self.path(&format!("/jobs/read?{query}")))
            .await
    }

    /// Stops subagent run `run_id`; the number of runs stopped.
    ///
    /// # Errors
    /// Fails when the daemon is unreachable or refuses.
    pub async fn cancel(&self, run_id: u64) -> Result<usize, super::DaemonClientError> {
        let answer: CancelAnswer = self
            .client
            .delete(&self.path(&format!("/subagents/{run_id}")))
            .await?;
        Ok(answer.stopped)
    }

    /// Stops every subagent run; the number stopped.
    ///
    /// # Errors
    /// Fails when the daemon is unreachable or refuses.
    pub async fn cancel_all(&self) -> Result<usize, super::DaemonClientError> {
        let answer: CancelAnswer = self.client.delete(&self.path("/subagents")).await?;
        Ok(answer.stopped)
    }
}

impl SessionBackgroundSnapshot {
    /// The jobs and runs as the registries' own types.
    #[must_use]
    pub fn into_parts(self) -> (Vec<JobInfo>, Vec<SubagentRunSnapshot>) {
        (
            self.jobs.into_iter().map(JobInfo::from).collect(),
            self.subagents
                .into_iter()
                .map(SubagentRunSnapshot::from)
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_keeps_its_age_across_the_socket() {
        let job = JobInfo {
            pid: 7,
            task_id: "t1".into(),
            execution_key: "k".into(),
            script: "sleep 30".into(),
            mode: TerminalMode::Sandboxed,
            started_at: Instant::now()
                .checked_sub(Duration::from_secs(5))
                .expect("clock"),
            status: JobStatus::Completed { exit_code: 3 },
            output_path: None,
        };
        let wire: WireJob =
            serde_json::from_str(&serde_json::to_string(&WireJob::from(&job)).expect("serializes"))
                .expect("parses");
        let back = JobInfo::from(wire);
        assert_eq!(back.pid, 7);
        assert_eq!(back.status, JobStatus::Completed { exit_code: 3 });
        let age = back.started_at.elapsed().as_secs();
        assert!((5..7).contains(&age), "age {age}");
    }

    #[test]
    fn output_bytes_survive_as_base64() {
        let delta = WireTerminalDelta {
            cursor: 3,
            bytes: vec![0xff, b'a', 0],
            status: WireJobStatus::Running,
        };
        let text = serde_json::to_string(&delta).expect("serializes");
        assert!(text.contains("\"/2EA\""), "{text}");
        assert_eq!(
            serde_json::from_str::<WireTerminalDelta>(&text).expect("parses"),
            delta
        );
        let detail = WireJobDetail {
            job: None,
            stdout: Some(b"out".to_vec()),
            stderr: None,
        };
        let back: WireJobDetail =
            serde_json::from_str(&serde_json::to_string(&detail).expect("serializes"))
                .expect("parses");
        assert_eq!(back, detail);
    }
}
