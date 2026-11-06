//! UI components for the TUI.

pub mod chat;
pub mod chat_layout;
pub mod file_picker;
pub(super) mod highlight;
pub mod image;
#[cfg(unix)]
mod image_probe;
mod kitty_diacritics;
mod layout;
pub mod links;
pub mod markdown;
pub mod streaming_markdown;
#[cfg(test)]
pub(crate) mod test_screen;

use ratatui::Frame;

use super::app::App;

/// Render the entire UI.
pub fn render(frame: &mut Frame, app: &mut App) {
    layout::render(frame, app);
}
