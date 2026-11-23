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
use std::time::{Duration, Instant};

use aither_sandbox::{Job, JobInfo, JobRead, JobStatus, Jobs, ShellEvent};
use skyzen::Result as SkyResult;
use skyzen::extract::Query;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};

use crate::agent::{DeliveredJobCompletion, SessionJobHooks, SubagentTracker, render_job_ended};
use crate::client::background::{
    SessionBackgroundSnapshot, WireJob, WireJobDetail, WireSubagentRun, WireTerminalDelta,
};
use crate::error::bad_request_error;
use crate::state::DaemonState;
use crate::ui_event_bus::{UiEventBus, UiEventTopics};

/// How often running background work is checked for changes.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
/// Largest terminal read one request returns.
const MAX_READ_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct SessionBackground {
    subagents: SubagentTracker,
    /// Jobs of the session's agents' shells, numbered, newest last.
    jobs: Vec<(u64, Jobs)>,
    /// Background wakes being served: received from the idle agent and not
    /// yet settled by the turn they start.
    resuming: Arc<std::sync::atomic::AtomicUsize>,
    /// Monotonic start instant of each user prompt row persisted in this
    /// daemon process.
    prompt_starts: Vec<(i64, Instant)>,
    /// Jobs terminated by a conversation rewind, keyed by `(pid, started_at)`
    /// so later OS PID reuse never hides a new job.
    abandoned_jobs: Arc<Mutex<Vec<(u32, Instant)>>>,
    /// Completions read by an agent during turns that may later be rewound.
    delivered_completions: Arc<Mutex<Vec<DeliveredJobCompletion>>>,
    /// Pre-rewind job completions restored across a rewind, injected on the
    /// next built agent.
    rewound_alerts: Vec<String>,
}

/// A background wake of a session being served; the session reads as
/// resuming until this drops. It holds the completions the wake recorded,
/// whose jobs read as owed a turn until the turn the wake starts settles
/// them ([`Self::settle`]).
#[derive(Debug)]
#[must_use]
pub struct ResumePending {
    resuming: Arc<std::sync::atomic::AtomicUsize>,
    completions: Vec<aither_sandbox::PendingCompletion>,
}

impl ResumePending {
    /// Settles the wake once the turn it started released the session: its
    /// completions are taken, returning once their registries recorded it,
    /// and the session stops reading as resuming.
    pub async fn settle(mut self) {
        futures_util::future::join_all(
            std::mem::take(&mut self.completions)
                .into_iter()
                .map(aither_sandbox::PendingCompletion::take),
        )
        .await;
    }
}

impl Drop for ResumePending {
    fn drop(&mut self) {
        self.resuming
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// A request that the watcher bring the record of the sessions with
/// background work up to date; it replies once the record is written.
type RecordRequest = async_channel::Sender<()>;

/// Background work of every session the daemon ran.
pub struct SessionBackgrounds {
    sessions: Mutex<HashMap<String, SessionBackground>>,
    next_registry: std::sync::atomic::AtomicU64,
    started_at: Instant,
    /// Where the watcher, the record's one writer, takes requests to bring
    /// it up to date; set once the watcher runs.
    record: std::sync::OnceLock<async_channel::Sender<RecordRequest>>,
}

impl Default for SessionBackgrounds {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            next_registry: std::sync::atomic::AtomicU64::new(0),
            started_at: Instant::now(),
            record: std::sync::OnceLock::new(),
        }
    }
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

    /// The subagent tracker `session_id`'s agents are built with.
    pub fn subagents(&self, session_id: &str) -> SubagentTracker {
        self.sessions()
            .entry(session_id.to_string())
            .or_default()
            .subagents
            .clone()
    }

    /// Records the jobs of the shell of an agent just built for `session_id`.
    pub fn add_jobs(&self, session_id: &str, jobs: Jobs) {
        let id = self
            .next_registry
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.sessions()
            .entry(session_id.to_string())
            .or_default()
            .jobs
            .push((id, jobs));
    }

    /// Records when user prompt `message_id` began in this daemon process so
    /// rewinding to it distinguishes earlier jobs from jobs launched by the
    /// rewound timeline.
    pub fn record_prompt_start(&self, session_id: &str, message_id: i64, at: Instant) {
        let mut sessions = self.sessions();
        let entry = sessions.entry(session_id.to_string()).or_default();
        entry.prompt_starts.retain(|(id, _)| *id != message_id);
        entry.prompt_starts.push((message_id, at));
        drop(sessions);
    }

