//! Home-environment disk exhaustion survival and container runtime recovery.
//!
//! When the machine's disk fills (`ENOSPC`, `EDQUOT`) or a container runtime
//! crashes on host I/O failure (`EIO` / disappeared socket), the daemon keeps
//! serving:
//! - exhaustion failures in tool outputs and container execution are classified
//!   and reported to the model in natural language so it knows its home
//!   environment is out of disk space;
//! - transcript messages, context checkpoints, and assistant stats that SQLite
//!   cannot write are buffered per session in [`PendingTranscriptStore`] and
//!   retried once space returns, losing nothing;
//! - container execution wraps the active runtime in [`ResilientContainerExec`],
//!   which reconnects to a restarted runtime and restarts or recreates the
//!   session's container on demand.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use aither_agent::AgentCheckpoint;
use aither_sandbox::{
    AnyContainerExec, BollardContainerExec, ContainerExec, ContainerExecOutcome,
    ContainerLaunchSpec, ExecLifetime,
};
use askama::Template;
use async_channel::{Receiver, Sender};

use super::state::DaemonState;
use crate::container::lifecycle::ContainerManager;
use crate::db::{ChatDb, ChatDbError, MessageStatsParams, SessionContextSnapshotUpsert};
use crate::session::ChatMessage;

#[derive(Template)]
#[template(path = "disk_exhausted_tool_notice.txt", escape = "none")]
struct HomeDiskExhaustedToolNotice<'a> {
    cause: &'a str,
    detail: &'a str,
}

#[derive(Template)]
#[template(path = "reminder_disk_exhausted.txt", escape = "none")]
struct HomeDiskExhaustedAlert<'a> {
    cause: &'a str,
}

#[derive(Template)]
#[template(path = "container_runtime_unavailable.txt", escape = "none")]
struct ContainerRuntimeUnavailableNotice<'a> {
    detail: &'a str,
}

#[derive(Debug, Default)]
struct SessionTranscriptBacklog {
    messages: Vec<ChatMessage>,
    snapshot: Option<SessionContextSnapshotUpsert>,
    stats: Option<MessageStatsParams>,
    pending_alert: Option<DiskExhaustionCause>,
}

impl SessionTranscriptBacklog {
    const fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.snapshot.is_none() && self.stats.is_none()
    }
}

/// Buffers transcript writes, context snapshots, and assistant usage stats in
/// memory when SQLite fails on an exhausted disk, retrying once space returns.
#[derive(Debug, Clone)]
pub struct PendingTranscriptStore {
    sessions: Arc<Mutex<HashMap<String, SessionTranscriptBacklog>>>,
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
}

impl Default for PendingTranscriptStore {
    fn default() -> Self {
        let (wake_tx, wake_rx) = async_channel::bounded(1);
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            wake_tx,
            wake_rx,
        }
    }
}

impl PendingTranscriptStore {
    fn nudge(&self) {
        let _ = self.wake_tx.try_send(());
    }

    /// Queues a one-shot `<system-reminder>` alert for `session_id` if one is
    /// not already queued.
    ///
    /// # Panics
    /// Panics when the pending-transcript lock is poisoned.
    pub fn arm_alert(&self, session_id: &str, cause: DiskExhaustionCause) {
        let mut sessions = self.sessions.lock().expect("pending transcript lock");
        let entry = sessions.entry(session_id.to_string()).or_default();
        if entry.pending_alert.is_none() {
            entry.pending_alert = Some(cause);
        }
        drop(sessions);
    }

    /// Takes the queued one-shot `<system-reminder>` alert for `session_id`, if any.
    #[must_use]
    /// Takes the queued alert for `session_id`, if one is armed.
    ///
    /// # Panics
    /// Panics when the pending-transcript lock is poisoned.
    pub fn take_alert(&self, session_id: &str) -> Option<String> {
        let mut sessions = self.sessions.lock().expect("pending transcript lock");
        let alert = sessions
            .get_mut(session_id)
            .and_then(|entry| entry.pending_alert.take())
            .map(format_disk_exhausted_alert);
        if sessions
            .get(session_id)
            .is_some_and(|entry| entry.is_empty() && entry.pending_alert.is_none())
        {
            sessions.remove(session_id);
        }
        alert
    }

