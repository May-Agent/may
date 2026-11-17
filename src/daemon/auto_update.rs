//! Background self-update, for installed builds only: the worker starts on a
//! release or CI build and never on a local one (`BuildKind::auto_updates`),
//! so a `cargo build` binary is not overwritten behind its developer.
//!
//! While `[update] auto` is on, the daemon checks its channel every
//! `[update] check_interval`, installs a newer build over its executable and
//! restarts onto it once it is idle: no running turn, no pending
//! interaction and no running background job. The restart is the graceful
//! one every restart takes (`lifecycle::spawn_restart`). The first check
//! comes one interval after the daemon started, so a short-lived daemon never
//! checks.
//!
//! The settings are read on every round, so a change applies at once; the
//! worker wakes on a short tick so neither a new interval nor a session
//! turning idle waits for the previous interval to run out.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::update::{BuildKind, check, running_executable};

use super::DaemonPaths;
use super::lifecycle;
use super::state::DaemonState;

/// How often the worker looks at the settings and, with an update waiting,
/// at whether the daemon is idle.
const TICK: Duration = Duration::from_secs(30);

/// What the daemon is busy with, for deciding whether it may restart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Activity {
    /// Turns running.
    turns: usize,
    /// Permission, workspace and `ask_user` requests waiting for an answer.
    pending_interactions: usize,
    /// Sessions with a running background job or subagent run.
    background_sessions: usize,
}

impl Activity {
    /// Whether nothing is running, waiting or in the background.
    pub(super) const fn is_idle(self) -> bool {
        self.turns == 0 && self.pending_interactions == 0 && self.background_sessions == 0
    }

    /// What the daemon is busy with now; `None` when the database cannot be
    /// read, which is no evidence of idleness.
    pub(super) async fn of(state: &DaemonState) -> Option<Self> {
        let sessions = state.db().list_sessions().await.ok()?;
        let session_ids: Vec<String> = sessions.into_iter().map(|session| session.id).collect();
        let summaries = state
            .db()
            .list_session_runtime_summaries(&session_ids)
            .await
            .ok()?;
        let pending_interactions = summaries
            .values()
            .map(|summary| {
                usize::from(summary.waiting_permission.is_some())
                    + usize::from(summary.waiting_workspace.is_some())
                    + usize::from(summary.waiting_ask_user.is_some())
            })
            .sum();
        Some(Self {
            turns: state.runtime_service.runs().live_count(),
            pending_interactions,
            background_sessions: state.session_background.busy_sessions().await.len(),
        })
    }
}

/// Whether a check is due once `elapsed` passed since the last one, or, for
/// the first check, since the daemon started: a daemon checks on the
/// interval, and one just started has run for less than one.
fn check_due(elapsed: Duration, interval: Duration) -> bool {
    elapsed >= interval
}

/// Starts the worker of the running build's kind.
pub(super) fn spawn(state: Arc<DaemonState>) {
    spawn_for(BuildKind::current(), state);
}

/// Starts the worker when a build of `kind` updates itself; the handle of
/// the worker, `None` when it does not start.
fn spawn_for(kind: BuildKind, state: Arc<DaemonState>) -> Option<tokio::task::JoinHandle<()>> {
    if !kind.auto_updates() {
        tracing::debug!(?kind, "automatic update is off: not an installed build");
        return None;
    }
    // Read before any update replaces it: on Linux a replaced executable no
    // longer resolves by its own path.
    let executable = match running_executable() {
        Ok(executable) => executable,
        Err(error) => {
            tracing::warn!(%error, "automatic update is off: the daemon's executable is unknown");
            return None;
        }
    };
    let mut worker = Worker {
        paths: DaemonPaths::new(state.may_dir.clone()),
        state,
        executable,
        last_check: Instant::now(),
    };
    let handle = worker.state.tokio_handle.clone();
    Some(handle.spawn(async move {
        loop {
            crate::panic_boundary::round("auto update", worker.round()).await;
            async_io::Timer::after(TICK).await;
        }
    }))
}

struct Worker {
    state: Arc<DaemonState>,
    paths: DaemonPaths,
    /// The executable the daemon runs, resolved when it started.
    executable: PathBuf,
    /// When the last check ran; the daemon's start before the first one.
    last_check: Instant,
}

