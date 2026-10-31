//! Each session's background work: terminal jobs and subagent runs.
//!
//! A turn rebuilds the session's agent, and with it the terminal tool's
//! job registry. [`SessionBackgrounds`] keeps what outlives the turn: the
//! session's subagent handles, which every agent of the session is built
//! with, and the job registries of earlier agents while their jobs still
//! run. Clients list, read and stop this work over REST, and the
//! `session_background` event carries a snapshot whenever it changes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aither_sandbox::{JobRegistry, JobStatus};
use skyzen::Result as SkyResult;
use skyzen::extract::Query;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};

use crate::agent::SubagentHandles;
use crate::client::background::{
    SessionBackgroundSnapshot, WireJob, WireJobDetail, WireSubagentRun, WireTerminalDelta,
};
use crate::ui_event_bus::{UiEventBus, UiEventTopics};
use crate::daemon::error::bad_request_error;
use crate::daemon::state::DaemonState;

/// How often running background work is checked for changes.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
/// Largest terminal read one request returns.
const MAX_READ_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct SessionBackground {
    subagents: SubagentHandles,
    /// Job registries of the session's agents, numbered, newest last.
    jobs: Vec<(u64, JobRegistry)>,
}

/// Background work of every session the daemon ran.
#[derive(Default)]
pub struct SessionBackgrounds {
    sessions: Mutex<HashMap<String, SessionBackground>>,
    next_registry: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for SessionBackgrounds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionBackgrounds").finish_non_exhaustive()
    }
}

impl SessionBackgrounds {
    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionBackground>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The subagent handles `session_id`'s agents are built with.
    pub(crate) fn subagents(&self, session_id: &str) -> SubagentHandles {
        self.sessions()
            .entry(session_id.to_string())
            .or_default()
            .subagents
            .clone()
    }

    /// Records the job registry of an agent just built for `session_id`.
    pub(crate) fn add_job_registry(&self, session_id: &str, registry: JobRegistry) {
        let id = self
            .next_registry
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.sessions()
            .entry(session_id.to_string())
            .or_default()
            .jobs
            .push((id, registry));
    }

    fn registries(&self, session_id: &str) -> Vec<(u64, JobRegistry)> {
        self.sessions()
            .get(session_id)
            .map(|session| session.jobs.clone())
            .unwrap_or_default()
    }

    fn subagents_of(&self, session_id: &str) -> Option<SubagentHandles> {
        self.sessions()
            .get(session_id)
            .map(|session| session.subagents.clone())
    }

    /// Jobs of `session_id`: every job of its newest agent, and the running
    /// ones of earlier agents. Earlier registries with nothing running are
    /// dropped.
    async fn jobs(&self, session_id: &str) -> Vec<aither_sandbox::JobInfo> {
        let registries = self.registries(session_id);
        let Some(((_, newest), older)) = registries.split_last() else {
            return Vec::new();
        };
        let mut jobs = Vec::new();
        let mut idle = Vec::new();
        for (id, registry) in older {
            let running: Vec<_> = registry
                .list()
                .await
                .into_iter()
                .filter(|job| job.status == JobStatus::Running)
                .collect();
            if running.is_empty() {
                idle.push(*id);
            }
            jobs.extend(running);
        }
        jobs.extend(newest.list().await);
        if !idle.is_empty()
            && let Some(session) = self.sessions().get_mut(session_id)
        {
            session.jobs.retain(|(id, _)| !idle.contains(id));
        }
        jobs
    }

    /// The registry that holds job `pid` of `session_id`, newest first.
    async fn registry_with(&self, session_id: &str, pid: u32) -> Option<JobRegistry> {
        for (_, registry) in self.registries(session_id).into_iter().rev() {
            if registry.get(pid).await.is_some() {
                return Some(registry);
            }
        }
        None
    }

    /// The registry that holds task `task_id` of `session_id`, newest first.
    async fn registry_with_task(&self, session_id: &str, task_id: &str) -> Option<JobRegistry> {
        for (_, registry) in self.registries(session_id).into_iter().rev() {
            if registry.get_by_task_id(task_id).await.is_some() {
                return Some(registry);
            }
        }
        None
    }

