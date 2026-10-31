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
    cancel_tx: Sender<()>,
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

    fn live_runs(&self) -> std::sync::MutexGuard<'_, HashMap<String, LiveRun>> {
        self.live.lock().expect("session runs map is poisoned")
    }

    fn publish_runtime_change(&self) {
        self.ui_event_bus.publish(
            UiEventTopics::SESSION_RUNTIME
                | UiEventTopics::SESSION_RUNTIMES
                | UiEventTopics::PROFILE_SESSIONS,
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
            run_id: uuid::Uuid::new_v4().to_string(),
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
        match self.db.claim_session_run(&fence).await {
            Ok(true) => {}
            Ok(false) => {
                self.unregister(&fence);
                return Err(SessionBusy {
                    session_id: session_id.to_string(),
                }
                .into());
            }
            Err(error) => {
                self.unregister(&fence);
                return Err(AcquireError::Storage(error));
            }
        }
        self.publish_runtime_change();
        Ok(SessionRunGuard {
            runs: self.clone(),
            fence,
            cancel_rx,
            settled: false,
        })
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
    pub fn cancel(&self, session_id: &str) -> bool {
        self.live_runs()
            .get(session_id)
            .is_some_and(|run| run.cancel_tx.try_send(()).is_ok())
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
    pub async fn recover_after_restart(&self) -> Result<(), ChatDbError> {
        self.db.recover_session_runtimes_after_restart().await
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
    cancel_rx: Receiver<()>,
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
    pub const fn cancellation(&self) -> &Receiver<()> {
        &self.cancel_rx
    }

    /// Settle the session: resolve its waiting requests and terminal
    /// resources, move it to `Idle` and clear the run id. Returns whether
    /// the run still held the session; a superseded run leaves the session
    /// to the run that took it.
    async fn settle(&mut self, outcome: RunOutcome) -> Result<bool, ChatDbError> {
        self.settled = true;
        self.runs.unregister(&self.fence);
        let held = self.runs.db.finish_session_run(&self.fence).await?;
        if !held {
            tracing::warn!(
                session_id = %self.fence.session_id,
                ?outcome,
                "session run ended after another run took the session"
            );
        }
        self.runs.publish_runtime_change();
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
        (self.runs.spawner)(Box::pin(async move {
            match runs.db.finish_session_run(&fence).await {
                Ok(_) => runs.publish_runtime_change(),
                Err(error) => tracing::error!(
                    session_id = %fence.session_id,
                    error = %error,
                    "failed to settle a dropped session run"
                ),
            }
        }));
    }
}
