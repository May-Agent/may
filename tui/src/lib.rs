#![cfg_attr(test, allow(clippy::disallowed_methods))]
//! TUI components for May agent.

/// Adapt agent tool events into frontend-agnostic kinds.
pub mod agent_event_adapter;
/// Application state and top-level event handling.
pub mod app;
/// Clipboard read/write, image download, and paste helpers.
pub mod clipboard;
/// Slash-command and file-mention completion state.
pub mod completion;
/// The `/connect` picker for ACP agents.
pub mod connect;
/// Desktop notifications raised through the terminal.
pub mod desktop_notification;
/// TUI error type and result alias.
pub mod error;
/// Terminal event polling and app event channel.
pub mod event;
/// Per-session file-diff worker feeding tool-call diffs to the chat stream.
pub mod file_diff;
/// Grapheme-boundary cursor arithmetic for the composer.
pub mod grapheme;
/// Persistent input history.
pub mod history;
/// New Year greeting and fireworks.
pub mod new_year;
/// Jank instrumentation (frame/loop timing for stall reports).
pub mod perf;
/// Pending message and attachment types.
pub mod queue;
/// Event reducers for non-chat modes.
pub mod reducer;
/// Agent worker over the May daemon.
pub mod remote;
/// Agent worker runtime glue for the TUI.
pub mod runtime;
/// Right sidebar state: task tree, linked issues.
pub mod sidebar;
/// The table of slash commands completion, dispatch and help share.
pub mod slash;
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

/// Cached user home directory (`dirs::home_dir()`), queried once per process
/// so render-path callers avoid repeated environment lookups per frame.
#[must_use]
pub(crate) fn home_dir() -> Option<&'static std::path::Path> {
    static HOME: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    HOME.get_or_init(dirs::home_dir).as_deref()
}
