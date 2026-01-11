//! Markdown rendering using tui-markdown.

use ratatui::prelude::*;

/// Render markdown text to ratatui lines.
pub fn render_markdown(text: &str) -> Vec<Line<'static>> {
    let parsed = tui_markdown::from_str(text);
    // Convert to owned to get 'static lifetime while preserving line styles/alignment.
    parsed
        .lines
        .into_iter()
        .map(|line| {
            let spans = line
                .spans
                .into_iter()
                .map(|span| Span::styled(span.content.to_string(), span.style))
                .collect::<Vec<_>>();

            Line {
                style: line.style,
                alignment: line.alignment,
                spans,
            }
        })
        .collect()
}
