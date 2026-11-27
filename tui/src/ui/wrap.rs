//! Word wrapping of styled lines to a display width.
//!
//! ratatui's `Paragraph` word wrapper (`Wrap { trim: false }`) can end a
//! row with a double-width glyph one cell past the area: it flushes a
//! pending word that leaves a single free column and then appends a
//! two-column glyph to it. The half-outside glyph lands on whatever is
//! drawn right of the area, and the buffer diff treats the next cell as
//! that glyph's trailing half and never sends it, so stale cells survive
//! on screen. Every row [`wrap_line`] yields fits its width, so the TUI
//! renders pre-wrapped rows ([`WrappedParagraph`]) and never asks
//! `Paragraph` to wrap.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, StyledGrapheme, Text};
use ratatui::widgets::{Block, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

/// Word-wrapped text in an optional block: the text is wrapped by
/// [`wrap_lines`] to the block's inner width, so no glyph reaches past
/// the inner area.
#[derive(Debug, Clone, Default)]
pub struct WrappedParagraph<'a> {
    text: Text<'a>,
    block: Option<Block<'a>>,
    style: Style,
    scroll: u16,
}

impl<'a> WrappedParagraph<'a> {
    /// Wrapped `text`, with no block, base style or scroll.
    #[must_use]
    pub fn new(text: impl Into<Text<'a>>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    /// Surround the text with `block`.
    #[must_use]
    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Base style of the whole area, under the text's own styles.
    #[must_use]
    pub fn style(mut self, style: impl Into<Style>) -> Self {
        self.style = style.into();
        self
    }

    /// Skip the first `rows` wrapped rows.
    #[must_use]
    pub const fn scroll(mut self, rows: u16) -> Self {
        self.scroll = rows;
        self
    }
}

impl Widget for WrappedParagraph<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        buf.set_style(area, self.style);
        let inner = self.block.map_or(area, |block| {
            let inner = block.inner(area);
            block.render(area, buf);
            inner
        });
        let wrapped = wrap_lines(&self.text.lines, inner.width);
        let mut rows = Text::from(wrapped.clone()).style(self.text.style);
        rows.alignment = self.text.alignment;
        Paragraph::new(rows)
            .scroll((self.scroll, 0))
            .render(inner, buf);
        paint_line_backgrounds(&wrapped, usize::from(self.scroll), inner, buf);
    }
}

/// Fills trailing empty cells of each rendered row with the row's
/// `Line::style` background (`ratatui`'s `Paragraph` only styles cells that
/// hold graphemes, leaving trailing cells past `line.width()` untouched).
pub fn paint_line_backgrounds(wrapped: &[Line<'_>], scroll: usize, area: Rect, buf: &mut Buffer) {
    for (dy, line) in wrapped
        .iter()
        .skip(scroll)
        .take(usize::from(area.height))
        .enumerate()
    {
        let Some(bg) = line.style.bg else {
            continue;
        };
        if bg == Color::Reset {
            continue;
        }
        let y = area.y + u16::try_from(dy).unwrap_or(u16::MAX);
        let drawn = u16::try_from(line.width())
            .unwrap_or(area.width)
            .min(area.width);
        for x in area.x + drawn..area.x + area.width {
            buf[(x, y)].set_bg(bg);
        }
    }
}

/// Rows of `line` wrapped to `width` display columns.
///
/// Breaks at explicit line breaks (`\n`, `\r\n`, `\r`); at whitespace, which
/// is dropped at the break; between wide (CJK) glyphs, each of which is its
/// own break opportunity; and inside a run of narrow glyphs only when the run
/// alone is wider than `width`. The line's leading whitespace (indentation) is
/// kept on its first row; continuation rows start at the next visible glyph.
/// A glyph wider than `width` is dropped, as nothing can show it. Every row
/// carries the line's style and alignment, and an empty line is one empty row.
/// A zero `width` has no rows.
#[must_use]
pub fn wrap_line(line: &Line<'_>, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    if has_newline(line) {
        return split_hard_breaks(line)
            .iter()
            .flat_map(|sub| wrap_single_line(sub, width))
            .collect();
    }
    wrap_single_line(line, width)
}

pub(super) fn has_newline(line: &Line<'_>) -> bool {
    line.spans
        .iter()
        .any(|span| span.content.contains(['\n', '\r']))
}

