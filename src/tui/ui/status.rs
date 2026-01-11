//! Status bar component.

use ratatui::{
    prelude::*,
    widgets::Paragraph,
};

use super::super::app::App;

/// Render the status bar.
pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    // If there's a status message (like Ctrl+C hint), show it prominently
    if let Some(ref status) = app.status {
        let paragraph = Paragraph::new(status.as_str())
            .style(Style::default().fg(Color::Yellow));
        frame.render_widget(paragraph, area);
        return;
    }

    let model = app.model_display();
    let queue = app.queue.len();
    let bg = app.background_tasks;

    // Build status line - show model and context usage
    let mut parts = vec![model];

    // Add token usage info
    if let Some(ref usage) = app.usage {
        if let Some(prompt_tokens) = usage.prompt_tokens {
            let used = format_number(prompt_tokens as usize);
            if let Some(context_window) = app.context_window {
                // Show used/total if we know context window
                parts.push(format!("{}/{}", used, format_number(context_window as usize)));
            } else {
                // Show just tokens used
                parts.push(format!("{}tok", used));
            }
        }
    }

    if queue > 0 {
        parts.push(format!("Q:{}", queue));
    }

    if bg > 0 {
        parts.push(format!("BG:{}", bg));
    }

    // Show processing indicator
    if app.processing {
        parts.push("...".to_string());
    }

    let status_text = parts.join(" | ");

    let paragraph = Paragraph::new(status_text)
        .style(Style::default().fg(Color::Cyan));

    frame.render_widget(paragraph, area);
}

/// Format large numbers with K/M suffix for readability.
fn format_number(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
