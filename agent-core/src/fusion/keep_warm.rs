//! Unified Fusion prompt-cache `keep_warm` predicate and pair state registry.
//!
//! Both the lead and companion sidekick evaluate the same predicate:
//! refresh caches if and only if
//! 1. either agent has a background shell command in flight,
//! 2. either agent has an active turn not paused on an approval past the
//!    configured grace window (`[experimental] fusion_keep_warm_grace`, default 15m), or
//! 3. the pair is idle with an open `ask_user` request younger than that grace window.
//!
//! As soon as the predicate turns false, `keep_warm` stops (`refresh_in = None`) so
//! `idle_compaction_due_in` runs `compact_when_idle` while the cache is still warm.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aither_sandbox::{JobStatus, Jobs};
use arc_swap::ArcSwap;
use tokio::sync::Notify;

use crate::agent::keep_warm::refresh_margin;
use crate::command_policy::WorkspaceWriteLease;
use crate::db::ChatDb;
use crate::fusion::WorkspaceEditTracker;
use crate::session::ExecutionMode;

/// Planned idle keep-warm timing for one agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeepWarmPlan {
    /// How long until the next prompt-cache refresh should be sent; `None` means
    /// keep-warm is inactive and idle compaction may proceed.
    pub refresh_in: Option<Duration>,
    /// Optional earlier wake-up when a grace-bounded wait will expire and flip the
    /// predicate to `false`, allowing `next_command` to hand off to idle compaction.
    pub recheck_in: Option<Duration>,
}

/// Snapshot of a Fusion lead/sidekick pair's activity used to evaluate keep-warm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FusionPairActivity {
    /// Either lead or sidekick has a running background shell command.
    pub either_has_running_job: bool,
    /// Lead has an active turn not paused on an approval past the grace window.
    pub lead_active_within_grace: bool,
    /// Sidekick has an active turn not paused on an approval past the grace window.
    pub sidekick_active_within_grace: bool,
    /// Pair is idle and has an open `ask_user` request younger than the grace window.
    pub open_ask_user_within_grace: bool,
    /// Earliest time until a currently-within-grace wait expires (when keep-warm is
    /// solely sustained by grace-bounded waits).
    pub grace_expires_in: Option<Duration>,
}

impl FusionPairActivity {
    /// Whether the unified Fusion keep-warm predicate holds.
    #[must_use]
    pub const fn should_keep_warm(&self) -> bool {
        self.either_has_running_job
            || self.lead_active_within_grace
            || self.sidekick_active_within_grace
            || self.open_ask_user_within_grace
    }
}

/// Evaluates [`FusionPairActivity`] from pair run flags, job states, and wait timestamps.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
#[must_use]
pub fn evaluate_fusion_pair_activity(
    now_ms: i64,
    grace: Duration,
    lead_running: bool,
    sidekick_running: bool,
    lead_has_running_job: bool,
    sidekick_has_running_job: bool,
    lead_oldest_approval_ms: Option<i64>,
    sidekick_oldest_approval_ms: Option<i64>,
    open_ask_user_newest_ms: Option<i64>,
) -> FusionPairActivity {
    let grace_ms = i64::try_from(grace.as_millis()).unwrap_or(i64::MAX);
    let either_has_running_job = lead_has_running_job || sidekick_has_running_job;

    let mut unpaused_active = false;
    let mut max_grace_remaining_ms: Option<i64> = None;
    let mut record_grace = |remaining_ms: i64| {
        max_grace_remaining_ms = Some(
            max_grace_remaining_ms.map_or(remaining_ms, |existing| existing.max(remaining_ms)),
        );
    };

    let lead_active_within_grace = if lead_running {
        lead_oldest_approval_ms.map_or_else(
            || {
                unpaused_active = true;
                true
            },
            |created_ms| {
                let age = now_ms.saturating_sub(created_ms);
                if age < grace_ms {
                    record_grace(grace_ms.saturating_sub(age));
                    true
                } else {
                    false
                }
            },
        )
    } else {
        false
    };

    let sidekick_active_within_grace = if sidekick_running {
        sidekick_oldest_approval_ms.map_or_else(
            || {
                unpaused_active = true;
                true
            },
            |created_ms| {
                let age = now_ms.saturating_sub(created_ms);
                if age < grace_ms {
                    record_grace(grace_ms.saturating_sub(age));
                    true
                } else {
                    false
                }
            },
        )
    } else {
        false
    };

    let open_ask_user_within_grace = if !lead_running && !sidekick_running {
        open_ask_user_newest_ms.is_some_and(|created_ms| {
            let age = now_ms.saturating_sub(created_ms);
            if age < grace_ms {
                record_grace(grace_ms.saturating_sub(age));
                true
            } else {
                false
            }
        })
    } else {
        false
    };

    let grace_expires_in = if !either_has_running_job && !unpaused_active {
        max_grace_remaining_ms
            .map(|ms| Duration::from_millis(u64::try_from(ms.max(0)).unwrap_or(0)))
    } else {
        None
    };

    FusionPairActivity {
        either_has_running_job,
        lead_active_within_grace,
        sidekick_active_within_grace,
        open_ask_user_within_grace,
        grace_expires_in,
    }
}