    /// Defers chat messages that could not be committed to SQLite.
    ///
    /// # Panics
    /// Panics when the pending-transcript lock is poisoned.
    pub fn defer_messages(
        &self,
        session_id: &str,
        messages: Vec<ChatMessage>,
        cause: DiskExhaustionCause,
    ) {
        if messages.is_empty() {
            self.arm_alert(session_id, cause);
            return;
        }
        {
            let mut sessions = self.sessions.lock().expect("pending transcript lock");
            let entry = sessions.entry(session_id.to_string()).or_default();
            entry.messages.extend(messages);
            if entry.pending_alert.is_none() {
                entry.pending_alert = Some(cause);
            }
            drop(sessions);
        }
        self.nudge();
    }

    /// Defers the latest agent checkpoint when SQLite cannot persist it.
    ///
    /// # Panics
    /// Panics when the pending-transcript lock is poisoned.
    pub fn defer_checkpoint(
        &self,
        session_id: &str,
        checkpoint: AgentCheckpoint,
        cause: DiskExhaustionCause,
    ) {
        let Ok(upsert) = crate::session::snapshot_upsert(checkpoint) else {
            return;
        };
        {
            let mut sessions = self.sessions.lock().expect("pending transcript lock");
            let entry = sessions.entry(session_id.to_string()).or_default();
            entry.snapshot = Some(upsert);
            if entry.pending_alert.is_none() {
                entry.pending_alert = Some(cause);
            }
            drop(sessions);
        }
        self.nudge();
    }

    /// Defers assistant token/latency stats when SQLite cannot persist them.
    ///
    /// # Panics
    /// Panics when the pending-transcript lock is poisoned.
    pub fn defer_stats(
        &self,
        session_id: &str,
        stats: MessageStatsParams,
        cause: DiskExhaustionCause,
    ) {
        {
            let mut sessions = self.sessions.lock().expect("pending transcript lock");
            let entry = sessions.entry(session_id.to_string()).or_default();
            entry.stats = Some(stats);
            if entry.pending_alert.is_none() {
                entry.pending_alert = Some(cause);
            }
            drop(sessions);
        }
        self.nudge();
    }

    /// Whether `session_id` has deferred transcript writes waiting for space.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn has_pending(&self, session_id: &str) -> bool {
        self.sessions
            .lock()
            .expect("pending transcript lock")
            .get(session_id)
            .is_some_and(|entry| !entry.is_empty())
    }

    /// Sessions with deferred writes waiting for disk space.
    ///
    /// # Panics
    /// Panics when the pending-transcript lock is poisoned.
    #[must_use]
    pub fn pending_session_ids(&self) -> Vec<String> {
        self.sessions
            .lock()
            .expect("pending transcript lock")
            .iter()
            .filter(|(_, entry)| !entry.is_empty())
            .map(|(session_id, _)| session_id.clone())
            .collect()
    }

    /// Attempts to flush deferred writes for `session_id` to `db`.
    ///
    /// Any write that still fails stays buffered in order for the next retry.
    ///
    /// # Errors
    /// Fails when a deferred write still cannot be committed.
    ///
    /// # Panics
    /// Panics when the pending-transcript lock is poisoned.
    pub async fn flush_session(&self, db: &ChatDb, session_id: &str) -> Result<bool, ChatDbError> {
        let (messages, snapshot, stats) = {
            let mut sessions = self.sessions.lock().expect("pending transcript lock");
            let Some(entry) = sessions.get_mut(session_id) else {
                return Ok(false);
            };
            if entry.is_empty() {
                return Ok(false);
            }
            let taken = (
                std::mem::take(&mut entry.messages),
                entry.snapshot.take(),
                entry.stats.take(),
            );
            drop(sessions);
            taken
        };

        if !messages.is_empty()
            && let Err(error) = db.append_chat_messages(session_id, &messages).await
        {
            let mut sessions = self.sessions.lock().expect("pending transcript lock");
            let entry = sessions.entry(session_id.to_string()).or_default();
            let mut combined = messages;
            combined.append(&mut entry.messages);
            entry.messages = combined;
            if entry.snapshot.is_none() {
                entry.snapshot = snapshot;
            }
            if entry.stats.is_none() {
                entry.stats = stats;
            }
            drop(sessions);
            return Err(error);
        }

        if let Some(snapshot) = snapshot {
            let backup_checkpoint = snapshot.checkpoint.clone();
            let backup_seq = snapshot.applied_seq;
            let backup_entries = snapshot.transcript_entries.clone();
            if let Err(error) = db
                .upsert_session_context_snapshot(session_id, snapshot)
                .await
            {
                let mut sessions = self.sessions.lock().expect("pending transcript lock");
                let entry = sessions.entry(session_id.to_string()).or_default();
                if entry.snapshot.is_none() {
                    entry.snapshot = Some(SessionContextSnapshotUpsert {
                        checkpoint: backup_checkpoint,
                        applied_seq: backup_seq,
                        transcript_entries: backup_entries,
                    });
                }
                if entry.stats.is_none() {
                    entry.stats = stats;
                }
                drop(sessions);
                return Err(error);
            }
        }

        if let Some(stats) = stats
            && let Err(error) = db.set_latest_assistant_stats(session_id, &stats).await
        {
            let mut sessions = self.sessions.lock().expect("pending transcript lock");
            let entry = sessions.entry(session_id.to_string()).or_default();
            if entry.stats.is_none() {
                entry.stats = Some(stats);
            }
            drop(sessions);
            return Err(error);
        }

        self.sessions
            .lock()
            .expect("pending transcript lock")
            .retain(|id, entry| {
                !(id == session_id && entry.is_empty() && entry.pending_alert.is_none())
            });
        Ok(true)
    }

    /// Flushes every session with deferred writes, waking the search indexer
    /// when recovered messages land.
    pub async fn flush_all(&self, state: &DaemonState) -> usize {
        let session_ids = self.pending_session_ids();
        let mut flushed = 0usize;
        for session_id in session_ids {
            match self.flush_session(&state.db(), &session_id).await {
                Ok(true) => {
                    flushed += 1;
                    state.notify_ui_events(crate::ui_event_bus::UiEventTopics::PROFILE_SESSIONS);
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::debug!(
                        %session_id,
                        %error,
                        "deferred transcript writes still waiting for disk space"
                    );
                }
            }
        }
        if flushed > 0 {
            let _ = state.search_index_wake_tx.try_send(());
        }
        flushed
    }
}

