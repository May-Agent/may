//! Session management for May agent.
//!
//! Each session has its own sandbox directory and chat history.

pub use may_protocol::session_import::*;
pub use may_protocol::session_messages::*;
pub use may_protocol::session_runtime::*;

mod binding;
mod context_snapshot;
mod directory;
pub mod history;
pub mod import;
pub use may_protocol::session_layout as layout;
mod manager;
pub mod open_questions;
/// A message row read into its model form.
pub mod persisted_message;
pub use may_protocol::session_plan as plan;
pub mod runs;
#[cfg(test)]
mod runs_tests;
pub use may_protocol::session_runtime as runtime;
/// The daemon-side session service the client talks to.
pub mod runtime_service;
pub mod store;
pub use may_protocol::tool_result_wire;
pub mod user_wait;

pub use binding::{BindingError, RepoBinding, group_sessions, project_label};
pub use context_snapshot::snapshot_upsert;
pub use context_snapshot::{
    PersistContextSnapshotError, PersistedContextSnapshot, checkpoint_watermark,
    load_persisted_context_snapshot, parse_persisted_context_snapshot,
    persist_agent_context_snapshot, persist_run_checkpoint,
};
pub use directory::SessionDirectory;
pub use history::{
    ChatHistory, ChatMessage, ChatMessageStats, HistoryReplayError, MessageRole,
    MessageTranslation, SessionMetadata, ToolEventRecord, chat_messages_from_transcript,
    language_display_name,
};
pub use layout::{MayHomePaths, ProfileHomePaths, SessionPaths};
pub use manager::{
    RewindPoint, Session, SessionError, SessionId, SessionLaunch, SessionManager, SessionSummary,
    chat_message_from_row, cloud_repository, initialize_session_persistence,
    list_summaries_from_db, load_history_from_db, reconcile_container_profile_sessions,
    summarize_db_sessions,
};
pub use persisted_message::{PersistedMessage, PersistedMessageError};
pub use plan::SessionPlanState;
pub use runs::{
    AcquireError, CancelReason, FinishError, RunOutcome, RunReleased, SessionBusy, SessionRunGuard,
    SessionRuns, SettleRetry,
};
pub use runtime::{
    AttachmentKind, BrowserInteractionState, PendingMessageState, PendingUserMessage,
    PendingUserMessageMode, PersistedAskUserQuestion, PersistedAskUserRequest, PersistedAttachment,
    PersistedPermissionRequest, PersistedPermissionRequestPayload, PersistedSecretRequest,
    PersistedWorkspaceRequest, PersistedWorkspaceRequestPayload, SessionInteractionState,
    SessionRuntimeState, SessionRuntimeSummary, TerminalRuntimeState,
};
pub use runtime_service::{AnswerError, AskUserAnswer, ResolvedRequest, SessionRuntimeService};
pub use store::{SandboxResourceLayout, SessionStore, StoreError};
pub use user_wait::{UserAsk, UserWait};

/// Creates `path` (and missing parents) and restricts `path` itself to mode
/// `0700` on Unix so other local users cannot traverse `~/.may`.
///
/// # Errors
/// Returns an I/O error when directory creation or permission tightening fails.
#[allow(
    clippy::disallowed_methods,
    reason = "explicitly synchronous counterpart of ensure_private_dir_async"
)]
pub fn ensure_private_dir_sync(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)?;
    }
    Ok(())
}

/// Async counterpart of [`ensure_private_dir_sync`].
///
/// # Errors
/// Returns an I/O error when directory creation or permission tightening fails.
pub async fn ensure_private_dir_async(path: &std::path::Path) -> std::io::Result<()> {
    let owned = path.to_path_buf();
    blocking::unblock(move || ensure_private_dir_sync(&owned)).await
}