    /// Registers `agent`'s shell jobs, forwards surviving earlier shells'
    /// event queues into `agent`, and injects any pre-cut completions restored
    /// across a rewind.
    pub async fn attach_agent_jobs(&self, session_id: &str, agent: &mut crate::MayAgent) {
        let (older_registries, abandoned, delivered, restored_alerts) = {
            let mut sessions = self.sessions();
            let entry = sessions.entry(session_id.to_string()).or_default();
            let tuple = (
                entry.jobs.clone(),
                entry.abandoned_jobs.clone(),
                entry.delivered_completions.clone(),
                std::mem::take(&mut entry.rewound_alerts),
            );
            drop(sessions);
            tuple
        };
        let abandoned_snapshot = abandoned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut prior_events = Vec::new();
        for (_, registry) in older_registries {
            let has_surviving = registry.list().await.into_iter().any(|job| {
                !abandoned_snapshot.contains(&(job.pid, job.started_at))
                    && (job.status == JobStatus::Running || job.status.completion_pending())
            });
            if has_surviving {
                prior_events.push(registry.events());
            }
        }
        self.add_jobs(session_id, agent.jobs());
        let hooks = SessionJobHooks::new(
            move |pid, started_at| {
                let guard = abandoned
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                started_at.map_or_else(
                    || guard.iter().any(|(abandoned_pid, _)| *abandoned_pid == pid),
                    |at| guard.contains(&(pid, at)),
                )
            },
            move |completion| {
                delivered
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(completion);
            },
        );
        agent.attach_session_job_hooks(prior_events, hooks, restored_alerts);
    }

    /// Rewinds `session_id`'s background work to before `user_message_id`:
    /// kills terminal jobs and cancels subagent runs started at or after the
    /// cut, settles their pending completions silently, and preserves jobs
    /// started before the cut along with any completions they delivered during
    /// the rewound turns.
    pub async fn rewind(&self, session_id: &str, user_message_id: i64) {
        let (cut_at, registries, subagents, abandoned_arc, delivered_arc) = {
            let mut sessions = self.sessions();
            let Some(entry) = sessions.get_mut(session_id) else {
                return;
            };
            let cut_at = entry
                .prompt_starts
                .iter()
                .find(|(id, _)| *id == user_message_id)
                .map(|(_, at)| *at)
                .or_else(|| {
                    entry
                        .prompt_starts
                        .iter()
                        .filter(|(id, _)| *id > user_message_id)
                        .map(|(_, at)| *at)
                        .min()
                        .filter(|_| {
                            entry
                                .prompt_starts
                                .iter()
                                .any(|(id, _)| *id < user_message_id)
                        })
                })
                .unwrap_or(self.started_at);
            entry.prompt_starts.retain(|(id, _)| *id < user_message_id);
            let tuple = (
                cut_at,
                entry.jobs.clone(),
                entry.subagents.clone(),
                entry.abandoned_jobs.clone(),
                entry.delivered_completions.clone(),
            );
            drop(sessions);
            tuple
        };

        let mut newly_abandoned = Vec::new();
        let mut to_settle = Vec::new();
        let mut restored_alerts = Vec::new();

        for (_, registry) in &registries {
            let list = registry.list().await;
            for job in &list {
                if job.started_at >= cut_at {
                    newly_abandoned.push((job.pid, job.started_at));
                    if job.status == JobStatus::Running {
                        let _ = registry.kill(job.pid).await;
                    }
                }
            }
        }

        if !newly_abandoned.is_empty() {
            let mut guard = abandoned_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for item in &newly_abandoned {
                if !guard.contains(item) {
                    guard.push(*item);
                }
            }
        }

        let abandoned_all = abandoned_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();

        for (_, registry) in &registries {
            let events = registry.events();
            while let Some(event) = events.try_recv() {
                if !event.agent().is_main() {
                    continue;
                }
                if let ShellEvent::JobEnded(ended) = event {
                    let pid = ended.info().pid;
                    let started_at = ended.info().started_at;
                    if started_at >= cut_at || abandoned_all.contains(&(pid, started_at)) {
                        let (_, _, _, pending) = (*ended).into_parts();
                        to_settle.extend(pending);
                    } else {
                        restored_alerts.push(render_job_ended(&ended));
                        let (_, _, _, pending) = (*ended).into_parts();
                        to_settle.extend(pending);
                    }
                }
            }
        }

        futures_util::future::join_all(
            to_settle
                .into_iter()
                .map(aither_sandbox::PendingCompletion::take),
        )
        .await;

        for run in subagents.running() {
            if run.started_at >= cut_at {
                let _ = subagents.runs().cancel(run.run_id);
            }
        }

        let reclaimed_from_delivered = {
            let mut delivered = delivered_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut kept = Vec::with_capacity(delivered.len());
            let mut reclaimed = Vec::new();
            for item in delivered.drain(..) {
                if item.started_at >= cut_at {
                    continue;
                }
                if item.delivered_at >= cut_at {
                    reclaimed.push(item.alert);
                } else {
                    kept.push(item);
                }
            }
            *delivered = kept;
            reclaimed
        };

        let mut all_restored = reclaimed_from_delivered;
        all_restored.extend(restored_alerts);
        if !all_restored.is_empty()
            && let Some(entry) = self.sessions().get_mut(session_id)
        {
            for alert in all_restored {
                if !entry.rewound_alerts.contains(&alert) {
                    entry.rewound_alerts.push(alert);
                }
            }
        }
    }