const TRANSCRIPT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Spawns a background worker that retries deferred transcript writes whenever
/// writes are queued, stopping once the backlog empties or the daemon shuts down.
pub(super) fn spawn_pending_transcript_flusher(state: &Arc<DaemonState>) {
    let weak_state = Arc::downgrade(state);
    let wake_rx = state.pending_transcripts.wake_rx.clone();
    let shutdown = state.daemon_shutdown.clone();
    state.tokio_handle.spawn(async move {
        loop {
            let should_run = futures_lite::future::or(
                async {
                    shutdown.notified().await;
                    false
                },
                async { wake_rx.recv().await.is_ok() },
            )
            .await;
            if !should_run {
                break;
            }
            loop {
                let Some(state) = weak_state.upgrade() else {
                    return;
                };
                let _ = state.pending_transcripts.flush_all(&state).await;
                if state.pending_transcripts.pending_session_ids().is_empty() {
                    drop(state);
                    break;
                }
                drop(state);
                let keep_waiting = futures_lite::future::or(
                    async {
                        shutdown.notified().await;
                        false
                    },
                    async {
                        async_io::Timer::after(TRANSCRIPT_RETRY_INTERVAL).await;
                        true
                    },
                )
                .await;
                if !keep_waiting {
                    return;
                }
            }
        }
    });
}

/// Whether a container execution error indicates that the container runtime
/// process or container disappeared and should be reconnected/recreated.
#[must_use]
pub fn is_container_runtime_disconnect(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("connection refused")
        || lower.contains("no such file or directory")
        || lower.contains("broken pipe")
        || lower.contains("connection reset")
        || lower.contains("cannot connect to the docker daemon")
        || lower.contains("error trying to connect")
        || lower.contains("no such container")
        || lower.contains("is not running")
        || lower.contains("container is stopped")
        || lower.contains("eof")
        || lower.contains("socket")
}

/// Reconnects to a container runtime and ensures the target container is running.
pub trait ContainerReconnect: Send + Sync + 'static {
    /// Re-establishes the container exec, returning it and its container id.
    fn reconnect(&self) -> impl Future<Output = Result<(AnyContainerExec, String), String>> + Send;
}

/// [`ContainerExec`] wrapper that survives a crashed or restarted container
/// runtime and classifies host disk exhaustion (`ENOSPC`, `EDQUOT`, `EIO`) into
/// clear natural-language model errors.
pub struct ResilientContainerExec<R> {
    active: async_lock::Mutex<(AnyContainerExec, String)>,
    reconnect: R,
}

