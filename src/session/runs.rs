//! Single-owner session runs, held in this process.
//!
//! [`SessionRuns::acquire`] is the one way an agent loop starts on a session:
//! the Web chat path, the follow-up dispatcher and the TUI worker (which
//! `may oneshot` also drives) all hold a [`SessionRunGuard`] while they run.
//! The daemon owns every run, so exclusion is this registry's in-memory map;
//! the guard's [`RunFence`] stamps `session_runtimes.run_id` so run-scoped
//! database commits still verify the run they belong to, and the guard
//! settles the session when the run ends.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_channel::{Receiver, Sender};

use crate::db::{ChatDb, ChatDbError, RunFence};
use crate::ui_event_bus::{UiEventBus, UiEventTopics};

/// A live run holds the session.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("session {session_id} already has a run in progress")]
pub struct SessionBusy {
    /// Session whose run is held.
    pub session_id: String,
}

/// Why a run could not be acquired.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    /// Another run holds the session.
    #[error(transparent)]
    Busy(#[from] SessionBusy),
    /// The run's claim could not be written.
    #[error("failed to acquire the session run: {0}")]
    Storage(#[from] ChatDbError),
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The run finished its work; pending messages may continue it.
    Completed,
    /// The run failed; pending messages wait for the next trigger.
    Failed,
    /// The run was stopped; pending messages wait for the next trigger.
    Cancelled,
}

/// Who asked a run to stop, carried to the run so it can say why it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// The user stopped the run (the session stop endpoint).
    User,
    /// The daemon is shutting down and draining its turns.
    DaemonRestarting,
}

/// Starts a detached background task: the settling of a dropped guard.
///
/// The default spawns on the global executor, which only works from a thread
/// inside its runtime; an entrypoint that acquires runs from elsewhere, the
/// web server's request handlers, supplies one bound to its runtime.
pub type Spawner =
    Arc<dyn Fn(std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>) + Send + Sync>;

fn global_spawner() -> Spawner {
    Arc::new(|task| executor_core::spawn(task).detach())
}

/// Process-local view of a live guard.
struct LiveRun {
    fence: RunFence,
    cancel_tx: Sender<CancelReason>,
}

/// Acquires, cancels and inspects session runs.
///
/// Clones share the registry of live guards, so the hooks of a running agent
/// find the fence of the run they belong to.
#[derive(Clone)]
pub struct SessionRuns {
    db: ChatDb,
    ui_event_bus: UiEventBus,
    spawner: Spawner,
    live: Arc<Mutex<HashMap<String, LiveRun>>>,
}

impl std::fmt::Debug for SessionRuns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRuns").finish_non_exhaustive()
    }
}

