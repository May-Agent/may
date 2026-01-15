//! TUI components for May agent.

pub mod app;
pub mod clipboard;
pub mod completion;
pub mod event;
pub mod history;
pub mod queue;
pub mod ui;

pub use app::{App, AppMode, SendAction};
pub use completion::{Completion, CompletionMode};
pub use event::{AppEvent, EventHandler};
pub use history::InputHistory;
pub use queue::{Attachment, PendingMessage};
