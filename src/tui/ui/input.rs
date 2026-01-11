//! Input box component with completion overlay.

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};

use super::super::app::App;
use super::super::completion::CompletionMode;
use super::super::queue::Attachment;

/// Convert a character index to a byte index in a string.
fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Render the input box with optional completion overlay.
pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    // Build input line with attachments
    let mut spans = Vec::new();

    // Prompt
    spans.push(Span::styled("> ", Style::default().fg(Color::Cyan)));

    // Render image attachments
    for att in &app.attachments {
        if matches!(att, Attachment::Image { .. }) {
            spans.push(Span::styled(
                "[image] ",
                Style::default().bg(Color::DarkGray),
            ));
        }
    }

    // File references
    for att in &app.attachments {
        if let Attachment::FileReference { path } = att {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
            spans.push(Span::styled(
                format!("@{} ", name),
                Style::default().fg(Color::Cyan),
            ));
        }
    }

    // Pasted text attachments
    for att in &app.attachments {
        if let Attachment::PastedText { content, .. } = att {
            let char_count = content.chars().count();
            spans.push(Span::styled(
                format!("[pasted: {} chars] ", char_count),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }

    // Input text with cursor (convert char index to byte index)
    let byte_idx = char_to_byte_index(&app.input, app.cursor);
    let (before, after) = app.input.split_at(byte_idx);
    spans.push(Span::raw(before.to_string()));
    spans.push(Span::styled("|", Style::default().fg(Color::Yellow)));
    spans.push(Span::raw(after.to_string()));

    let line = Line::from(spans);

    let paragraph = Paragraph::new(line);

    frame.render_widget(paragraph, area);

    // Render completion overlay if active
    if app.completion.is_active() && !app.completion.is_empty() {
        render_completion_overlay_impl(frame, area, app);
    }
}

/// Internal implementation of completion overlay rendering.
fn render_completion_overlay_impl(frame: &mut Frame, input_area: Rect, app: &App) {
    // Calculate overlay position (above input)
    let max_items = 10;
    let item_count = app.completion.len().min(max_items);
    let height = (item_count + 2) as u16; // +2 for borders

    // Position overlay above input, use full width
    let overlay_y = input_area.y.saturating_sub(height);
    let overlay_area = Rect {
        x: input_area.x,
        y: overlay_y,
        width: input_area.width,
        height,
    };

    // Clear background
    frame.render_widget(Clear, overlay_area);

    // Build list items
    let selected = app.completion.selected();
    let items: Vec<ListItem> = app
        .completion
        .visible_items(max_items)
        .map(|(idx, item)| {
            let is_selected = idx == selected;
            let style = if is_selected {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default().fg(Color::White)
            };

            // Format based on completion mode - no truncation
            let text = match app.completion.mode {
                CompletionMode::Slash => {
                    format!("{:<15} {}", item.label, item.description)
                }
                CompletionMode::File => {
                    // Just show the relative path directly
                    item.label.clone()
                }
                CompletionMode::None => item.label.clone(),
            };

            ListItem::new(text).style(style)
        })
        .collect();

    let title = match app.completion.mode {
        CompletionMode::Slash => "Commands",
        CompletionMode::File => "Files",
        CompletionMode::None => "",
    };

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title),
    );

    frame.render_widget(list, overlay_area);
}