impl<R: std::fmt::Debug> std::fmt::Debug for ResilientContainerExec<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResilientContainerExec")
            .field("reconnect", &self.reconnect)
            .finish_non_exhaustive()
    }
}

impl<R: ContainerReconnect> ResilientContainerExec<R> {
    /// A resilient exec over `initial_exec`, reconnecting through `reconnect`.
    #[must_use]
    pub const fn new(
        initial_exec: AnyContainerExec,
        initial_container_id: String,
        reconnect: R,
    ) -> Self {
        Self {
            active: async_lock::Mutex::new((initial_exec, initial_container_id)),
            reconnect,
        }
    }

    async fn active_target(&self) -> (AnyContainerExec, String) {
        self.active.lock().await.clone()
    }

    async fn try_reconnect(
        &self,
        failed_container_id: &str,
    ) -> Result<(AnyContainerExec, String), String> {
        // The guard stays held across `reconnect` so concurrent commands
        // serialize: a second caller waits for the fresh container instead of
        // racing a recreate of its own.
        let guard = &mut *self.active.lock().await;
        if guard.1 != failed_container_id {
            return Ok(guard.clone());
        }
        let next = self.reconnect.reconnect().await?;
        *guard = next.clone();
        Ok(next)
    }

    fn map_exec_error(error: String) -> String {
        if let Some(cause) = classify_error_text(&error) {
            return format_disk_exhausted_tool_error(cause, &error);
        }
        if is_container_runtime_disconnect(&error) {
            return format_container_unavailable_error(&error);
        }
        error
    }
}

impl<R: ContainerReconnect> ContainerExec for ResilientContainerExec<R> {
    async fn exec(
        &self,
        _container_id: &str,
        script: &str,
        working_dir: &str,
        kill_rx: Receiver<()>,
        stdin_rx: Receiver<Vec<u8>>,
        lifetime: ExecLifetime,
    ) -> Result<ContainerExecOutcome, String> {
        let (exec, active_id) = self.active_target().await;
        match exec
            .exec(
                &active_id,
                script,
                working_dir,
                kill_rx.clone(),
                stdin_rx.clone(),
                lifetime.clone(),
            )
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(first_error) if is_container_runtime_disconnect(&first_error) => {
                match self.try_reconnect(&active_id).await {
                    Ok((reconnected_exec, new_id)) => reconnected_exec
                        .exec(&new_id, script, working_dir, kill_rx, stdin_rx, lifetime)
                        .await
                        .map_err(Self::map_exec_error),
                    Err(reconnect_error) => Err(format_container_unavailable_error(&format!(
                        "{first_error}; reconnect failed: {reconnect_error}"
                    ))),
                }
            }
            Err(error) => Err(Self::map_exec_error(error)),
        }
    }

    async fn exec_streamed(
        &self,
        _container_id: &str,
        script: &str,
        working_dir: &str,
        kill_rx: Receiver<()>,
        stdin_rx: Receiver<Vec<u8>>,
        stdout: Sender<Vec<u8>>,
        lifetime: ExecLifetime,
    ) -> Result<ContainerExecOutcome, String> {
        let (exec, active_id) = self.active_target().await;
        match exec
            .exec_streamed(
                &active_id,
                script,
                working_dir,
                kill_rx.clone(),
                stdin_rx.clone(),
                stdout.clone(),
                lifetime.clone(),
            )
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(first_error) if is_container_runtime_disconnect(&first_error) => {
                match self.try_reconnect(&active_id).await {
                    Ok((reconnected_exec, new_id)) => reconnected_exec
                        .exec_streamed(
                            &new_id,
                            script,
                            working_dir,
                            kill_rx,
                            stdin_rx,
                            stdout,
                            lifetime,
                        )
                        .await
                        .map_err(Self::map_exec_error),
                    Err(reconnect_error) => Err(format_container_unavailable_error(&format!(
                        "{first_error}; reconnect failed: {reconnect_error}"
                    ))),
                }
            }
            Err(error) => Err(Self::map_exec_error(error)),
        }
    }
}

/// Production [`ContainerReconnect`] for daemon chat sessions: re-detects the
/// container runtime when needed and restarts or recreates `launch_spec`.
#[derive(Debug)]
pub struct DaemonContainerReconnect {
    state: Weak<DaemonState>,
    launch_spec: ContainerLaunchSpec,
}

impl DaemonContainerReconnect {
    /// A reconnect holding `state` for `launch_spec`.
    #[must_use]
    pub const fn new(state: Weak<DaemonState>, launch_spec: ContainerLaunchSpec) -> Self {
        Self { state, launch_spec }
    }
}

