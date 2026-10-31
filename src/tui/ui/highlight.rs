//! Shell-script syntax highlighting for tool-call command text.
//!
//! Mirrors the web UI's Prism/bash card: one `Vec<Span>` per source line,
//! colored with a dark syntect theme (the TUI assumes a dark terminal).

use std::sync::LazyLock;

use ratatui::prelude::*;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

/// Bundled default syntaxes; loaded once, immutable afterwards.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// The dark theme shell commands are highlighted with.
static THEME: LazyLock<Theme> = LazyLock::new(|| {
    ThemeSet::load_defaults()
        .themes
        .get("base16-ocean.dark")
        .expect("syntect's default themes include base16-ocean.dark")
        .clone()
});

/// Convert a syntect style to a ratatui style: foreground color and font
/// modifiers; the theme's background is dropped so the chat surface shows
/// through.
fn to_style(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

/// Highlight `script` as bash, returning one span vector per source line.
/// The script is highlighted as a unit so state (quotes, heredocs) carries
/// across lines. An empty script yields no lines.
pub(super) fn shell_spans(script: &str) -> Vec<Vec<Span<'static>>> {
    let syntax = SYNTAXES
        .find_syntax_by_extension("bash")
        .expect("syntect's default syntaxes include bash");
    let mut highlighter = HighlightLines::new(syntax, &THEME);
    script
        .lines()
        .map(|line| {
            highlighter
                .highlight_line(line, &SYNTAXES)
                .expect("bundled syntaxes highlight without missing references")
                .into_iter()
                .map(|(style, text)| Span::styled(text.to_string(), to_style(style)))
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::shell_spans;

    #[test]
    fn shell_spans_colors_a_command() {
        let lines = shell_spans("echo hi | grep h");
        assert_eq!(lines.len(), 1);
        // A pipeline produces more than one styled region.
        assert!(lines[0].len() > 1);
        assert_eq!(
            lines[0]
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "echo hi | grep h"
        );
    }

    #[test]
    fn shell_spans_keeps_state_across_lines() {
        let lines = shell_spans("x=1\necho \"$x\"");
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[1]
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "echo \"$x\""
        );
    }
}