    /// The snapshot clients render.
    pub async fn snapshot(&self, session_id: &str) -> SessionBackgroundSnapshot {
        let jobs = self.jobs(session_id).await;
        let subagents = self
            .subagents_of(session_id)
            .map(|handles| handles.tracker.running())
            .unwrap_or_default();
        SessionBackgroundSnapshot {
            session_id: session_id.to_string(),
            jobs: jobs.iter().map(WireJob::from).collect(),
            subagents: subagents.iter().map(WireSubagentRun::from).collect(),
        }
    }

    fn session_ids(&self) -> Vec<String> {
        self.sessions().keys().cloned().collect()
    }
}

/// What a client sees of a session's background work, without the clocks:
/// `session_background` fires when this changes.
fn shape(snapshot: &SessionBackgroundSnapshot) -> Vec<(u64, String)> {
    snapshot
        .jobs
        .iter()
        .map(|job| (u64::from(job.pid), format!("{:?}", job.status)))
        .chain(
            snapshot
                .subagents
                .iter()
                .map(|run| (run.run_id, "subagent".to_string())),
        )
        .collect()
}

/// Publishes `session_background` once a second while some session has
/// background work, and once more when it changed.
pub fn spawn_watcher(state: Arc<DaemonState>, bus: UiEventBus) {
    state.tokio_handle.clone().spawn(async move {
        let mut last: HashMap<String, Vec<(u64, String)>> = HashMap::new();
        loop {
            tokio::time::sleep(WATCH_INTERVAL).await;
            let mut publish = false;
            for session_id in state.session_background.session_ids() {
                let snapshot = state.session_background.snapshot(&session_id).await;
                let shape = shape(&snapshot);
                let running = snapshot
                    .jobs
                    .iter()
                    .any(|job| job.status == crate::client::background::WireJobStatus::Running)
                    || !snapshot.subagents.is_empty();
                // Running work changes every second (elapsed time, tokens).
                publish |= running || last.get(&session_id) != Some(&shape);
                last.insert(session_id, shape);
            }
            if publish {
                bus.publish(UiEventTopics::SESSION_BACKGROUND);
            }
        }
    });
}

fn session_id_param(params: &Params) -> SkyResult<String> {
    Ok(params.get("id").map_err(bad_request_error)?.to_string())
}

fn pid_param(params: &Params) -> SkyResult<u32> {
    params
        .get("pid")
        .map_err(bad_request_error)?
        .parse()
        .map_err(bad_request_error)
}