impl ContainerReconnect for DaemonContainerReconnect {
    async fn reconnect(&self) -> Result<(AnyContainerExec, String), String> {
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| "daemon is shutting down".to_string())?;
        let manager = state
            .resolve_container_manager(false)
            .await
            .map_err(|error| error.to_string())?;
        let container_id = manager
            .ensure_running(&self.launch_spec)
            .await
            .map_err(|error| error.to_string())?;
        manager.touch_activity(&container_id);
        let exec = AnyContainerExec::new(Arc::new(BollardContainerExec::new(Arc::new(
            manager.runtime().client.clone(),
        ))));
        Ok((exec, container_id))
    }
}

/// [`ContainerReconnect`] for scheduled container tasks: re-detects a live
/// runtime into `may_dir` if the cached manager's runtime disappeared, and
/// ensures `launch_spec` is running.
#[derive(Debug)]
pub struct TaskContainerReconnect {
    may_dir: std::path::PathBuf,
    cached_manager: Arc<ContainerManager>,
    launch_spec: ContainerLaunchSpec,
}

impl TaskContainerReconnect {
    /// A reconnect re-detecting into `may_dir` for `launch_spec`.
    #[must_use]
    pub const fn new(
        may_dir: std::path::PathBuf,
        cached_manager: Arc<ContainerManager>,
        launch_spec: ContainerLaunchSpec,
    ) -> Self {
        Self {
            may_dir,
            cached_manager,
            launch_spec,
        }
    }
}

impl ContainerReconnect for TaskContainerReconnect {
    async fn reconnect(&self) -> Result<(AnyContainerExec, String), String> {
        let ping_ok = futures_lite::future::or(
            async { self.cached_manager.runtime().client.ping().await.is_ok() },
            async {
                async_io::Timer::after(Duration::from_secs(2)).await;
                false
            },
        )
        .await;
        let manager = if ping_ok {
            self.cached_manager.clone()
        } else {
            let runtime = crate::container::ContainerRuntime::detect()
                .await
                .map_err(|error| error.to_string())?;
            Arc::new(ContainerManager::new(
                Arc::new(runtime),
                self.may_dir.clone(),
            ))
        };
        let container_id = manager
            .ensure_running(&self.launch_spec)
            .await
            .map_err(|error| error.to_string())?;
        manager.touch_activity(&container_id);
        let exec = AnyContainerExec::new(Arc::new(BollardContainerExec::new(Arc::new(
            manager.runtime().client.clone(),
        ))));
        Ok((exec, container_id))
    }
}

/// Renders the natural-language error returned to the model when an output
/// store or container execution fails due to home-environment disk exhaustion.
///
/// # Panics
/// Panics when the notice template fails to render.
#[must_use]
pub fn format_disk_exhausted_tool_error(cause: DiskExhaustionCause, detail: &str) -> String {
    HomeDiskExhaustedToolNotice {
        cause: cause.label(),
        detail: detail.trim(),
    }
    .render()
    .expect("disk exhaustion tool notice template must render")
}

/// Renders a one-shot `<system-reminder>` block telling the model that the home
/// environment's disk is exhausted.
///
/// # Panics
/// Panics when the alert template fails to render.
#[must_use]
pub fn format_disk_exhausted_alert(cause: DiskExhaustionCause) -> String {
    HomeDiskExhaustedAlert {
        cause: cause.label(),
    }
    .render()
    .expect("disk exhaustion alert template must render")
}

/// Renders a natural-language notice when a container runtime crashed and has
/// not yet come back up.
///
/// # Panics
/// Panics when the notice template fails to render.
#[must_use]
pub fn format_container_unavailable_error(detail: &str) -> String {
    if let Some(cause) = classify_error_text(detail) {
        return format_disk_exhausted_tool_error(cause, detail);
    }
    ContainerRuntimeUnavailableNotice {
        detail: detail.trim(),
    }
    .render()
    .expect("container runtime unavailable notice template must render")
}

pub use may_protocol::disk_exhaustion::*;

#[cfg(test)]
mod tests {
    use std::process::{ExitStatus, Output};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use aither_agent::{
        AgentCheckpoint, Context, ContextWindowMetrics, ContextWindowPhase, ContextWindowSnapshot,
    };
    use aither_core::llm::{Message as LlmMessage, ToolResult};
    use aither_sandbox::{AnyContainerExec, ContainerExec, ContainerExecOutcome, ExecLifetime};

