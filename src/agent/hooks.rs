//! Agent-runtime hooks wired onto [`super::may::MayAgent`].
//!
//! [`PendingSteerHook`] watches the persisted session-runtime state
//! for a queued "immediate" user message and ends the current agent
//! turn the moment one appears, so steering messages take effect
//! without waiting for the model to finish the current reasoning loop.
//!
//! [`ContextSnapshotHook`] runs on every aither checkpoint event and
//! writes the checkpoint payload to SQLite so the session can be
//! resumed after a restart. Both hooks are no-ops when the agent was
//! created without a live [`SessionRuntimeService`] (for example in
//! exec mode). The snapshot is a run-scoped commit, fenced by the lease of
//! the run in this process; the steer hook also ends the turn of a run that
//! lost its lease.

use aither_agent::{CheckpointContext, Hook, TurnBoundaryAction, TurnBoundaryContext};

use std::sync::Arc;

use crate::session::{SessionId, SessionRuntimeService};

/// Ends the current agent turn when the host-side session runtime
/// reports a pending user message waiting to steer the conversation.
#[derive(Debug, Clone)]
pub(super) struct PendingSteerHook {
    runtime: Option<Arc<SessionRuntimeService>>,
    session_id: String,
}

impl PendingSteerHook {
    pub(super) fn new(runtime: Option<Arc<SessionRuntimeService>>, session_id: &SessionId) -> Self {
        Self {
            runtime,
            session_id: session_id.as_str().to_string(),
        }
    }
}

impl Hook for PendingSteerHook {
    async fn on_turn_boundary(&self, _ctx: &TurnBoundaryContext<'_>) -> TurnBoundaryAction {
        let Some(service) = &self.runtime else {
            return TurnBoundaryAction::Continue;
        };
        if service.runs().is_lost(&self.session_id) {
            return TurnBoundaryAction::EndTurn;
        }
        let Ok(runtime) = service.db().get_session_runtime(&self.session_id).await else {
            tracing::warn!(
                session_id = %self.session_id,
                "failed to inspect runtime for steer boundary"
            );
            return TurnBoundaryAction::Continue;
        };
        if runtime.is_some_and(|summary| summary.has_pending_immediate()) {
            TurnBoundaryAction::EndTurn
        } else {
            TurnBoundaryAction::Continue
        }
    }
}

/// Persists aither checkpoint payloads into session storage so the
/// session can be resumed after a restart.
#[derive(Debug, Clone)]
pub(super) struct ContextSnapshotHook {
    runtime: Option<Arc<SessionRuntimeService>>,
    session_id: String,
}

impl ContextSnapshotHook {
    pub(super) fn new(runtime: Option<Arc<SessionRuntimeService>>, session_id: &SessionId) -> Self {
        Self {
            runtime,
            session_id: session_id.as_str().to_string(),
        }
    }
}

impl Hook for ContextSnapshotHook {
    async fn on_checkpoint(&self, ctx: &CheckpointContext<'_>) -> Option<String> {
        let service = self.runtime.as_ref()?;
        let Some(fence) = service.runs().live_fence(&self.session_id) else {
            return Some(format!(
                "session {} checkpointed outside a run lease",
                self.session_id
            ));
        };
        match crate::session::persist_checkpoint_from_hook(&service.db(), &fence, ctx).await {
            Ok(()) => None,
            Err(error) => Some(error.to_string()),
        }
    }
}