    /// Marks `session_id` as resuming until the guard drops, which holds the
    /// `completions` of the wake being served.
    pub fn resume_pending(
        &self,
        session_id: &str,
        completions: Vec<aither_sandbox::PendingCompletion>,
    ) -> ResumePending {
        let resuming = self
            .sessions()
            .entry(session_id.to_string())
            .or_default()
            .resuming
            .clone();
        resuming.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        ResumePending {
            resuming,
            completions,
        }
    }

    fn resuming(&self, session_id: &str) -> bool {
        self.sessions()
            .get(session_id)
            .is_some_and(|session| session.resuming.load(std::sync::atomic::Ordering::Acquire) > 0)
    }

    fn registries(&self, session_id: &str) -> Vec<(u64, Jobs)> {
        self.sessions()
            .get(session_id)
            .map_or_default(|session| session.jobs.clone())
    }

    fn subagents_of(&self, session_id: &str) -> Option<SubagentTracker> {
        self.sessions()
            .get(session_id)
            .map(|session| session.subagents.clone())
    }

    /// Jobs of `session_id`: every job of its newest agent, and the ones of
    /// earlier agents that still run or whose completion their agent has
    /// not taken. Earlier registries with neither are dropped.
    async fn jobs(&self, session_id: &str) -> Vec<JobInfo> {
        let (registries, abandoned) = {
            let sessions = self.sessions();
            let Some(session) = sessions.get(session_id) else {
                return Vec::new();
            };
            let abandoned = session
                .abandoned_jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let tuple = (session.jobs.clone(), abandoned);
            drop(sessions);
            tuple
        };
        let Some(((_, newest), older)) = registries.split_last() else {
            return Vec::new();
        };
        let mut jobs = Vec::new();
        let mut idle = Vec::new();
        for (id, registry) in older {
            let live: Vec<_> = registry
                .list()
                .await
                .into_iter()
                .filter(|job| {
                    !abandoned.contains(&(job.pid, job.started_at))
                        && (job.status == JobStatus::Running || job.status.completion_pending())
                })
                .collect();
            if live.is_empty() {
                idle.push(*id);
            }
            jobs.extend(live);
        }
        jobs.extend(
            newest
                .list()
                .await
                .into_iter()
                .filter(|job| !abandoned.contains(&(job.pid, job.started_at))),
        );
        if !idle.is_empty()
            && let Some(session) = self.sessions().get_mut(session_id)
        {
            session.jobs.retain(|(id, _)| !idle.contains(id));
        }
        jobs
    }

    /// The job `pid` of `session_id`, newest shell first, with what it holds.
    async fn job_with(&self, session_id: &str, pid: u32) -> Option<(Job, JobInfo)> {
        for (_, jobs) in self.registries(session_id).into_iter().rev() {
            let Some(info) = jobs.list().await.into_iter().find(|job| job.pid == pid) else {
                continue;
            };
            if let Some(job) = jobs.get(info.pid).await {
                return Some((job, info));
            }
        }
        None
    }

    /// The snapshot clients render.
    pub async fn snapshot(&self, session_id: &str) -> SessionBackgroundSnapshot {
        let jobs = self.jobs(session_id).await;
        let subagents = self
            .subagents_of(session_id)
            .map_or_default(|tracker| tracker.running());
        // A finished job whose completion its agent has not taken still owes
        // the session a turn, from the moment the registry records the finish.
        let owed_turn = jobs.iter().any(|job| job.status.completion_pending());
        SessionBackgroundSnapshot {
            session_id: session_id.to_string(),
            jobs: jobs.iter().map(WireJob::from).collect(),
            subagents: subagents.iter().map(WireSubagentRun::from).collect(),
            resuming: owed_turn || self.resuming(session_id),
        }
    }

