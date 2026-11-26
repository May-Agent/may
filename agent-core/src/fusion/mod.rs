//! Experimental Fusion mode: frontier lead paired with a persistent cache-warm sidekick.
//!
//! Coordinates the shared runtime state of a Fusion lead/sidekick pair:
//! - [`keep_warm`]: unified keep-warm predicate and pair activity state shared by both
//!   agents so `keep_warm_due_in` suppresses early `idle_compaction_due_in` while active
//!   and hands off to `compact_when_idle` once idle past the grace window;
//! - [`edit_tracker`]: workspace-tree mutation detector for the lead's `terminal` calls
//!   producing a once-per-turn reminder (reset on the next `sidekick` call) without
//!   blocking edits or misfiring on read-only calls;
//! - [`sidekick_tool`]: the `main_exclusive` dynamic `sidekick` IPC command and daemon
//!   execution trait.

pub mod edit_tracker;
pub mod keep_warm;
pub mod sidekick_tool;

use std::path::PathBuf;
use std::sync::Arc;

use crate::session::SessionId;

pub use edit_tracker::WorkspaceEditTracker;
pub use keep_warm::{
    ActiveTurnGuard, FusionPairActivity, FusionPairRegistry, FusionPairState, KeepWarmPlan,
    evaluate_fusion_pair_activity, fusion_keep_warm_plan,
};
pub use sidekick_tool::{
    SidekickArgs, SidekickCommandSet, SidekickExecutor, SidekickOutcome, SidekickRequest,
    SidekickTool, render_sidekick_completed,
};

/// Companion role string stored in `session_companions.role` for Fusion sidekicks.
pub const SIDEKICK_ROLE: &str = "sidekick";

/// Whether an agent belongs to a normal user-facing session or a hidden Fusion companion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SessionRole {
    /// Normal user-facing session (including a Fusion lead).
    #[default]
    Main,
    /// Persistent hidden Fusion sidekick linked to `parent_session_id`.
    FusionSidekick {
        /// Parent lead session id.
        parent_session_id: SessionId,
        /// Parent lead session's sandbox root (whose `repos/` clones are shared directly).
        parent_sandbox_dir: PathBuf,
    },
}

impl SessionRole {
    /// Whether this role is a hidden Fusion sidekick companion.
    #[must_use]
    pub const fn is_sidekick(&self) -> bool {
        matches!(self, Self::FusionSidekick { .. })
    }

    /// Parent session id when this role is a companion sidekick.
    #[must_use]
    pub const fn parent_session_id(&self) -> Option<&SessionId> {
        match self {
            Self::Main => None,
            Self::FusionSidekick {
                parent_session_id, ..
            } => Some(parent_session_id),
        }
    }

    /// Parent sandbox root when this role is a companion sidekick.
    #[must_use]
    pub fn parent_sandbox_dir(&self) -> Option<&std::path::Path> {
        match self {
            Self::Main => None,
            Self::FusionSidekick {
                parent_sandbox_dir, ..
            } => Some(parent_sandbox_dir.as_path()),
        }
    }
}

/// Daemon-hosted service coordinating Fusion mode pairs and `sidekick` execution.
#[derive(Clone)]
pub struct FusionService {
    pairs: FusionPairRegistry,
    executor: Arc<dyn SidekickExecutor>,
}

impl std::fmt::Debug for FusionService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FusionService").finish_non_exhaustive()
    }
}

impl FusionService {
    /// Creates a new Fusion service backed by `pairs` and `executor`.
    #[must_use]
    pub fn new(pairs: FusionPairRegistry, executor: Arc<dyn SidekickExecutor>) -> Self {
        Self { pairs, executor }
    }

    /// Shared pair activity/lease registry.
    #[must_use]
    pub const fn pairs(&self) -> &FusionPairRegistry {
        &self.pairs
    }

    /// Executes a `sidekick` request through the daemon executor.
    ///
    /// # Errors
    /// Returns an error string when execution fails.
    pub async fn execute(&self, request: SidekickRequest) -> Result<SidekickOutcome, String> {
        self.executor.execute(request).await
    }
}
