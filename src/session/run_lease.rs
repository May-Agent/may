//! Single-owner session runs.
//!
//! [`SessionRuns::acquire`] is the one way an agent loop starts on a session:
//! the Web chat path, the follow-up dispatcher and the TUI worker (which
//! `may oneshot` also drives) all hold a [`SessionRunGuard`] while they run.
//! The guard owns the run's fenced lease: it renews the heartbeat, surfaces
//! cancellation and lease loss, hands out the [`RunFence`] that run-scoped
//! database commits verify, and settles the session when the run ends.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use async_channel::{Receiver, Sender};

use crate::db::{ChatDb, ChatDbError, RunFence};
use crate::ui_event_bus::{UiEventBus, UiEventTopics};

/// How long a lease survives without a heartbeat before another run may take
/// it over. The holder renews every third of it.
pub const DEFAULT_RUN_LEASE_TTL: Duration = Duration::from_secs(30);

/// A live run holds the session.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("session {session_id} already has a run in progress")]
pub struct SessionBusy {
    /// Session whose lease is held.
    pub session_id: String,
}

/// Why a run could not be acquired.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    /// Another run holds the session.
    #[error(transparent)]
    Busy(#[from] SessionBusy),
    /// The lease could not be read or written.
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

/// Starts a detached background task: the heartbeat of a run, or the
/// settling of a dropped guard.
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
#[derive(Clone)]
struct LiveRun {
    fence: RunFence,
    cancel_tx: Sender<()>,
    lost: Arc<AtomicBool>,
}

/// Acquires, cancels and inspects session run leases.
///
/// Clones share the registry of this process's live guards, so the hooks of
/// a running agent find the fence of the run they belong to.
#[derive(Clone)]
pub struct SessionRuns {
    db: ChatDb,
    ui_event_bus: UiEventBus,
    ttl: Duration,
    spawner: Spawner,
    live: Arc<ArcSwap<HashMap<String, LiveRun>>>,
}

impl std::fmt::Debug for SessionRuns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRuns")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

impl SessionRuns {
    /// Runs over `db` with the default lease TTL, publishing state changes
    /// on `ui_event_bus`.
    #[must_use]
    pub fn new(db: ChatDb, ui_event_bus: UiEventBus) -> Self {
        Self {
            db,
            ui_event_bus,
            ttl: DEFAULT_RUN_LEASE_TTL,
            spawner: global_spawner(),
            live: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        }
    }

    /// Use `ttl` as the lease lifetime; the heartbeat interval follows.
    #[must_use]
    pub const fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Spawn background tasks with `spawner` instead of the global executor.
    #[must_use]
    pub fn with_spawner(mut self, spawner: Spawner) -> Self {
        self.spawner = spawner;
        self
    }

    fn heartbeat_interval(&self) -> Duration {
        self.ttl / 3
    }

    fn expired_before_ms(&self) -> i64 {
        let ttl_ms = i64::try_from(self.ttl.as_millis()).expect("lease TTL fits in milliseconds");
        now_ms() - ttl_ms
    }

    fn publish_runtime_change(&self) {
        self.ui_event_bus.publish(
            UiEventTopics::SESSION_RUNTIME
                | UiEventTopics::SESSION_RUNTIMES
                | UiEventTopics::PROFILE_SESSIONS,
        );
    }

    /// Take the session's run lease and mark it `Running`.
    ///
    /// # Errors
    /// Returns [`AcquireError::Busy`] while a live run holds the session and
    /// [`AcquireError::Storage`] when the lease cannot be written.
    pub async fn acquire(&self, session_id: &str) -> Result<SessionRunGuard, AcquireError> {
        let owner = uuid::Uuid::new_v4().to_string();
        let fence = self
            .db
            .acquire_session_run_lease(session_id, &owner, now_ms(), self.expired_before_ms())
            .await?
            .ok_or_else(|| SessionBusy {
                session_id: session_id.to_string(),
            })?;
        let (cancel_tx, cancel_rx) = async_channel::bounded(1);
        let lost = Arc::new(AtomicBool::new(false));
        self.register(&LiveRun {
            fence: fence.clone(),
            cancel_tx: cancel_tx.clone(),
            lost: lost.clone(),
        });
        self.publish_runtime_change();
        let (stop_tx, stop_rx) = async_channel::bounded(1);
        (self.spawner)(Box::pin(heartbeat(
            self.clone(),
            fence.clone(),
            cancel_tx,
            lost.clone(),
            stop_rx,
        )));
        Ok(SessionRunGuard {
            runs: self.clone(),
            fence,
            cancel_rx,
            lost,
            heartbeat_stop: Some(stop_tx),
            settled: false,
        })
    }

    fn register(&self, run: &LiveRun) {
        self.live.rcu(|live| {
            let mut next = HashMap::clone(live);
            next.insert(run.fence.session_id.clone(), run.clone());
            next
        });
    }

    fn unregister(&self, fence: &RunFence) {
        self.live.rcu(|live| {
            let mut next = HashMap::clone(live);
            if next
                .get(&fence.session_id)
                .is_some_and(|run| run.fence == *fence)
            {
                next.remove(&fence.session_id);
            }
            next
        });
    }

    /// Fence of the run this process currently holds on `session_id`.
    #[must_use]
    pub fn live_fence(&self, session_id: &str) -> Option<RunFence> {
        self.live
            .load()
            .get(session_id)
            .map(|run| run.fence.clone())
    }

    /// Whether this process's run on `session_id` lost its lease.
    #[must_use]
    pub fn is_lost(&self, session_id: &str) -> bool {
        self.live
            .load()
            .get(session_id)
            .is_some_and(|run| run.lost.load(Ordering::Acquire))
    }

    /// Stop the run of `session_id` in this process, if any. Returns whether
    /// one was signalled.
    #[must_use]
    pub fn cancel_local(&self, session_id: &str) -> bool {
        self.live
            .load()
            .get(session_id)
            .is_some_and(|run| run.cancel_tx.try_send(()).is_ok())
    }

    /// Ask the run holding `session_id` to stop, wherever it lives: the
    /// request is recorded on the lease for a holder in another process and
    /// signalled directly to one in this process. Returns whether a run was
    /// reached.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn cancel(&self, session_id: &str) -> Result<bool, ChatDbError> {
        let recorded = self.db.request_session_run_cancel(session_id).await?;
        Ok(self.cancel_local(session_id) || recorded)
    }

    /// Whether a live run holds the session's lease.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn is_held(&self, session_id: &str) -> Result<bool, ChatDbError> {
        self.db
            .session_run_lease_held(session_id, self.expired_before_ms())
            .await
    }

    /// Repair persisted runtime state after a server restart; see
    /// [`ChatDb::recover_session_runtimes_after_restart`]. Only leases whose
    /// heartbeat expired are reclaimed.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn recover_after_restart(&self) -> Result<(), ChatDbError> {
        self.db
            .recover_session_runtimes_after_restart(self.expired_before_ms())
            .await
    }
}

