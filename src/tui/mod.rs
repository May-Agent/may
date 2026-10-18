//! TUI components for May agent.

/// Adapt agent tool events into frontend-agnostic kinds.
pub mod agent_event_adapter;
/// Application state and top-level event handling.
pub mod app;
/// Clipboard read/write, image download, and paste helpers.
pub mod clipboard;
/// Slash-command and file-mention completion state.
pub mod completion;
/// TUI error type and result alias.
pub mod error;
/// Terminal event polling and app event channel.
pub mod event;
/// Per-session file-diff worker feeding tool-call diffs to the chat stream.
pub mod file_diff;
/// Persistent input history.
pub mod history;
/// Pending message and attachment types.
pub mod queue;
/// Event reducers for non-chat modes.
pub mod reducer;
/// Agent worker runtime glue for the TUI.
pub mod runtime;
/// UI mode and per-mode state types.
pub mod state;
/// Rendering layer for the TUI.
pub mod ui;

pub use app::{App, SendAction};
pub use completion::{Completion, CompletionMode};
pub use event::{AppEvent, EventHandler};
pub use history::InputHistory;
pub use queue::{Attachment, PendingMessage};
pub use state::{AppMode, AskUserState, SelectionPoint, TextSelection};