/// Computes the [`KeepWarmPlan`] for a Fusion agent whose cache entry goes cold in
/// `cold_in` under cache `lifetime`.
#[must_use]
pub fn fusion_keep_warm_plan(
    cold_in: Option<Duration>,
    lifetime: Duration,
    activity: FusionPairActivity,
) -> KeepWarmPlan {
    if !activity.should_keep_warm() {
        return KeepWarmPlan::default();
    }
    let Some(cold_in) = cold_in else {
        return KeepWarmPlan::default();
    };
    let refresh_in = cold_in.saturating_sub(refresh_margin().min(lifetime / 2));
    let recheck_in = activity
        .grace_expires_in
        .filter(|recheck| *recheck < refresh_in);
    KeepWarmPlan {
        refresh_in: Some(refresh_in),
        recheck_in,
    }
}

/// Live shared state of one Fusion lead + sidekick pair.
pub struct FusionPairState {
    parent_session_id: String,
    mode: ArcSwap<ExecutionMode>,
    sidekick_session_id: Mutex<Option<String>>,
    lead_running: AtomicBool,
    sidekick_running: AtomicBool,
    lead_jobs: Mutex<Option<Jobs>>,
    sidekick_jobs: Mutex<Option<Jobs>>,
    write_lease: Mutex<Option<WorkspaceWriteLease>>,
    edit_tracker: Mutex<Option<WorkspaceEditTracker>>,
    wake: Notify,
}

impl std::fmt::Debug for FusionPairState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FusionPairState")
            .field("parent_session_id", &self.parent_session_id)
            .field("lead_running", &self.lead_running.load(Ordering::Acquire))
            .field(
                "sidekick_running",
                &self.sidekick_running.load(Ordering::Acquire),
            )
            .finish_non_exhaustive()
    }
}

/// RAII guard marking a lead or sidekick turn active and notifying parked partners on drop.
#[derive(Debug)]
pub struct ActiveTurnGuard {
    state: Arc<FusionPairState>,
    is_sidekick: bool,
}

impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        if self.is_sidekick {
            self.state.sidekick_running.store(false, Ordering::Release);
        } else {
            self.state.lead_running.store(false, Ordering::Release);
        }
        self.state.wake.notify_waiters();
    }
}

impl FusionPairState {
    fn new(parent_session_id: String) -> Self {
        Self {
            parent_session_id,
            mode: ArcSwap::from_pointee(ExecutionMode::Standard),
            sidekick_session_id: Mutex::new(None),
            lead_running: AtomicBool::new(false),
            sidekick_running: AtomicBool::new(false),
            lead_jobs: Mutex::new(None),
            sidekick_jobs: Mutex::new(None),
            write_lease: Mutex::new(None),
            edit_tracker: Mutex::new(None),
            wake: Notify::new(),
        }
    }

    /// Parent lead session id.
    #[must_use]
    pub fn parent_session_id(&self) -> &str {
        &self.parent_session_id
    }

    /// Current live [`ExecutionMode`] of the lead session.
    #[must_use]
    pub fn execution_mode(&self) -> ExecutionMode {
        (**self.mode.load()).clone()
    }