/// Renews the lease every `ttl / 3` until it is lost or `stop_rx` closes,
/// which is when the guard ends the run.
async fn heartbeat(
    runs: SessionRuns,
    fence: RunFence,
    cancel_tx: Sender<()>,
    lost: Arc<AtomicBool>,
    stop_rx: Receiver<()>,
) {
    let mut cancellation_signalled = false;
    loop {
        let due = futures_lite::future::or(
            async {
                async_io::Timer::after(runs.heartbeat_interval()).await;
                true
            },
            async {
                let _ = stop_rx.recv().await;
                false
            },
        )
        .await;
        if !due {
            return;
        }
        match runs.db.renew_session_run_lease(&fence, now_ms()).await {
            Ok(Some(requested)) => {
                if requested && !cancellation_signalled {
                    let _ = cancel_tx.try_send(());
                }
                cancellation_signalled = requested;
            }
            // A renewal in flight when the guard finished finds the lease
            // released, which is not a loss.
            Ok(None) if stop_rx.is_closed() => return,
            Ok(None) => {
                tracing::error!(
                    session_id = %fence.session_id,
                    generation = fence.generation,
                    "session run lease lost; cancelling the run"
                );
                lost.store(true, Ordering::Release);
                let _ = cancel_tx.try_send(());
                return;
            }
            // A failed renewal is retried on the next tick; if storage stays
            // unreachable past the TTL another run may take the lease, which
            // the next successful renewal reports as lost.
            Err(error) => tracing::warn!(
                session_id = %fence.session_id,
                error = %error,
                "failed to renew the session run lease"
            ),
        }
    }
}

/// A run's hold on its session.
///
/// Dropping a guard that was not [finished](Self::finish) (a panic or an
/// early return) stops the heartbeat and settles the session in a spawned
/// task, so the session is never left busy.
#[must_use = "dropping the guard ends the run; hold it for the whole run"]
pub struct SessionRunGuard {
    runs: SessionRuns,
    fence: RunFence,
    cancel_rx: Receiver<()>,
    lost: Arc<AtomicBool>,
    heartbeat_stop: Option<Sender<()>>,
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

    /// Receives a message when the run must stop: a stop request or loss of
    /// the lease. The run drops its work and finishes the guard.
    #[must_use]
    pub const fn cancellation(&self) -> &Receiver<()> {
        &self.cancel_rx
    }

    /// Whether the lease was taken over by another run.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    fn stop_heartbeat(&mut self) {
        self.heartbeat_stop = None;
        self.runs.unregister(&self.fence);
    }

    /// Settle the session: resolve its waiting requests and terminal
    /// resources, move it to `Idle` and release the lease. Returns whether
    /// the run still held the lease; a run that lost it leaves the session
    /// to the new holder.
    async fn settle(&mut self, outcome: RunOutcome) -> Result<bool, ChatDbError> {
        self.stop_heartbeat();
        self.settled = true;
        let held = self.runs.db.finish_session_run(&self.fence).await?;
        if !held {
            tracing::warn!(
                session_id = %self.fence.session_id,
                ?outcome,
                "session run ended after losing its lease"
            );
        }
        self.runs.publish_runtime_change();
        Ok(held)
    }

    /// End the run: settle the session, release the lease, then look for
    /// pending work and take the lease again to continue it. Returns the
    /// guard of the continuing run, `None` when nothing is dispatchable or a
    /// run that started meanwhile will dispatch it when it ends.
    ///
    /// Only a [`RunOutcome::Completed`] run continues.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure; the session is then left
    /// to lease expiry and restart recovery.
    pub async fn finish(mut self, outcome: RunOutcome) -> Result<Option<Self>, ChatDbError> {
        let held = self.settle(outcome).await?;
        if outcome != RunOutcome::Completed || !held {
            return Ok(None);
        }
        let session_id = self.fence.session_id.clone();
        let runs = self.runs.clone();
        // The release happens before this check, so a message enqueued while
        // this run held the lease and found it busy is seen here.
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
        self.stop_heartbeat();
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