    use super::{
        ContainerReconnect, DiskExhaustionCause, ResilientContainerExec, classify_error_text,
        format_disk_exhausted_alert, format_disk_exhausted_tool_error,
    };
    use crate::chat::TurnKind;
    use crate::config::MayConfig;
    use crate::db::{MessageStatsParams, RunFence};
    use crate::session::{ChatMessage, PendingUserMessageMode, load_history_from_db};
    use crate::state::DaemonState;
    use crate::types::ChatRequest;
    use may_protocol::disk_exhaustion::classify_io_error;

    #[cfg(unix)]
    fn ok_status() -> ExitStatus {
        use std::os::unix::process::ExitStatusExt as _;
        ExitStatus::from_raw(0)
    }

    #[cfg(unix)]
    #[test]
    fn classifies_enospc_edquot_and_eio_and_renders_home_environment_notices() {
        assert_eq!(
            classify_io_error(&std::io::Error::from_raw_os_error(libc::ENOSPC)),
            Some(DiskExhaustionCause::NoSpaceLeft)
        );
        assert_eq!(
            classify_io_error(&std::io::Error::from_raw_os_error(libc::EDQUOT)),
            Some(DiskExhaustionCause::QuotaExceeded)
        );
        assert_eq!(
            classify_io_error(&std::io::Error::from_raw_os_error(libc::EIO)),
            Some(DiskExhaustionCause::RuntimeIoError)
        );
        assert_eq!(
            classify_error_text("database or disk is full (SQLITE_FULL)"),
            Some(DiskExhaustionCause::NoSpaceLeft)
        );

        let tool_err = format_disk_exhausted_tool_error(
            DiskExhaustionCause::NoSpaceLeft,
            "failed to write output file",
        );
        assert!(tool_err.contains("home environment is out of disk space"));
        assert!(tool_err.contains("no space left on device"));

        let alert = format_disk_exhausted_alert(DiskExhaustionCause::QuotaExceeded);
        assert!(alert.starts_with("<system-reminder>"));
        assert!(alert.contains("home environment is out of disk space"));
        assert!(alert.contains("disk quota exceeded"));
    }

