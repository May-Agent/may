//! TUI components for May agent.

pub mod agent_event_adapter;
pub mod app;
pub mod clipboard;
pub mod completion;
pub mod event;
pub mod history;
pub mod queue;
pub mod reducer;
pub mod runtime;
pub mod state;
pub mod ui;

pub use app::{App, SendAction};
pub use state::{AppMode, AskUserState};
pub use completion::{Completion, CompletionMode};
pub use event::{AppEvent, EventHandler};
pub use history::InputHistory;
pub use queue::{Attachment, PendingMessage};