    /// Kills every running job of `session_id`'s agents and cancels its
    /// subagent runs; returns how many it stopped.
    pub async fn stop_all(&self, session_id: &str) -> usize {
        let mut stopped = 0;
        for (_, jobs) in self.registries(session_id) {
            stopped += jobs.kill_all().await;
        }
        stopped
            + self
                .subagents_of(session_id)
                .map_or(0, |tracker| tracker.runs().cancel_all())
    }

    /// The sessions with background work: a command or subagent run in
    /// flight, or a wake being served.
    pub async fn busy_sessions(&self) -> std::collections::BTreeSet<String> {
        let mut busy = std::collections::BTreeSet::new();
        for session_id in self.session_ids() {
            if self.snapshot(&session_id).await.will_resume() {
                busy.insert(session_id);
            }
        }
        busy
    }

    fn session_ids(&self) -> Vec<String> {
        self.sessions().keys().cloned().collect()
    }

    /// Brings the persisted record of the sessions with background work up
    /// to date with the work alive now, and returns once it is written. A
    /// run that released its session calls this before its clients hear the
    /// turn end, so a daemon killed after that end never finds the finished
    /// session recorded as interrupted. Before the watcher runs nothing is
    /// recorded, so there is nothing to bring up to date.
    pub async fn record_now(&self) {
        let Some(requests) = self.record.get() else {
            return;
        };
        let (reply, written) = async_channel::bounded(1);
        // Either end closes only when the daemon shuts down.
        if requests.send(reply).await.is_ok() {
            let _ = written.recv().await;
        }
    }
}

/// What woke the background watcher.
enum WatcherWake {
    /// The once-a-second check is due.
    Tick,
    /// A run asked for the record to be brought up to date.
    Record(RecordRequest),
}