fn split_hard_breaks(line: &Line<'_>) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    for span in &line.spans {
        let text = span.content.as_ref();
        if !text.contains(['\n', '\r']) {
            current_spans.push(Span::styled(text.to_string(), span.style));
            continue;
        }
        let mut rest = text;
        while let Some(pos) = rest.find(['\n', '\r']) {
            let before = &rest[..pos];
            if !before.is_empty() {
                current_spans.push(Span::styled(before.to_string(), span.style));
            }
            let mut built = Line::from(std::mem::take(&mut current_spans)).style(line.style);
            built.alignment = line.alignment;
            out.push(built);
            let matched = rest.as_bytes()[pos];
            rest = &rest[pos + 1..];
            if matched == b'\r' && rest.starts_with('\n') {
                rest = &rest[1..];
            }
        }
        if !rest.is_empty() {
            current_spans.push(Span::styled(rest.to_string(), span.style));
        }
    }
    let mut built = Line::from(current_spans).style(line.style);
    built.alignment = line.alignment;
    out.push(built);
    out
}

fn wrap_single_line(line: &Line<'_>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }
    let row_line = |graphemes: Vec<StyledGrapheme<'_>>| {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for g in graphemes {
            match spans.last_mut() {
                Some(last) if last.style == g.style => last.content.to_mut().push_str(g.symbol),
                _ => spans.push(Span::styled(g.symbol.to_string(), g.style)),
            }
        }
        let mut row = Line::from(spans).style(line.style);
        row.alignment = line.alignment;
        row
    };
    if line.width() <= width {
        let mut spans = Vec::with_capacity(line.spans.len());
        for span in &line.spans {
            if span.content.is_empty() {
                continue;
            }
            spans.push(Span::styled(span.content.to_string(), span.style));
        }
        let mut row = Line::from(spans).style(line.style);
        row.alignment = line.alignment;
        return vec![row];
    }

    let graphemes: Vec<StyledGrapheme<'_>> = graphemes(line).collect();
    let is_space = |g: &StyledGrapheme<'_>| g.symbol.trim().is_empty();
    let mut rows = Vec::new();
    let mut current: Vec<StyledGrapheme<'_>> = Vec::new();
    let mut row_width = 0usize;
    let mut i = 0;
    while i < graphemes.len() {
        let g = &graphemes[i];
        let g_width = g.symbol.width();
        if is_space(g) {
            if row_width + g_width > width {
                rows.push(std::mem::take(&mut current));
                row_width = 0;
            } else if row_width > 0 || rows.is_empty() {
                current.push(g.clone());
                row_width += g_width;
            }
            i += 1;
            continue;
        }
        // One unbreakable token: a single wide glyph, or a run of narrow
        // visible glyphs.
        let end = if g_width > 1 {
            i + 1
        } else {
            graphemes[i..]
                .iter()
                .position(|g| is_space(g) || g.symbol.width() > 1)
                .map_or(graphemes.len(), |len| i + len)
        };
        let token_width: usize = graphemes[i..end].iter().map(|g| g.symbol.width()).sum();
        if row_width > 0 && row_width + token_width > width && token_width <= width {
            rows.push(std::mem::take(&mut current));
            row_width = 0;
        }
        for g in &graphemes[i..end] {
            let w = g.symbol.width();
            if w > width {
                continue;
            }
            if row_width + w > width {
                rows.push(std::mem::take(&mut current));
                row_width = 0;
            }
            current.push(g.clone());
            row_width += w;
        }
        i = end;
    }
    rows.push(current);
    let last = rows.len() - 1;
    rows.into_iter()
        .enumerate()
        .map(|(index, mut row)| {
            // Whitespace a break left at a row's end is not content.
            if index < last {
                while row.last().is_some_and(is_space) {
                    row.pop();
                }
            }
            row_line(row)
        })
        .collect()
}

