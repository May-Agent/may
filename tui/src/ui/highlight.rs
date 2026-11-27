//! Syntax highlighting for tool-call command text and markdown code blocks.
//!
//! Mirrors the web UI's highlighted code card: one `Vec<Span>` per source
//! line, colored with a dark syntect theme (the TUI assumes a dark
//! terminal).

use std::sync::Arc;

use ratatui::prelude::*;
use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

/// The syntaxes and theme highlighting reads: loaded once when the TUI
/// starts, immutable afterwards. Cloning shares them.
#[derive(Debug, Clone)]
pub struct Highlighting {
    /// Bundled default syntaxes.
    syntaxes: Arc<SyntaxSet>,
    /// The dark theme's compiled selectors, built once.
    highlighter: Arc<Highlighter<'static>>,
}

impl Default for Highlighting {
    fn default() -> Self {
        let theme = ThemeSet::load_defaults()
            .themes
            .get("base16-ocean.dark")
            .expect("syntect's default themes include base16-ocean.dark")
            .clone();
        // syntect's `Highlighter` borrows its theme. The theme is loaded once
        // per TUI and read until the process exits, so it is leaked rather
        // than recompiled into selectors for every highlighted line.
        let theme: &'static Theme = Box::leak(Box::new(theme));
        Self {
            syntaxes: Arc::new(SyntaxSet::load_defaults_newlines()),
            highlighter: Arc::new(Highlighter::new(theme)),
        }
    }
}

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

impl Highlighting {
    /// A highlighter for `language` — a fence token like `rust` or `rs`
    /// matched against syntax names and extensions; `None` when no syntax
    /// knows it.
    pub(super) fn highlighter(&self, language: &str) -> Option<LineHighlighter> {
        self.syntaxes.find_syntax_by_token(language).map(|syntax| {
            let highlight = HighlightState::new(&self.highlighter, ScopeStack::new());
            LineHighlighter {
                highlighter: self.highlighter.clone(),
                parse: ParseState::new(syntax),
                highlight,
            }
        })
    }

    /// A highlighter inferred from `path`'s filename or extension without
    /// reading from disk; `None` for unknown or plain-text files.
    pub(super) fn highlighter_for_path(&self, path: &str) -> Option<LineHighlighter> {
        let p = std::path::Path::new(path);
        let syntax = p
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| self.syntaxes.find_syntax_by_extension(name))
            .or_else(|| {
                let ext = p.extension().and_then(|ext| ext.to_str())?;
                self.syntaxes.find_syntax_by_extension(ext).or_else(|| {
                    let mapped = match ext.to_ascii_lowercase().as_str() {
                        "ts" | "tsx" | "mts" | "cts" | "jsx" | "mjs" | "cjs" => "js",
                        "toml" => "ini",
                        "jsonc" => "json",
                        "zsh" => "sh",
                        "mdx" => "md",
                        _ => return None,
                    };
                    self.syntaxes.find_syntax_by_extension(mapped)
                })
            })?;
        if syntax.name == "Plain Text" {
            return None;
        }
        let highlight = HighlightState::new(&self.highlighter, ScopeStack::new());
        Some(LineHighlighter {
            highlighter: self.highlighter.clone(),
            parse: ParseState::new(syntax),
            highlight,
        })
    }

    /// Highlight one line of a diff without a trailing newline, feeding a
    /// synthetic `'\n'` to `parse_line` so single-line comments close at EOL,
    /// then stripping the trailing `'\n'` from the returned spans.
    pub(super) fn diff_line_spans(
        &self,
        highlighter: &mut LineHighlighter,
        line: &str,
    ) -> Vec<Span<'static>> {
        let mut with_nl = String::with_capacity(line.len() + 1);
        with_nl.push_str(line);
        with_nl.push('\n');
        let ops = highlighter
            .parse
            .parse_line(&with_nl, &self.syntaxes)
            .expect("bundled syntaxes highlight without missing references");
        let mut spans: Vec<Span<'static>> = HighlightIterator::new(
            &mut highlighter.highlight,
            &ops,
            &with_nl,
            &highlighter.highlighter,
        )
        .filter_map(|(style, text)| {
            let trimmed = text.strip_suffix('\n').unwrap_or(text);
            (!trimmed.is_empty()).then(|| Span::styled(trimmed.to_string(), to_style(style)))
        })
        .collect();
        if spans.is_empty() && !line.is_empty() {
            spans.push(Span::raw(line.to_string()));
        }
        spans
    }

    /// Highlight one source line, keeping parse state in `highlighter` so
    /// quotes, heredocs and open scopes carry across lines.
    pub(super) fn line_spans(
        &self,
        highlighter: &mut LineHighlighter,
        line: &str,
    ) -> Vec<Span<'static>> {
        let ops = highlighter
            .parse
            .parse_line(line, &self.syntaxes)
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

    /// Highlight `script` as `language`, a syntect token like `bash` or
    /// `python`; unknown languages fall back to bash so the script still
    /// reads as a command.
    pub(super) fn spans(&self, language: &str, script: &str) -> Vec<Vec<Span<'static>>> {
        let mut highlighter = self
            .highlighter(language)
            .or_else(|| self.highlighter("bash"))
            .expect("syntect's default syntaxes include bash");
        script
            .lines()
            .map(|line| self.line_spans(&mut highlighter, line))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Highlighting;

    fn shell_spans(script: &str) -> Vec<Vec<ratatui::text::Span<'static>>> {
        Highlighting::default().spans("bash", script)
    }

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
