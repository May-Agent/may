//! Pausing active shell-command timeouts while a permission prompt awaits the
//! user.
//!
//! A permission-gated terminal command (sandbox escape, publish approval,
//! workspace request, SSH connect) can sit waiting on the user for longer than
//! the command's own wall-clock timeout. While at least one prompt is open for
//! the active call (or session-wide when the prompt cannot identify its call),
//! the shell-tool watchdog suspends the command's timeout clock and resumes it
//! once the user answers or the prompt is cancelled.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use aither_core::llm::ToolCallId;
use event_listener::{Event, EventListener};

#[derive(Debug, Default)]
struct PauseState {
    session_holds: usize,
    call_holds: HashMap<ToolCallId, usize>,
}

impl PauseState {
    fn is_held(&self, call: Option<&ToolCallId>) -> bool {
        if self.session_holds > 0 {
            return true;
        }
        call.is_some_and(|id| self.call_holds.get(id).copied().unwrap_or(0) > 0)
    }
}

/// Shared coordinator between user-approval waiters and the shell-tool timeout
/// watchdog for one session.
#[derive(Debug, Clone, Default)]
pub struct ApprovalPause {
    state: Arc<Mutex<PauseState>>,
    changed: Arc<Event>,
}

impl ApprovalPause {
    /// Creates an empty pause coordinator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Holds timeout clocks for `call` until the returned guard is dropped.
    ///
    /// # Panics
    /// Panics if the internal pause mutex is poisoned.
    #[must_use]
    pub fn hold_call(&self, call: ToolCallId) -> ApprovalHoldGuard {
        {
            let mut state = self.state.lock().expect("approval pause mutex poisoned");
            *state.call_holds.entry(call.clone()).or_default() += 1;
        }
        self.changed.notify(usize::MAX);
        ApprovalHoldGuard {
            pause: self.clone(),
            call: Some(call),
        }
    }

    /// Holds timeout clocks across the session until the returned guard is
    /// dropped.
    ///
    /// # Panics
    /// Panics if the internal pause mutex is poisoned.
    #[must_use]
    pub fn hold_session(&self) -> ApprovalHoldGuard {
        {
            let mut state = self.state.lock().expect("approval pause mutex poisoned");
            state.session_holds += 1;
        }
        self.changed.notify(usize::MAX);
        ApprovalHoldGuard {
            pause: self.clone(),
            call: None,
        }
    }

    /// Holds timeout clocks for `call` when present, or session-wide otherwise.
    #[must_use]
    pub fn hold(&self, call: Option<ToolCallId>) -> ApprovalHoldGuard {
        match call {
            Some(id) => self.hold_call(id),
            None => self.hold_session(),
        }
    }

    /// Whether timeout clocks for `call` are currently paused.
    ///
    /// # Panics
    /// Panics if the internal pause mutex is poisoned.
    #[must_use]
    pub fn is_held(&self, call: Option<&ToolCallId>) -> bool {
        self.state
            .lock()
            .expect("approval pause mutex poisoned")
            .is_held(call)
    }

    /// Registers a listener notified whenever any hold is acquired or released.
    #[must_use]
    pub fn listen(&self) -> EventListener {
        self.changed.listen()
    }
}

/// RAII guard that releases one approval pause hold on drop.
#[derive(Debug)]
pub struct ApprovalHoldGuard {
    pause: ApprovalPause,
    call: Option<ToolCallId>,
}

impl Drop for ApprovalHoldGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.pause.state.lock() {
            match &self.call {
                Some(id) => {
                    if let Some(count) = state.call_holds.get_mut(id) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            state.call_holds.remove(id);
                        }
                    }
                }
                None => {
                    state.session_holds = state.session_holds.saturating_sub(1);
                }
            }
        }
        self.pause.changed.notify(usize::MAX);
    }
}

/// Detached job ids that the pause-aware shell watchdog explicitly timed out
/// (and killed), so completion reports can tell a timeout kill from an external
/// signal even when the inner shell ran with an uncapped timeout.
#[derive(Debug, Clone, Default)]
pub struct TimedOutJobs {
    jobs: Arc<Mutex<HashSet<String>>>,
}

impl TimedOutJobs {
    /// Creates an empty timed-out job registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `job_id` as killed by the pause-aware timeout watchdog.
    ///
    /// # Panics
    /// Panics if the internal mutex is poisoned.
    pub fn record(&self, job_id: impl Into<String>) {
        self.jobs
            .lock()
            .expect("timed out jobs mutex poisoned")
            .insert(job_id.into());
    }

    /// Removes and returns whether `job_id` was recorded as timed out.
    ///
    /// # Panics
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn take(&self, job_id: &str) -> bool {
        self.jobs
            .lock()
            .expect("timed out jobs mutex poisoned")
            .remove(job_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_call_and_session_track_refcounts() {
        let pause = ApprovalPause::new();
        let call_a = ToolCallId::new("call-a");
        let call_b = ToolCallId::new("call-b");

        assert!(!pause.is_held(Some(&call_a)));
        assert!(!pause.is_held(None));

        let guard_a1 = pause.hold_call(call_a.clone());
        let guard_a2 = pause.hold_call(call_a.clone());
        assert!(pause.is_held(Some(&call_a)));
        assert!(!pause.is_held(Some(&call_b)));
        assert!(!pause.is_held(None));

        drop(guard_a1);
        assert!(pause.is_held(Some(&call_a)));
        drop(guard_a2);
        assert!(!pause.is_held(Some(&call_a)));

        let session_guard = pause.hold_session();
        assert!(pause.is_held(Some(&call_a)));
        assert!(pause.is_held(Some(&call_b)));
        assert!(pause.is_held(None));
        drop(session_guard);
        assert!(!pause.is_held(Some(&call_a)));
    }

    #[test]
    fn timed_out_jobs_take_is_one_shot() {
        let timed_out = TimedOutJobs::new();
        timed_out.record("job-1");
        assert!(timed_out.take("job-1"));
        assert!(!timed_out.take("job-1"));
        assert!(!timed_out.take("job-2"));
    }
}
