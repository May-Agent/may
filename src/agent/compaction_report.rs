//! Reports a compaction's progress as it runs.
//!
//! A compaction is a [`Run`] like a turn, but it also runs while the session
//! is idle, where no turn carries its events to the clients. The worker
//! drives it through a [`CompactionReporter`], which hands each start and
//! finish to the daemon as a [`CompactionPhase`].

use std::future::Future;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};

use aither_agent::{AgentError, AgentEvent, CompactResult, CompactionOutcome, Run};
use async_channel::Sender;

use crate::client::CompactionPhase;

/// Where the phases of the compactions it drives go.
#[derive(Debug)]
pub struct CompactionReporter {
    phases: Option<Sender<CompactionPhase>>,
    /// A `Started` went out whose finish has not.
    open: AtomicBool,
}

impl CompactionReporter {
    /// A reporter that sends each phase on `phases`.
    #[must_use]
    pub const fn new(phases: Sender<CompactionPhase>) -> Self {
        Self {
            phases: Some(phases),
            open: AtomicBool::new(false),
        }
    }

    /// A reporter nobody listens to.
    #[must_use]
    pub const fn silent() -> Self {
        Self {
            phases: None,
            open: AtomicBool::new(false),
        }
    }

    fn report(&self, phase: CompactionPhase) {
        self.open
            .store(phase == CompactionPhase::Started, Ordering::Release);
        if let Some(phases) = &self.phases {
            // Unbounded: never full, and closed only with the daemon.
            let _ = phases.try_send(phase);
        }
    }

    /// Drives `run` to its end, reporting each phase, and returns the
    /// result the compaction applied, `None` when it applied nothing.
    ///
    /// # Errors
    ///
    /// The compaction's failure, after its `Failed` phase went out.
    pub async fn drive<F>(&self, run: Run<F>) -> Result<Option<CompactResult>, AgentError>
    where
        F: Future<Output = Result<(), AgentError>>,
    {
        let driven = run
            .drive(
                None,
                std::future::pending::<std::convert::Infallible>(),
                |applied, timed| async move {
                    if let Some(phase) = CompactionPhase::from_event(&timed.event) {
                        self.report(phase);
                    }
                    ControlFlow::Continue(match timed.event {
                        AgentEvent::CompactionEnded {
                            outcome: CompactionOutcome::Applied(result),
                            ..
                        } => Some(result),
                        _ => applied,
                    })
                },
            )
            .await?;
        match driven {
            ControlFlow::Continue(applied) => Ok(applied),
            ControlFlow::Break(never) => match never {},
        }
    }
}

impl Drop for CompactionReporter {
    /// A compaction dropped between its start and its finish ended with
    /// nothing applied; clients waiting on it are told.
    fn drop(&mut self) {
        if *self.open.get_mut() {
            self.report(CompactionPhase::Discarded);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_compaction_is_reported_discarded() {
        let (phases, rx) = async_channel::unbounded();
        let reporter = CompactionReporter::new(phases);
        reporter.report(CompactionPhase::Started);
        drop(reporter);
        assert_eq!(rx.try_recv(), Ok(CompactionPhase::Started));
        assert_eq!(rx.try_recv(), Ok(CompactionPhase::Discarded));
        assert!(rx.try_recv().is_err(), "nothing more");

        // A finished compaction owes nothing on drop.
        let (phases, rx) = async_channel::unbounded();
        let reporter = CompactionReporter::new(phases);
        reporter.report(CompactionPhase::Started);
        reporter.report(CompactionPhase::Applied);
        drop(reporter);
        assert_eq!(rx.try_recv(), Ok(CompactionPhase::Started));
        assert_eq!(rx.try_recv(), Ok(CompactionPhase::Applied));
        assert!(rx.try_recv().is_err(), "nothing more");
    }
}
