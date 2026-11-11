//! Agent-runtime hooks wired onto [`super::may::MayAgent`].
//!
//! [`MayHooks`] is the one [`Hooks`] value an agent is built with. Its parts:
//!
//! - the pending-steer check ends the current agent turn the moment the
//!   persisted session-runtime state holds a queued "immediate" user message,
//!   so steering takes effect without waiting for the model to finish the
//!   current reasoning loop; it is a no-op when the agent was created without
//!   a live [`SessionRuntimeService`] (for example in exec mode);
//! - secret redaction replaces stored secret values in every tool result
//!   with the secret's name before the agent commits it, so the session
//!   context, the persisted history and the published events only ever carry
//!   the name;
//! - the park rule ends a run whose turn only re-read unchanged output while
//!   background commands still run: another request would read the same
//!   output again, and only a background event can say something new.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aither_agent::{AfterTool, Hooks, ToolOutcome, TurnBoundaryAction, TurnBoundaryContext};
use aither_sandbox::{JobStatus, Jobs, Refused, UnchangedRead};

use crate::command_policy::RefusedReads;
use crate::harness::compact_tool::{COMPACT_TOOL_NAME, CompactSignal};
use crate::secrets::SecretRedactor;
use crate::session::{SessionId, SessionRuntimeService};

/// What a May agent decides at the loop's control points.
#[derive(Debug, Clone)]
pub(super) struct MayHooks {
    runtime: Option<Arc<SessionRuntimeService>>,
    session_id: String,
    redactor: SecretRedactor,
    refused: RefusedReads,
    jobs: Jobs,
    /// The `compact` tool's valve: a request it holds ends the turn.
    valve: Arc<CompactSignal>,
    /// Set when the valve ended a turn for a compaction; the runner takes it.
    compaction: Arc<AtomicBool>,
}

impl MayHooks {
    pub(super) fn new(
        runtime: Option<Arc<SessionRuntimeService>>,
        session_id: &SessionId,
        redactor: SecretRedactor,
        refused: RefusedReads,
        jobs: Jobs,
        valve: Arc<CompactSignal>,
        compaction: Arc<AtomicBool>,
    ) -> Self {
        Self {
            runtime,
            session_id: session_id.as_str().to_string(),
            redactor,
            refused,
            jobs,
            valve,
            compaction,
        }
    }

    /// Whether the persisted runtime holds a user message waiting to steer
    /// the conversation.
    async fn steer_pending(&self) -> bool {
        let Some(service) = &self.runtime else {
            return false;
        };
        let Ok(runtime) = service.db().get_session_runtime(&self.session_id).await else {
            tracing::warn!(
                session_id = %self.session_id,
                "failed to inspect runtime for steer boundary"
            );
            return false;
        };
        runtime.is_some_and(|summary| summary.has_pending_immediate())
    }

    /// Whether every call of the turn re-read unchanged output.
    ///
    /// Every outcome is asked, so no refused call stays on the record.
    fn only_unchanged_reads(&self, outcomes: &[ToolOutcome]) -> bool {
        outcomes
            .iter()
            .map(|outcome| {
                let refused = self.refused.take(&outcome.call);
                let polled =
                    outcome.result.as_ref().err().is_some_and(|error| {
                        error.downcast_ref::<Refused<UnchangedRead>>().is_some()
                    });
                refused || polled
            })
            .fold(!outcomes.is_empty(), |all, unchanged| all && unchanged)
    }
}

impl Hooks for MayHooks {
    async fn after_tool(&self, outcome: &ToolOutcome) -> AfterTool {
        if outcome.name != COMPACT_TOOL_NAME {
            self.valve.acted();
        }
        // A tool result is never shown to the model unredacted: when the
        // stored secrets cannot be read, the run ends with that error.
        let redactions = match self.redactor.current().await {
            Ok(redactions) => redactions,
            Err(error) => return AfterTool::Abort(error.to_string()),
        };
        let shown = outcome.for_model();
        let mut result = shown.clone();
        redactions.redact_tool_result(&mut result);
        if result == shown {
            AfterTool::Keep
        } else {
            AfterTool::Replace(result)
        }
    }

    async fn on_turn_boundary(&self, cx: &TurnBoundaryContext<'_>) -> TurnBoundaryAction {
        if self.steer_pending().await {
            return TurnBoundaryAction::EndTurn;
        }
        if self.valve.take_request() {
            self.compaction.store(true, Ordering::Release);
            return TurnBoundaryAction::EndTurn;
        }
        if self.only_unchanged_reads(cx.outcomes)
            && self
                .jobs
                .list()
                .await
                .iter()
                .any(|job| job.status == JobStatus::Running)
        {
            tracing::info!(
                turn = cx.turn,
                "every call re-read unchanged output; parking the run until background work reports"
            );
            return TurnBoundaryAction::EndTurn;
        }
        TurnBoundaryAction::Continue
    }
}