/// Rows of every line of `lines` wrapped to `width`, in order.
#[must_use]
pub fn wrap_lines(lines: &[Line<'_>], width: u16) -> Vec<Line<'static>> {
    lines
        .iter()
        .flat_map(|line| wrap_line(line, width))
        .collect()
}

/// Number of rows [`wrap_line`] yields for `line` at `width`.
#[must_use]
pub fn line_rows(line: &Line<'_>, width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    // A single-line string that fits never wraps.
    if !has_newline(line) && line.width() <= usize::from(width) {
        return 1;
    }
    wrap_line(line, width).len()
}

/// The visible graphemes of `line`, each with its span's style.
fn graphemes<'a>(line: &'a Line<'_>) -> impl Iterator<Item = StyledGrapheme<'a>> {
    line.spans
        .iter()
        .flat_map(|span| span.styled_graphemes(Style::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    fn texts(rows: &[Line<'static>]) -> Vec<String> {
        rows.iter()
            .map(|row| row.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// The shapes ratatui's wrapper overflows: no row is wider than the
    /// width, at any width.
    #[test]
    fn no_row_exceeds_width() {
        for text in [
            "│ 构建 │ 编译整个工作区并运行所有单元测试",
            "x 编译整个工作区并运行所有单元测试编译整个工作区",
            "ab编译整个工作区并运行所有单元测试",
            "    indented code line with some words",
            "`cargo test --workspace --all-features --locked`",
        ] {
            let line = Line::from(text);
            for width in 1u16..40 {
                let rows = wrap_line(&line, width);
                assert_eq!(rows.len(), line_rows(&line, width));
                for row in &rows {
                    assert!(
                        row.width() <= usize::from(width),
                        "w={width} {text:?} -> {:?}",
                        texts(&rows)
                    );
                }
                // Only whitespace is lost, and only at breaks.
                let joined: String = texts(&rows).concat().replace(' ', "");
                let expected = if width < 2 {
                    // Wide glyphs cannot show in one column.
                    text.split(|c: char| unicode_width::UnicodeWidthChar::width(c) == Some(2))
                        .collect::<String>()
                        .replace(' ', "")
                } else {
                    text.replace(' ', "")
                };
                assert_eq!(joined, expected, "w={width}");
            }
        }
    }

    #[test]
    fn breaks_at_whitespace_and_between_wide_glyphs() {
        let line = Line::from(vec![
            Span::raw("宽字符"),
            Span::styled("ab cd", Style::default().add_modifier(Modifier::BOLD)),
        ]);
        assert_eq!(texts(&wrap_line(&line, 4)), ["宽字", "符ab", "cd"]);
        assert_eq!(
            texts(&wrap_line(&Line::from("abcdefghij"), 4)),
            ["abcd", "efgh", "ij"]
        );
        assert_eq!(
            texts(&wrap_line(&Line::from("one two three"), 8)),
            ["one two", "three"]
        );
    }

    #[test]
    fn keeps_indentation_and_style() {
        let line = Line::from("    let value = compute(argument);").style(Modifier::BOLD);
        let rows = wrap_line(&line, 16);
        assert_eq!(texts(&rows), ["    let value =", "compute(argument", ");"]);
        assert!(rows.iter().all(|row| row.style == line.style));
        assert_eq!(texts(&wrap_line(&Line::from(""), 10)), [""]);
        assert_eq!(wrap_line(&Line::from("x"), 0).len(), 0);
    }

    #[test]
    fn fast_path_preserves_exact_spans_styles_and_alignment() {
        use ratatui::layout::Alignment;
        use ratatui::style::Color;

        let s1 = Style::default().fg(Color::Cyan);
        let s2 = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let s3 = Style::default().fg(Color::Green);
        let line = Line::from(vec![
            Span::styled("hello ", s1),
            Span::styled("中文日本語한국어", s2),
            Span::raw(""),
            Span::styled(" end", s3),
        ])
        .style(Style::default().bg(Color::Black))
        .alignment(Alignment::Right);

        let rows = wrap_line(&line, 40);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.style, line.style);
        assert_eq!(row.alignment, Some(Alignment::Right));
        assert_eq!(
            row.spans,
            vec![
                Span::styled("hello ", s1),
                Span::styled("中文日本語한국어", s2),
                Span::styled(" end", s3),
            ]
        );
    }

    #[test]
    fn hard_newlines_break_rows_and_preserve_styles() {
        let styled = Line::from(vec![
            Span::styled("你好\nworld", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("\n\n안녕\r\n"),
        ]);
        let rows = wrap_line(&styled, 20);
        assert_eq!(texts(&rows), ["你好", "world", "", "안녕", ""]);
        assert_eq!(line_rows(&styled, 20), 5);
        assert_eq!(
            rows[0].spans[0].style,
            Style::default().add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            rows[1].spans[0].style,
            Style::default().add_modifier(Modifier::BOLD)
        );

        let wrapped_cjk = Line::from(Span::raw("你好世界\nこんにちは\n안녕하세요"));
        let rows = wrap_line(&wrapped_cjk, 6);
        assert_eq!(
            texts(&rows),
            ["你好世", "界", "こんに", "ちは", "안녕하", "세요"]
        );
        assert_eq!(line_rows(&wrapped_cjk, 6), 6);
    }
}