impl SessionRuns {
    /// Runs over `db`, publishing state changes on `ui_event_bus`.
    #[must_use]
    pub fn new(db: ChatDb, ui_event_bus: UiEventBus) -> Self {
        Self {
            db,
            ui_event_bus,
            spawner: global_spawner(),
            live: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Spawn background tasks with `spawner` instead of the global executor.
    #[must_use]
    pub fn with_spawner(mut self, spawner: Spawner) -> Self {
        self.spawner = spawner;
        self
    }

    /// Records a request that only refreshed `entry`'s session's prompt
    /// cache with the session's usage, marked as such.
    pub(crate) async fn record_keep_warm_request(
        &self,
        entry: crate::daemon::types::RequestLogEntry,
    ) {
        let logged = match self.db.append_request_log(entry.as_params()).await {
            Ok(logged) => logged,
            Err(error) => {
                tracing::warn!(%error, "failed to persist a keep-warm request");
                return;
            }
        };
        if let Err(error) = self.db.mark_request_keep_warm(&logged).await {
            tracing::warn!(%error, "failed to mark a keep-warm request");
        }
        self.ui_event_bus.publish(UiEventTopics::REQUEST_LOGS);
    }

    fn live_runs(&self) -> std::sync::MutexGuard<'_, HashMap<String, LiveRun>> {
        self.live.lock().expect("session runs map is poisoned")
    }

    fn publish_runtime_change(&self, session_id: &str) {
        self.ui_event_bus
            .publish(UiEventTopics::SESSION_RUNTIMES | UiEventTopics::PROFILE_SESSIONS);
        self.ui_event_bus.publish_session_change(
            session_id,
            crate::daemon::session_events::SessionChange::Runtime,
        );
    }

    /// Take the session's run and mark it `Running`.
    ///
    /// # Errors
    /// Returns [`AcquireError::Busy`] while a live run holds the session and
    /// [`AcquireError::Storage`] when the run's claim cannot be written.
    pub async fn acquire(&self, session_id: &str) -> Result<SessionRunGuard, AcquireError> {
        let fence = RunFence {
            session_id: session_id.to_string(),
            run_id: crate::id::new(),
        };
        let (cancel_tx, cancel_rx) = async_channel::bounded(1);
        {
            let mut live = self.live_runs();
            if live.contains_key(session_id) {
                return Err(SessionBusy {
                    session_id: session_id.to_string(),
                }
                .into());
            }
            live.insert(
                session_id.to_string(),
                LiveRun {
                    fence: fence.clone(),
                    cancel_tx,
                },
            );
        }
        // The guard owns the registry entry from here on: a drop while the
        // claim is awaited runs its cleanup, whose fenced finish releases a
        // claim that landed and touches nothing for one that did not.
        let (released_tx, released_rx) = async_channel::bounded(1);
        let mut guard = SessionRunGuard {
            runs: self.clone(),
            fence,
            cancel_rx,
            released_tx: Some(released_tx),
            released_rx,
            settled: false,
        };
        match self.db.claim_session_run(&guard.fence).await {
            Ok(true) => {}
            Ok(false) => {
                guard.disarm();
                return Err(SessionBusy {
                    session_id: session_id.to_string(),
                }
                .into());
            }
            Err(error) => {
                guard.disarm();
                return Err(AcquireError::Storage(error));
            }
        }
        self.publish_runtime_change(&guard.fence.session_id);
        Ok(guard)
    }

    fn unregister(&self, fence: &RunFence) {
        let mut live = self.live_runs();
        if live
            .get(&fence.session_id)
            .is_some_and(|run| run.fence == *fence)
        {
            live.remove(&fence.session_id);
        }
    }

    /// Number of runs currently held.
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.live_runs().len()
    }

    /// Fence of the run currently holding `session_id`.
    #[must_use]
    pub fn live_fence(&self, session_id: &str) -> Option<RunFence> {
        self.live_runs()
            .get(session_id)
            .map(|run| run.fence.clone())
    }

    /// Stop the run holding `session_id`. Returns whether one was signalled.
    #[must_use]
    pub fn cancel(&self, session_id: &str, reason: CancelReason) -> bool {
        self.live_runs()
            .get(session_id)
            .is_some_and(|run| run.cancel_tx.try_send(reason).is_ok())
    }

    /// Sessions currently held by a live run.
    #[must_use]
    pub fn live_session_ids(&self) -> Vec<String> {
        self.live_runs().keys().cloned().collect()
    }

    /// Whether a live run holds the session.
    #[must_use]
    pub fn is_held(&self, session_id: &str) -> bool {
        self.live_runs().contains_key(session_id)
    }

    /// Repair persisted runtime state after a daemon restart; see
    /// [`ChatDb::recover_session_runtimes_after_restart`]. A run cannot
    /// outlive its process, so every session still marked owned is settled.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn recover_after_restart(
        &self,
        previous: crate::db::PreviousDaemon,
    ) -> Result<(), ChatDbError> {
        self.db
            .recover_session_runtimes_after_restart(previous)
            .await
    }
}

/// Fires once the run that held a session has released it: its entry is gone
/// from the registry and its database claim is finished.
#[derive(Debug, Clone)]
pub struct RunReleased(Receiver<()>);

impl RunReleased {
    /// A signal that has already fired, for a session nothing holds.
    #[must_use]
    pub fn already_released() -> Self {
        let (tx, rx) = async_channel::bounded(1);
        drop(tx);
        Self(rx)
    }

    /// Waits until the session is released; returns at once when it was.
    pub async fn wait(&self) {
        // Nothing is ever sent: the sender closing is the signal.
        let _ = self.0.recv().await;
    }
}

/// A run's hold on its session.
///
/// Dropping a guard that was not [finished](Self::finish) (a panic or an
/// early return) settles the session in a spawned task, so the session is
/// never left busy.
#[must_use = "dropping the guard ends the run; hold it for the whole run"]
pub struct SessionRunGuard {
    runs: SessionRuns,
    fence: RunFence,
    cancel_rx: Receiver<CancelReason>,
    /// Dropped, which closes the channel behind [`RunReleased`], once the
    /// session is released.
    released_tx: Option<Sender<()>>,
    released_rx: Receiver<()>,
    settled: bool,
}