/// Writes `busy` as the record of the sessions with background work when
/// it differs from what was `persisted`.
async fn write_record(
    state: &DaemonState,
    busy: &std::collections::BTreeSet<String>,
    persisted: &mut std::collections::BTreeSet<String>,
) {
    if busy == persisted {
        return;
    }
    match state.db().set_sessions_with_background(busy).await {
        Ok(()) => persisted.clone_from(busy),
        Err(error) => {
            tracing::error!(%error, "failed to record the sessions with background work");
        }
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
/// background work, and once more when it changed. It is the one writer of
/// the record of the sessions with background work: it updates the record
/// on each check, and at once when a run asks through
/// [`SessionBackgrounds::record_now`].
///
/// # Panics
/// Panics when a watcher already runs for this daemon.
pub fn spawn_watcher(state: Arc<DaemonState>, bus: UiEventBus) {
    let (requests, record_requests) = async_channel::unbounded::<RecordRequest>();
    assert!(
        state.session_background.record.set(requests).is_ok(),
        "one background watcher runs per daemon"
    );
    state.tokio_handle.clone().spawn(async move {
        let mut last: HashMap<String, Vec<(u64, String)>> = HashMap::new();
        let mut last_busy = std::collections::BTreeSet::new();
        // Startup cleared the record, so it starts empty.
        let mut persisted = std::collections::BTreeSet::new();
        let mut next_tick = std::time::Instant::now() + WATCH_INTERVAL;
        loop {
            let wake = futures_lite::future::or(
                async {
                    async_io::Timer::at(next_tick).await;
                    WatcherWake::Tick
                },
                async {
                    // The sender lives in the daemon state this task holds.
                    match record_requests.recv().await {
                        Ok(reply) => WatcherWake::Record(reply),
                        Err(_) => std::future::pending().await,
                    }
                },
            )
            .await;
            if matches!(wake, WatcherWake::Tick) {
                next_tick = std::time::Instant::now() + WATCH_INTERVAL;
            }
            crate::panic_boundary::round("background watcher", async {
                if let WatcherWake::Record(reply) = wake {
                    let busy = state.session_background.busy_sessions().await;
                    write_record(&state, &busy, &mut persisted).await;
                    let _ = reply.try_send(());
                    return;
                }
                let mut changed = std::collections::BTreeSet::new();
                let mut busy = std::collections::BTreeSet::new();
                for session_id in state.session_background.session_ids() {
                    let snapshot = state.session_background.snapshot(&session_id).await;
                    let shape = shape(&snapshot);
                    if snapshot.will_resume() {
                        busy.insert(session_id.clone());
                    }
                    let running =
                        snapshot.jobs.iter().any(|job| {
                            job.status == crate::client::background::WireJobStatus::Running
                        }) || !snapshot.subagents.is_empty();
                    // Running work changes every second (elapsed time, tokens).
                    if running || last.get(&session_id) != Some(&shape) {
                        changed.insert(session_id.clone());
                    }
                    last.insert(session_id, shape);
                }
                if !changed.is_empty() {
                    for session_id in changed {
                        bus.publish_session_change(
                            &session_id,
                            crate::session_events::SessionChange::Background,
                        );
                    }
                }
                // The session lists mark the sessions with background work, and
                // the record tells the next daemon which sessions an unexpected
                // exit interrupted.
                if busy != last_busy {
                    bus.publish(UiEventTopics::PROFILE_SESSIONS);
                }
                write_record(&state, &busy, &mut persisted).await;
                last_busy = busy;
            })
            .await;
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
    let Some((job, info)) = state.session_background.job_with(&session_id, pid).await else {
        return Ok(Json(WireJobDetail {
            job: None,
            output: None,
        }));
    };
    // The shell keeps one buffer per job, in which the two streams are
    // interleaved as they arrived.
    let output = match job.read(0, usize::MAX).await {
        Ok(JobRead::Delta(delta)) => Some(delta.bytes),
        Ok(JobRead::Unchanged { .. }) | Err(_) => None,
    };
    Ok(Json(WireJobDetail {
        job: Some(WireJob::from(&info)),
        output,
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
    let ok = match state.session_background.job_with(&session_id, pid).await {
        Some((job, _)) => job.kill().await,
        None => false,
    };
    if ok {
        state.publish_session_change(
            &session_id,
            crate::session_events::SessionChange::Background,
        );
    }
    Ok(Json(KillJobResponse { ok }))
}

/// Query of `GET /api/sessions/{id}/jobs/read`.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReadJobQuery {
    /// Process ID of the job.
    pub pid: u32,
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
    let (job, _) = state
        .session_background
        .job_with(&session_id, query.pid)
        .await
        .ok_or_else(|| bad_request_error("no such job"))?;
    match job
        .read(query.cursor, MAX_READ_BYTES)
        .await
        .map_err(bad_request_error)?
    {
        JobRead::Delta(delta) => Ok(Json(WireTerminalDelta {
            cursor: delta.cursor,
            bytes: delta.bytes,
            status: (&delta.status).into(),
        })),
        // A client that polls reads what its last read did; the answer is
        // the job's state with nothing new.
        JobRead::Unchanged { .. } => Ok(Json(WireTerminalDelta {
            cursor: query.cursor,
            bytes: Vec::new(),
            status: (&job.info().await.status).into(),
        })),
    }
}

/// Answer of the subagent cancel endpoints.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CancelSubagentsResponse {
    /// How many runs were stopped.
    pub stopped: usize,
}

/// `DELETE /api/sessions/{id}/subagents/{runId}`: stops one subagent run.
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
    let Some(tracker) = state.session_background.subagents_of(&session_id) else {
        return Ok(Json(CancelSubagentsResponse { stopped: 0 }));
    };
    let stopped = usize::from(tracker.runs().cancel(run_id));
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
        .map_or(0, |tracker| tracker.runs().cancel_all());
    Ok(Json(CancelSubagentsResponse { stopped }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_sandbox::TestTerminal;

    /// A real session shell in its own directory, running a `sleep` job in
    /// the background: the shell, the job and the job's pid.
    async fn shell_with_job(dir: &std::path::Path) -> (TestTerminal, aither_sandbox::Job, u32) {
        let terminal = TestTerminal::new(dir).await;
        let (job, pid) = terminal.sleeping_job().await;
        (terminal, job, pid)
    }

    /// The agent taking the completion of the shell's next ended job: the
    /// event that carries it is read and dropped.
    async fn take_completion(terminal: &TestTerminal) {
        let events = terminal.shell().jobs().events();
        loop {
            let event = events
                .recv_timeout(std::time::Duration::from_secs(30))
                .await
                .expect("the job ended");
            if matches!(event, aither_sandbox::ShellEvent::JobEnded(_)) {
                return;
            }
        }
    }

    #[tokio::test]
    async fn earlier_agents_jobs_stay_listed_while_they_run() {
        crate::test_support::install_global_executor();
        let dir = tempfile::tempdir().expect("temp dir");
        let backgrounds = SessionBackgrounds::default();
        let (old_shell, old, old_pid) = shell_with_job(&dir.path().join("old")).await;
        backgrounds.add_jobs("s", old_shell.shell().jobs());
        let (new_shell, _new, new_pid) = shell_with_job(&dir.path().join("new")).await;
        backgrounds.add_jobs("s", new_shell.shell().jobs());

        let pids = async || -> Vec<u32> {
            backgrounds
                .jobs("s")
                .await
                .iter()
                .map(|job| job.pid)
                .collect()
        };
        assert_eq!(pids().await, vec![old_pid, new_pid]);
        assert!(backgrounds.job_with("s", old_pid).await.is_some());

        assert!(old.kill().await, "the job was running");
        old.info().await;
        assert_eq!(
            pids().await,
            vec![old_pid, new_pid],
            "its completion is still owed"
        );
        take_completion(&old_shell).await;
        drop(old);
        assert_eq!(
            pids().await,
            vec![new_pid],
            "a finished earlier job drops out"
        );
        assert_eq!(backgrounds.registries("s").len(), 1, "its shell is let go");
    }

    #[tokio::test]
    async fn a_finished_job_reads_as_resuming_until_its_completion_is_taken() {
        crate::test_support::install_global_executor();
        let dir = tempfile::tempdir().expect("temp dir");
        let backgrounds = SessionBackgrounds::default();
        let (shell, job, _) = shell_with_job(dir.path()).await;
        backgrounds.add_jobs("s", shell.shell().jobs());
        assert!(backgrounds.snapshot("s").await.will_resume(), "it runs");

        // The job finishes; its agent has not taken the completion yet.
        assert!(job.kill().await, "the job was running");
        let snapshot = backgrounds.snapshot("s").await;
        assert!(snapshot.resuming, "{snapshot:?}");

        take_completion(&shell).await;
        assert!(!backgrounds.snapshot("s").await.will_resume());
    }

    #[tokio::test]
    async fn rewind_kills_post_cut_jobs_and_preserves_pre_cut_jobs_and_completions() {
        crate::test_support::install_global_executor();
        let dir = tempfile::tempdir().expect("temp dir");
        let backgrounds = SessionBackgrounds::default();

        backgrounds.record_prompt_start("s", 1, Instant::now());
        let (pre_shell, _pre_job, pre_pid) = shell_with_job(&dir.path().join("pre")).await;
        backgrounds.add_jobs("s", pre_shell.shell().jobs());

        let (done_shell, done_job, done_pid) = shell_with_job(&dir.path().join("done")).await;
        backgrounds.add_jobs("s", done_shell.shell().jobs());

        let cut_at = Instant::now();
        backgrounds.record_prompt_start("s", 2, cut_at);

        // Simulate a pre-cut job whose completion was read during the rewound turn.
        assert!(done_job.kill().await);
        take_completion(&done_shell).await;
        {
            let delivered = backgrounds
                .sessions()
                .get("s")
                .expect("session entry")
                .delivered_completions
                .clone();
            delivered
                .lock()
                .expect("lock")
                .push(DeliveredJobCompletion {
                    pid: done_pid,
                    started_at: cut_at.checked_sub(Duration::from_millis(50)).unwrap(),
                    delivered_at: Instant::now(),
                    alert: format!("Background job {done_pid} finished"),
                });
        }

        let (post_shell, _post_job, post_pid) = shell_with_job(&dir.path().join("post")).await;
        backgrounds.add_jobs("s", post_shell.shell().jobs());

        assert_eq!(
            backgrounds
                .snapshot("s")
                .await
                .jobs
                .iter()
                .map(|j| j.pid)
                .collect::<Vec<_>>(),
            vec![pre_pid, post_pid]
        );

        backgrounds.rewind("s", 2).await;

        let after = backgrounds.snapshot("s").await;
        assert_eq!(
            after.jobs.iter().map(|j| j.pid).collect::<Vec<_>>(),
            vec![pre_pid],
            "post-cut job is killed and removed while pre-cut job keeps running"
        );
        assert!(
            !after.resuming,
            "abandoned job completion never leaves the session resuming"
        );
        {
            let alerts = backgrounds
                .sessions()
                .get("s")
                .expect("session entry")
                .rewound_alerts
                .clone();
            assert_eq!(
                alerts,
                vec![format!("Background job {done_pid} finished")],
                "pre-cut completion read during rewound turn is restored for the next agent"
            );
        }

        backgrounds.stop_all("s").await;
    }
}