    #[tokio::test]
    async fn daemon_survives_exhausted_transcript_and_output_writes_and_recovers_when_space_returns()
     {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = MayConfig::default();
        config.upsert_provider(
            "openai",
            crate::config::ProviderInstance::new(crate::config::ProviderType::OpenAI)
                .with_api_key("sk-test"),
        );
        config.models.advanced = vec!["openai::gpt-4o".parse().expect("selector")];
        let state = Arc::new(
            DaemonState::for_test(dir.path(), config)
                .await
                .expect("daemon state"),
        );
        let db = state.db();
        let profile_id = db.ensure_default_profiles().await.expect("profiles");
        db.create_session_in_profile(
            "s-disk",
            Some("Disk survival"),
            Some("openai::gpt-4o"),
            Some(&profile_id),
        )
        .await
        .expect("session");
        let fence = RunFence {
            session_id: "s-disk".to_string(),
            run_id: "run-disk-1".to_string(),
        };
        assert!(db.claim_session_run(&fence).await.expect("claim"));

        // Simulate OutputStore failing on ENOSPC during a tool call.
        let tool_err =
            ToolResult::error("execution failed: I/O error: No space left on device (os error 28)");
        let rendered = tool_err.render_for_model();
        let cause = classify_error_text(&rendered).expect("ENOSPC classified");
        state.pending_transcripts.arm_alert("s-disk", cause);

        // Inject SQLITE_FULL / ENOSPC on all transcript, checkpoint, and stats writes.
        db.run_gates
            .fail_transcript_disk_full
            .store(true, Ordering::SeqCst);

        // Daemon reads & health remain responsive while disk is full.
        assert!(
            db.get_session("s-disk")
                .await
                .expect("session read")
                .is_some()
        );

        // Resolving the next turn's context while disk is STILL full must NOT fail:
        // it injects the home-disk-exhausted <system-reminder> to the agent and defers
        // the user message into `pending_transcripts`.
        let req = ChatRequest {
            session_id: "s-disk".to_string(),
            content: "free up tmp files".to_string(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            delivery_mode: PendingUserMessageMode::Steer,
            pending_entry_id: None,
        };
        let ctx = crate::chat::resolve_session_context(&state, &req, &fence, true, true, None)
            .await
            .expect("turn preparation must survive SQLITE_FULL / ENOSPC");
        let hidden = ctx
            .hidden_context_xml
            .expect("agent must be told the home environment is out of disk space");
        assert!(
            hidden.contains("home environment is out of disk space"),
            "unexpected hidden context: {hidden}"
        );

        // Defer assistant reply, checkpoint, and stats while disk remains full.
        state.pending_transcripts.defer_messages(
            "s-disk",
            vec![ChatMessage::assistant(
                "Cleaned workspace scratch files.",
                jiff::Timestamp::now(),
            )],
            DiskExhaustionCause::NoSpaceLeft,
        );
        let mut agent_ctx = Context::new();
        agent_ctx.push(LlmMessage::user("free up tmp files"));
        agent_ctx.push(LlmMessage::assistant("Cleaned workspace scratch files."));
        let checkpoint = AgentCheckpoint {
            context: agent_ctx.checkpoint(),
            tool_surface_hash: "hash-1".to_string(),
            context_window: ContextWindowSnapshot {
                phase: ContextWindowPhase::Stable,
                metrics: ContextWindowMetrics {
                    usage_fraction: 0.1,
                    context_window: 128_000,
                    estimated_tokens: 32,
                    chars_per_token: 4.0,
                    system_block_count: 0,
                    recent_message_count: 2,
                    recent_system_message_count: 0,
                    has_handoff: false,
                    has_staged_system_updates: false,
                    prompt_cache_epoch: 0,
                },
                messages: Vec::new(),
            },
        };
        state.pending_transcripts.defer_checkpoint(
            "s-disk",
            checkpoint,
            DiskExhaustionCause::NoSpaceLeft,
        );
        state.pending_transcripts.defer_stats(
            "s-disk",
            MessageStatsParams {
                provider_id: Some("openai".to_string()),
                prompt_tokens: Some(20),
                completion_tokens: Some(12),
                total_tokens: Some(32),
                cache_read_tokens: None,
                cache_write_tokens: None,
                context_window: Some(128_000),
                elapsed_ms: Some(150),
            },
            DiskExhaustionCause::NoSpaceLeft,
        );
        assert!(state.pending_transcripts.has_pending("s-disk"));
        assert!(
            state
                .pending_transcripts
                .flush_session(&db, "s-disk")
                .await
                .is_err(),
            "while disk remains full, backlog stays queued without losing entries"
        );
        assert!(state.pending_transcripts.has_pending("s-disk"));

        // Space returns: flush succeeds and nothing was lost!
        db.run_gates
            .fail_transcript_disk_full
            .store(false, Ordering::SeqCst);
        assert_eq!(state.pending_transcripts.flush_all(&state).await, 1);
        assert!(!state.pending_transcripts.has_pending("s-disk"));

        let history = load_history_from_db(&db, "s-disk")
            .await
            .expect("history loads");
        let contents: Vec<&str> = history
            .messages()
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert!(contents.iter().any(|c| c.contains("free up tmp files")));
        assert!(
            contents
                .iter()
                .any(|c| c.contains("Cleaned workspace scratch files."))
        );
        let assistant = history
            .messages()
            .iter()
            .rfind(|m| m.content == "Cleaned workspace scratch files.")
            .expect("assistant persisted");
        assert_eq!(
            assistant.stats.as_ref().and_then(|s| s.total_tokens),
            Some(32)
        );
        let snap = db
            .get_session_context_snapshot("s-disk")
            .await
            .expect("snapshot query")
            .expect("checkpoint recovered");
        assert_eq!(snap.checkpoint.tool_surface_hash, "hash-1");
        let _ = TurnKind::Prompt;
    }

    #[derive(Clone)]
    struct MockExec {
        mode: Arc<Mutex<Result<String, String>>>,
        seen_containers: Arc<Mutex<Vec<String>>>,
    }

    impl ContainerExec for MockExec {
        fn exec(
            &self,
            container_id: &str,
            _script: &str,
            _working_dir: &str,
            _kill_rx: async_channel::Receiver<()>,
            _stdin_rx: async_channel::Receiver<Vec<u8>>,
            _lifetime: ExecLifetime,
        ) -> impl Future<Output = Result<ContainerExecOutcome, String>> + Send {
            self.seen_containers
                .lock()
                .expect("lock")
                .push(container_id.to_string());
            let mode = self.mode.lock().expect("lock").clone();
            std::future::ready(match mode {
                Ok(stdout) => Ok(ContainerExecOutcome::Completed {
                    #[cfg(unix)]
                    output: Output {
                        status: ok_status(),
                        stdout: stdout.into_bytes(),
                        stderr: Vec::new(),
                    },
                    #[cfg(not(unix))]
                    output: panic!("unix test"),
                    killed: Vec::new(),
                }),
                Err(err) => Err(err),
            })
        }

        async fn exec_streamed(
            &self,
            container_id: &str,
            script: &str,
            working_dir: &str,
            kill_rx: async_channel::Receiver<()>,
            stdin_rx: async_channel::Receiver<Vec<u8>>,
            _stdout: async_channel::Sender<Vec<u8>>,
            lifetime: ExecLifetime,
        ) -> Result<ContainerExecOutcome, String> {
            self.exec(
                container_id,
                script,
                working_dir,
                kill_rx,
                stdin_rx,
                lifetime,
            )
            .await
        }
    }

    struct MockReconnect {
        available: Arc<AtomicBool>,
        reconnected_exec: AnyContainerExec,
        reconnect_calls: Arc<AtomicUsize>,
    }

    impl ContainerReconnect for MockReconnect {
        fn reconnect(
            &self,
        ) -> impl Future<Output = Result<(AnyContainerExec, String), String>> + Send {
            self.reconnect_calls.fetch_add(1, Ordering::SeqCst);
            if !self.available.load(Ordering::SeqCst) {
                return std::future::ready(Err(
                    "connection refused (docker.sock missing)".to_string()
                ));
            }
            std::future::ready(Ok((
                self.reconnected_exec.clone(),
                "container-recreated-v2".to_string(),
            )))
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn container_session_reports_eio_and_reconnects_when_disappeared_runtime_returns() {
        let initial_mode = Arc::new(Mutex::new(Err(
            "Input/output error (os error 5)".to_string()
        )));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let initial_exec = AnyContainerExec::new(Arc::new(MockExec {
            mode: initial_mode.clone(),
            seen_containers: seen.clone(),
        }));

        let recovered_exec = AnyContainerExec::new(Arc::new(MockExec {
            mode: Arc::new(Mutex::new(Ok("recovered-ok\n".to_string()))),
            seen_containers: seen.clone(),
        }));
        let runtime_up = Arc::new(AtomicBool::new(false));
        let reconnect_calls = Arc::new(AtomicUsize::new(0));
        let resilient = ResilientContainerExec::new(
            initial_exec,
            "container-v1".to_string(),
            MockReconnect {
                available: runtime_up.clone(),
                reconnected_exec: recovered_exec,
                reconnect_calls: reconnect_calls.clone(),
            },
        );

        let run_once = || async {
            let (_kill_tx, kill_rx) = async_channel::bounded(1);
            let (_stdin_tx, stdin_rx) = async_channel::bounded(1);
            resilient
                .exec(
                    "container-v1",
                    "echo hi",
                    "/workspace",
                    kill_rx,
                    stdin_rx,
                    ExecLifetime::Foreground,
                )
                .await
        };

        // 1. Storage EIO inside runtime surfaces as home-environment disk exhaustion.
        let eio_err = run_once().await.expect_err("EIO fails command");
        assert!(
            eio_err.contains("home environment is out of disk space"),
            "unexpected EIO message: {eio_err}"
        );

        // 2. Runtime crashes / disappears while host disk is full.
        *initial_mode.lock().expect("lock") =
            Err("error trying to connect: Connection refused".to_string());
        let down_err = run_once().await.expect_err("runtime down fails command");
        assert!(
            down_err.contains("container runtime for this session is currently unavailable"),
            "unexpected runtime-down message: {down_err}"
        );
        assert_eq!(reconnect_calls.load(Ordering::SeqCst), 1);

        // 3. Runtime comes back up: session automatically reconnects & switches to recreated container!
        runtime_up.store(true, Ordering::SeqCst);
        let outcome = run_once()
            .await
            .expect("session must reconnect and run command in recreated container");
        match outcome {
            ContainerExecOutcome::Completed { output, .. } => {
                assert_eq!(String::from_utf8_lossy(&output.stdout), "recovered-ok\n");
            }
            ContainerExecOutcome::Killed => panic!("unexpected kill"),
        }
        assert_eq!(reconnect_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            seen.lock().expect("lock").last().map(String::as_str),
            Some("container-recreated-v2")
        );
    }
}
