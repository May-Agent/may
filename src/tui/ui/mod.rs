//! UI components for the TUI.

pub mod chat;
pub mod file_picker;
mod layout;
mod markdown;

use ratatui::Frame;

use super::app::App;

/// Render the entire UI.
pub fn render(frame: &mut Frame, app: &App) {
    layout::render(frame, app);
}
