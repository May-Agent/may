//! Syntax highlighting for tool-call command text and markdown code blocks.
//!
//! Mirrors the web UI's highlighted code card: one `Vec<Span>` per source
//! line, colored with a dark syntect theme (the TUI assumes a dark
//! terminal).

use std::sync::{Arc, LazyLock};

use ratatui::prelude::*;
use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

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

/// Highlights a source text line by line, carrying the syntax parse state
/// and the style state from one line to the next.
///
/// syntect's `HighlightLines` does the same but cannot be cloned; this
/// one can, so a renderer can highlight a line on a copy and keep its own
/// state where the last complete line left it.
#[derive(Debug, Clone)]
pub(super) struct LineHighlighter {
    /// The theme's selectors, shared by every copy.
    highlighter: Arc<Highlighter<'static>>,
    parse: ParseState,
    highlight: HighlightState,
}

/// A highlighter for `language` — a fence token like `rust` or `rs`
/// matched against syntax names and extensions; `None` when no syntax
/// knows it.
pub(super) fn highlighter(language: &str) -> Option<LineHighlighter> {
    SYNTAXES.find_syntax_by_token(language).map(|syntax| {
        let highlighter = Highlighter::new(&THEME);
        let highlight = HighlightState::new(&highlighter, ScopeStack::new());
        LineHighlighter {
            highlighter: Arc::new(highlighter),
            parse: ParseState::new(syntax),
            highlight,
        }
    })
}

/// Highlight one source line, keeping parse state in `highlighter` so
/// quotes, heredocs and open scopes carry across lines.
pub(super) fn line_spans(highlighter: &mut LineHighlighter, line: &str) -> Vec<Span<'static>> {
    let ops = highlighter
        .parse
        .parse_line(line, &SYNTAXES)
        .expect("bundled syntaxes highlight without missing references");
    HighlightIterator::new(
        &mut highlighter.highlight,
        &ops,
        line,
        &highlighter.highlighter,
    )
    .map(|(style, text)| Span::styled(text.to_string(), to_style(style)))
    .collect()
}

/// Highlight `script` as bash, returning one span vector per source line.
/// The script is highlighted as a unit so state (quotes, heredocs) carries
/// across lines. An empty script yields no lines.
pub(super) fn shell_spans(script: &str) -> Vec<Vec<Span<'static>>> {
    let mut highlighter = highlighter("bash").expect("syntect's default syntaxes include bash");
    script
        .lines()
        .map(|line| line_spans(&mut highlighter, line))
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