/// `GET /api/sessions/{id}/background`: the session's jobs and subagents.
pub async fn get_background(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionBackgroundSnapshot>> {
    let session_id = session_id_param(&params)?;
    Ok(Json(state.session_background.snapshot(&session_id).await))
}

/// `GET /api/sessions/{id}/jobs/{pid}`: one job and its buffered output.
pub async fn get_job(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<WireJobDetail>> {
    let session_id = session_id_param(&params)?;
    let pid = pid_param(&params)?;
    let Some(registry) = state
        .session_background
        .registry_with(&session_id, pid)
        .await
    else {
        return Ok(Json(WireJobDetail {
            job: None,
            stdout: None,
            stderr: None,
        }));
    };
    let (job, output) =
        futures_lite::future::zip(registry.get(pid), registry.terminal_output(pid)).await;
    let (stdout, stderr) = output.map_or((None, None), |(out, err)| (Some(out), Some(err)));
    Ok(Json(WireJobDetail {
        job: job.as_ref().map(WireJob::from),
        stdout,
        stderr,
    }))
}

/// Answer of `POST /api/sessions/{id}/jobs/{pid}/kill`.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct KillJobResponse {
    /// Whether the job was running and is now killed.
    pub ok: bool,
}

/// `POST /api/sessions/{id}/jobs/{pid}/kill`.
pub async fn kill_job(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<KillJobResponse>> {
    let session_id = session_id_param(&params)?;
    let pid = pid_param(&params)?;
    let ok = match state
        .session_background
        .registry_with(&session_id, pid)
        .await
    {
        Some(registry) => registry.kill(pid).await,
        None => false,
    };
    if ok {
        state
            .ui_event_bus
            .publish(UiEventTopics::SESSION_BACKGROUND);
    }
    Ok(Json(KillJobResponse { ok }))
}

/// Query of `GET /api/sessions/{id}/jobs/read`.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReadJobQuery {
    /// Task id of the job.
    pub task_id: String,
    /// Cursor from the previous read.
    #[serde(default)]
    pub cursor: usize,
}

/// `GET /api/sessions/{id}/jobs/read`: a job's output since `cursor`.
pub async fn read_job(
    params: Params,
    Query(query): Query<ReadJobQuery>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<WireTerminalDelta>> {
    let session_id = session_id_param(&params)?;
    let registry = state
        .session_background
        .registry_with_task(&session_id, &query.task_id)
        .await
        .ok_or_else(|| bad_request_error("no such job"))?;
    let delta = registry
        .terminal_read(&query.task_id, query.cursor, MAX_READ_BYTES)
        .await
        .map_err(bad_request_error)?;
    Ok(Json(WireTerminalDelta {
        cursor: delta.cursor,
        bytes: delta.bytes,
        status: (&delta.status).into(),
    }))
}

/// Answer of the subagent cancel endpoints.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CancelSubagentsResponse {
    /// How many runs were stopped.
    pub stopped: usize,
}

/// `DELETE /api/sessions/{id}/subagents/{runId}`: stops one subagent run.
///
/// The tracker and the `subagent` command's own registry number runs
/// independently, but record each run back-to-back, so the earliest entry
/// with the same subagent and prompt is the same invocation.
pub async fn cancel_subagent(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<CancelSubagentsResponse>> {
    let session_id = session_id_param(&params)?;
    let run_id: u64 = params
        .get("runId")
        .map_err(bad_request_error)?
        .parse()
        .map_err(bad_request_error)?;
    let Some(handles) = state.session_background.subagents_of(&session_id) else {
        return Ok(Json(CancelSubagentsResponse { stopped: 0 }));
    };
    let stopped = handles.tracker.get(run_id).and_then(|run| {
        let (agent, prompt) = (run.agent, run.prompt);
        handles
            .runs
            .list()
            .into_iter()
            .filter(|info| info.subagent == agent && info.prompt == prompt)
            .min_by_key(|info| info.id)
            .map(|info| info.id)
    });
    let stopped = usize::from(stopped.is_some_and(|id| handles.runs.cancel(id)));
    Ok(Json(CancelSubagentsResponse { stopped }))
}

/// `DELETE /api/sessions/{id}/subagents`: stops every subagent run.
pub async fn cancel_subagents(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<CancelSubagentsResponse>> {
    let session_id = session_id_param(&params)?;
    let stopped = state
        .session_background
        .subagents_of(&session_id)
        .map_or(0, |handles| handles.runs.cancel_all());
    Ok(Json(CancelSubagentsResponse { stopped }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aither_sandbox::TerminalMode;

    async fn registry_with_job(pid: u32, task_id: &str) -> JobRegistry {
        let (registry, service) = aither_sandbox::job_registry_channel();
        tokio::spawn(service.serve());
        registry
            .register(
                pid,
                task_id,
                "key",
                "sleep 30",
                TerminalMode::Sandboxed,
                None,
            )
            .await;
        registry
    }

    #[tokio::test]
    async fn earlier_agents_jobs_stay_listed_while_they_run() {
        let backgrounds = SessionBackgrounds::default();
        let old = registry_with_job(10, "t-old").await;
        backgrounds.add_job_registry("s", old.clone());
        let new = registry_with_job(11, "t-new").await;
        backgrounds.add_job_registry("s", new);

        let pids: Vec<u32> = backgrounds
            .jobs("s")
            .await
            .iter()
            .map(|job| job.pid)
            .collect();
        assert_eq!(pids, vec![10, 11]);
        assert!(backgrounds.registry_with("s", 10).await.is_some());

        old.complete(10, 0, None).await;
        let pids: Vec<u32> = backgrounds
            .jobs("s")
            .await
            .iter()
            .map(|job| job.pid)
            .collect();
        assert_eq!(pids, vec![11], "a finished earlier job drops out");
        assert_eq!(
            backgrounds.registries("s").len(),
            1,
            "its registry is let go"
        );
    }
}