impl std::fmt::Debug for SessionRunGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRunGuard")
            .field("fence", &self.fence)
            .finish_non_exhaustive()
    }
}

impl SessionRunGuard {
    /// Session this run holds.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.fence.session_id
    }

    /// Fence for the run's database commits.
    #[must_use]
    pub const fn fence(&self) -> &RunFence {
        &self.fence
    }

    /// Receives a message when the run must stop. The run drops its work and
    /// finishes the guard.
    #[must_use]
    pub const fn cancellation(&self) -> &Receiver<CancelReason> {
        &self.cancel_rx
    }

    /// Signal that fires once the run has released the session: at the end
    /// of the settle `finish` / `finish_idle` run, or of the cleanup a drop
    /// runs.
    #[must_use]
    pub fn released(&self) -> RunReleased {
        RunReleased(self.released_rx.clone())
    }

    /// Release the registry entry of a claim that did not land; nothing is
    /// left to settle.
    fn disarm(&mut self) {
        self.settled = true;
        self.runs.unregister(&self.fence);
    }

    /// Settle the session: resolve its waiting requests and terminal
    /// resources, move it to `Idle` and clear the run id. Returns whether
    /// the run still held the session; a superseded run leaves the session
    /// to the run that took it.
    async fn settle(&mut self, outcome: RunOutcome) -> Result<bool, ChatDbError> {
        // `settled` is set only once the finish returned, so a settle
        // dropped at the await falls through to `Drop`, which repeats the
        // fenced, idempotent finish. A storage error leaves the session to
        // restart recovery.
        self.runs.unregister(&self.fence);
        let held = match self.runs.db.finish_session_run(&self.fence).await {
            Ok(held) => held,
            Err(error) => {
                self.settled = true;
                self.released_tx = None;
                return Err(error);
            }
        };
        self.settled = true;
        self.released_tx = None;
        if !held {
            tracing::warn!(
                session_id = %self.fence.session_id,
                ?outcome,
                "session run ended after another run took the session"
            );
        }
        self.runs.publish_runtime_change(&self.fence.session_id);
        Ok(held)
    }

    /// End the run: settle the session, then look for pending work and take
    /// the run again to continue it. Returns the guard of the continuing
    /// run, `None` when nothing is dispatchable or a run that started
    /// meanwhile will dispatch it when it ends.
    ///
    /// Only a [`RunOutcome::Completed`] run continues.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure; the session is then left
    /// to restart recovery.
    pub async fn finish(mut self, outcome: RunOutcome) -> Result<Option<Self>, ChatDbError> {
        let held = self.settle(outcome).await?;
        if outcome != RunOutcome::Completed || !held {
            return Ok(None);
        }
        let session_id = self.fence.session_id.clone();
        let runs = self.runs.clone();
        // The release happens before this check, so a message enqueued while
        // this run held the session and found it busy is seen here.
        let runtime = runs
            .db
            .get_session_runtime(&session_id)
            .await?
            .unwrap_or_default();
        if !runtime.can_dispatch_follow_up() {
            return Ok(None);
        }
        match runs.acquire(&session_id).await {
            Ok(next) => Ok(Some(next)),
            Err(AcquireError::Busy(_)) => Ok(None),
            Err(AcquireError::Storage(error)) => Err(error),
        }
    }

    /// End the run like [`finish`](Self::finish) but never continue it: for
    /// an entrypoint whose own loop dispatches pending work.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn finish_idle(mut self, outcome: RunOutcome) -> Result<(), ChatDbError> {
        self.settle(outcome).await?;
        Ok(())
    }
}

impl Drop for SessionRunGuard {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        self.runs.unregister(&self.fence);
        tracing::warn!(
            session_id = %self.fence.session_id,
            "session run guard dropped without finishing; settling the session"
        );
        let runs = self.runs.clone();
        let fence = self.fence.clone();
        let released_tx = self.released_tx.take();
        (self.runs.spawner)(Box::pin(async move {
            let _released_tx = released_tx;
            match runs.db.finish_session_run(&fence).await {
                Ok(_) => runs.publish_runtime_change(&fence.session_id),
                Err(error) => tracing::error!(
                    session_id = %fence.session_id,
                    error = %error,
                    "failed to settle a dropped session run"
                ),
            }
        }));
    }
}