impl Worker {
    async fn round(&mut self) {
        let settings = self.state.config_snapshot().update;
        if !settings.auto {
            return;
        }
        if !self.update_installed() {
            if check_due(self.last_check.elapsed(), settings.check_interval) {
                self.last_check = Instant::now();
                self.install(settings.channel()).await;
            }
            if !self.update_installed() {
                return;
            }
        }
        self.restart_when_idle().await;
    }

    /// Whether the executable on disk is no longer the build this daemon
    /// runs: an update waits for its restart.
    fn update_installed(&self) -> bool {
        match super::build_id_of(&self.executable) {
            Ok(id) => id != self.state.build_id,
            Err(error) => {
                tracing::debug!(%error, "cannot read the daemon's executable");
                false
            }
        }
    }

    async fn install(&self, channel: crate::update::Channel) {
        let mut progress = |step: &str| tracing::debug!(step, "auto update");
        let available = match check(channel, &mut progress).await {
            Ok(Some(available)) => available,
            Ok(None) => {
                tracing::debug!(%channel, "this build is the newest of its channel");
                return;
            }
            // Without a GitHub login the dev channel cannot look.
            Err(error @ crate::update::UpdateError::Token(_)) => {
                tracing::warn!(%error, %channel, "automatic update check skipped");
                return;
            }
            Err(error) => {
                tracing::warn!(%error, %channel, "automatic update check failed");
                return;
            }
        };
        let to = available.to().to_owned();
        match available.install(&self.executable, &mut progress).await {
            Ok(()) => tracing::info!(%to, %channel, "installed an update; restarting once idle"),
            Err(error) => tracing::warn!(%error, %to, "automatic update failed"),
        }
    }

    async fn restart_when_idle(&self) {
        // A restart already drains the daemon.
        if self
            .state
            .draining
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        match Activity::of(&self.state).await {
            Some(activity) if activity.is_idle() => {}
            Some(activity) => {
                tracing::debug!(?activity, "an update waits for the daemon to be idle");
                return;
            }
            None => return,
        }
        match lifecycle::spawn_restart(&self.paths, &self.executable) {
            Ok(()) => tracing::info!("the daemon is idle; restarting onto the installed update"),
            Err(error) => tracing::warn!(%error, "could not start the restart onto the update"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MayConfig;

    #[test]
    fn a_check_is_due_only_once_a_whole_interval_passed() {
        let hour = Duration::from_hours(1);
        // The first check counts from the daemon's start, so a daemon that
        // just started does not check.
        assert!(!check_due(Duration::ZERO, hour));
        assert!(!check_due(Duration::from_mins(59), hour));
        assert!(check_due(hour, hour));
        // A shorter interval, applied live, makes the same elapsed time due.
        assert!(check_due(Duration::from_mins(59), Duration::from_mins(30)));
    }

    #[test]
    fn the_daemon_is_idle_only_when_nothing_is_running_waiting_or_in_the_background() {
        assert!(Activity::default().is_idle());
        for busy in [
            Activity {
                turns: 1,
                ..Activity::default()
            },
            Activity {
                pending_interactions: 1,
                ..Activity::default()
            },
            Activity {
                background_sessions: 2,
                ..Activity::default()
            },
        ] {
            assert!(!busy.is_idle(), "{busy:?}");
        }
    }

    #[tokio::test]
    async fn the_worker_starts_only_for_installed_builds() {
        let dir = tempfile::tempdir().expect("a temp dir");
        // `auto` off, so a worker that started has nothing to do.
        let mut config = MayConfig::default();
        config.update.auto = false;
        let state = Arc::new(
            DaemonState::for_test(dir.path(), config)
                .await
                .expect("a daemon state"),
        );
        for (kind, starts) in [
            (BuildKind::Release, true),
            (BuildKind::Ci, true),
            (BuildKind::Local, false),
        ] {
            let worker = spawn_for(kind, state.clone());
            assert_eq!(worker.is_some(), starts, "{kind:?}");
            if let Some(worker) = worker {
                worker.abort();
            }
        }
    }

    #[tokio::test]
    async fn a_daemon_with_no_turn_is_idle_and_one_with_a_turn_is_not() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let state = DaemonState::for_test(dir.path(), MayConfig::default())
            .await
            .expect("a daemon state");
        let activity = Activity::of(&state).await.expect("the database reads");
        assert!(activity.is_idle(), "{activity:?}");

        let run = state
            .runtime_service
            .runs()
            .acquire("session")
            .await
            .expect("a run");
        let activity = Activity::of(&state).await.expect("the database reads");
        assert_eq!(activity.turns, 1);
        assert!(!activity.is_idle());
        drop(run);
    }
}