    /// Updates the live [`ExecutionMode`] and wakes parked workers to re-evaluate keep-warm.
    pub fn set_execution_mode(&self, mode: ExecutionMode) {
        let is_fusion = mode.is_fusion();
        self.mode.store(Arc::new(mode));
        if let Some(tracker) = self
            .edit_tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            tracker.set_enabled(is_fusion);
        }
        self.wake.notify_waiters();
    }

    /// Records the linked companion sidekick session id.
    pub fn set_sidekick_session_id(&self, session_id: String) {
        *self
            .sidekick_session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session_id);
        self.wake.notify_waiters();
    }

    /// Returns the linked companion sidekick session id, if known in memory.
    #[must_use]
    pub fn sidekick_session_id(&self) -> Option<String> {
        self.sidekick_session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Marks the lead turn active until the returned guard drops.
    #[must_use]
    pub fn begin_lead_turn(self: &Arc<Self>) -> ActiveTurnGuard {
        self.lead_running.store(true, Ordering::Release);
        self.wake.notify_waiters();
        ActiveTurnGuard {
            state: Arc::clone(self),
            is_sidekick: false,
        }
    }

    /// Marks the sidekick turn active until the returned guard drops.
    #[must_use]
    pub fn begin_sidekick_turn(self: &Arc<Self>) -> ActiveTurnGuard {
        self.sidekick_running.store(true, Ordering::Release);
        self.wake.notify_waiters();
        ActiveTurnGuard {
            state: Arc::clone(self),
            is_sidekick: true,
        }
    }

    /// Whether a sidekick turn is currently in flight.
    #[must_use]
    pub fn is_sidekick_running(&self) -> bool {
        self.sidekick_running.load(Ordering::Acquire)
    }

    /// Attaches the lead shell's [`Jobs`] registry.
    pub fn set_lead_jobs(&self, jobs: Jobs) {
        *self
            .lead_jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(jobs);
        self.wake.notify_waiters();
    }

    /// Attaches the sidekick shell's [`Jobs`] registry.
    pub fn set_sidekick_jobs(&self, jobs: Jobs) {
        *self
            .sidekick_jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(jobs);
        self.wake.notify_waiters();
    }

    /// Returns (creating if needed) the shared [`WorkspaceWriteLease`] for this pair.
    #[must_use]
    pub fn write_lease(&self, sandbox_dir: &Path, tasks_path: &Path) -> WorkspaceWriteLease {
        let mut guard = self
            .write_lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .get_or_insert_with(|| WorkspaceWriteLease::new(sandbox_dir, tasks_path))
            .clone()
    }

    /// Returns the existing [`WorkspaceWriteLease`] if initialized.
    #[must_use]
    pub fn existing_write_lease(&self) -> Option<WorkspaceWriteLease> {
        self.write_lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Returns (creating if needed) the shared [`WorkspaceEditTracker`] for this lead session.
    #[must_use]
    pub fn edit_tracker(&self, sandbox_dir: PathBuf, tasks_path: PathBuf) -> WorkspaceEditTracker {
        let lease = self.write_lease(&sandbox_dir, &tasks_path);
        let is_fusion = self.execution_mode().is_fusion();
        let tracker = self
            .edit_tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(|| {
                WorkspaceEditTracker::new(sandbox_dir, tasks_path, lease, is_fusion)
            })
            .clone();
        tracker.set_enabled(is_fusion);
        tracker
    }

    /// Returns the existing [`WorkspaceEditTracker`] if initialized.
    #[must_use]
    pub fn existing_edit_tracker(&self) -> Option<WorkspaceEditTracker> {
        self.edit_tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Notifies parked workers in this pair that activity changed.
    pub fn notify_changed(&self) {
        self.wake.notify_waiters();
    }

    /// Future completing when pair activity changes (turn start/end, mode change, etc.).
    pub async fn changed(&self) {
        self.wake.notified().await;
    }

    /// Evaluates the unified Fusion [`KeepWarmPlan`] against live jobs and DB wait state.
    pub async fn evaluate_plan(
        &self,
        db: &ChatDb,
        now_ms: i64,
        grace: Duration,
        cold_in: Option<Duration>,
        lifetime: Duration,
    ) -> KeepWarmPlan {
        let lead_jobs = self
            .lead_jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let sidekick_jobs = self
            .sidekick_jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let sidekick_id = self.sidekick_session_id();

        let lead_has_running_job = match lead_jobs {
            Some(jobs) => jobs
                .list()
                .await
                .iter()
                .any(|job| job.status == JobStatus::Running),
            None => false,
        };
        let sidekick_has_running_job = match sidekick_jobs {
            Some(jobs) => jobs
                .list()
                .await
                .iter()
                .any(|job| job.status == JobStatus::Running),
            None => false,
        };

        let lead_running = self.lead_running.load(Ordering::Acquire);
        let sidekick_running = self.sidekick_running.load(Ordering::Acquire);

        let lead_oldest_approval_ms = if lead_running {
            parse_ts_opt(
                db.oldest_pending_approval_created_at(&self.parent_session_id)
                    .await
                    .ok()
                    .flatten(),
            )
        } else {
            None
        };
        let sidekick_oldest_approval_ms = if sidekick_running {
            if let Some(id) = sidekick_id.as_deref() {
                parse_ts_opt(
                    db.oldest_pending_approval_created_at(id)
                        .await
                        .ok()
                        .flatten(),
                )
            } else {
                None
            }
        } else {
            None
        };
        let open_ask_user_newest_ms = if !lead_running && !sidekick_running {
            parse_ts_opt(
                db.newest_open_ask_user_created_at(&self.parent_session_id)
                    .await
                    .ok()
                    .flatten(),
            )
        } else {
            None
        };

        let activity = evaluate_fusion_pair_activity(
            now_ms,
            grace,
            lead_running,
            sidekick_running,
            lead_has_running_job,
            sidekick_has_running_job,
            lead_oldest_approval_ms,
            sidekick_oldest_approval_ms,
            open_ask_user_newest_ms,
        );
        fusion_keep_warm_plan(cold_in, lifetime, activity)
    }
}

fn parse_ts_opt(raw: Option<String>) -> Option<i64> {
    raw.and_then(|value| {
        value
            .parse::<jiff::Timestamp>()
            .ok()
            .map(jiff::Timestamp::as_millisecond)
    })
}

/// Process-wide registry mapping parent and companion sessions to their [`FusionPairState`].
#[derive(Debug, Clone, Default)]
pub struct FusionPairRegistry {
    pairs: Arc<Mutex<HashMap<String, Arc<FusionPairState>>>>,
    companion_to_parent: Arc<Mutex<HashMap<String, String>>>,
}

impl FusionPairRegistry {
    /// Returns (creating if absent) the [`FusionPairState`] for `parent_session_id`.
    #[must_use]
    pub fn for_parent(&self, parent_session_id: &str) -> Arc<FusionPairState> {
        let mut pairs = self
            .pairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            pairs
                .entry(parent_session_id.to_string())
                .or_insert_with(|| Arc::new(FusionPairState::new(parent_session_id.to_string()))),
        )
    }

    /// Associates `companion_session_id` with `parent_session_id` and returns their shared pair.
    #[must_use]
    pub fn link_companion(
        &self,
        parent_session_id: &str,
        companion_session_id: &str,
    ) -> Arc<FusionPairState> {
        let pair = self.for_parent(parent_session_id);
        pair.set_sidekick_session_id(companion_session_id.to_string());
        self.companion_to_parent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                companion_session_id.to_string(),
                parent_session_id.to_string(),
            );
        pair
    }

    /// Looks up the pair owning `session_id` (whether lead or companion).
    #[must_use]
    pub fn lookup(&self, session_id: &str) -> Option<Arc<FusionPairState>> {
        let parent_id = self
            .companion_to_parent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned();
        let key = parent_id.as_deref().unwrap_or(session_id);
        self.pairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    /// Removes pair state when a parent session is deleted.
    pub fn remove_parent(&self, parent_session_id: &str) {
        let removed = self
            .pairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(parent_session_id);
        if let Some(pair) = removed {
            if let Some(sidekick_id) = pair.sidekick_session_id() {
                self.companion_to_parent
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&sidekick_id);
            }
            pair.notify_changed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIFETIME: Duration = Duration::from_hours(1);
    const GRACE: Duration = Duration::from_mins(15);
    const DUE_59M30S: Duration = Duration::from_secs(59 * 60 + 30);

    #[test]
    fn active_background_job_on_either_agent_keeps_warm() {
        let activity = evaluate_fusion_pair_activity(
            1_000_000, GRACE, false, false, false, true, None, None, None,
        );
        assert!(activity.should_keep_warm());
        let plan = fusion_keep_warm_plan(Some(LIFETIME), LIFETIME, activity);
        assert_eq!(plan.refresh_in, Some(DUE_59M30S));
        assert_eq!(plan.recheck_in, None);
    }

    #[test]
    fn active_turn_within_and_past_approval_grace() {
        let now = 10_000_000_i64;
        // Active unpaused sidekick turn keeps lead warm.
        let unpaused =
            evaluate_fusion_pair_activity(now, GRACE, false, true, false, false, None, None, None);
        assert!(unpaused.should_keep_warm());
        assert_eq!(
            fusion_keep_warm_plan(Some(LIFETIME), LIFETIME, unpaused).refresh_in,
            Some(DUE_59M30S)
        );

        // Active turn paused on approval for 10m (< 15m grace): keeps warm and schedules 5m recheck.
        let approval_10m_ago = now - 10 * 60 * 1000;
        let within_grace = evaluate_fusion_pair_activity(
            now,
            GRACE,
            true,
            false,
            false,
            false,
            Some(approval_10m_ago),
            None,
            None,
        );
        assert!(within_grace.should_keep_warm());
        let plan = fusion_keep_warm_plan(Some(LIFETIME), LIFETIME, within_grace);
        assert_eq!(plan.refresh_in, Some(DUE_59M30S));
        assert_eq!(plan.recheck_in, Some(Duration::from_mins(5)));

        // Active turn paused on approval for exactly 15m (>= grace): stops keep_warm.
        let approval_15m_ago = now - 15 * 60 * 1000;
        let past_grace = evaluate_fusion_pair_activity(
            now,
            GRACE,
            true,
            false,
            false,
            false,
            Some(approval_15m_ago),
            None,
            None,
        );
        assert!(!past_grace.should_keep_warm());
        assert_eq!(
            fusion_keep_warm_plan(Some(LIFETIME), LIFETIME, past_grace),
            KeepWarmPlan::default()
        );
    }

    #[test]
    fn idle_open_ask_user_respects_grace_window_and_hands_off_to_compaction() {
        let now = 20_000_000_i64;
        // Open ask_user 4m ago (< 15m grace): keep_warm active, recheck in 11m.
        let young = evaluate_fusion_pair_activity(
            now,
            GRACE,
            false,
            false,
            false,
            false,
            None,
            None,
            Some(now - 4 * 60 * 1000),
        );
        assert!(young.should_keep_warm());
        let plan = fusion_keep_warm_plan(Some(LIFETIME), LIFETIME, young);
        assert_eq!(plan.refresh_in, Some(DUE_59M30S));
        assert_eq!(plan.recheck_in, Some(Duration::from_mins(11)));

        // Open ask_user 16m ago (>= 15m grace): keep_warm stops so idle compaction runs while warm.
        let stale = evaluate_fusion_pair_activity(
            now,
            GRACE,
            false,
            false,
            false,
            false,
            None,
            None,
            Some(now - 16 * 60 * 1000),
        );
        assert!(!stale.should_keep_warm());
        assert_eq!(
            fusion_keep_warm_plan(Some(LIFETIME), LIFETIME, stale),
            KeepWarmPlan::default()
        );

        // Fully idle with no open ask_user: keep_warm stops.
        let idle =
            evaluate_fusion_pair_activity(now, GRACE, false, false, false, false, None, None, None);
        assert!(!idle.should_keep_warm());
        assert_eq!(
            fusion_keep_warm_plan(Some(LIFETIME), LIFETIME, idle),
            KeepWarmPlan::default()
        );
    }
}
