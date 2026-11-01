//! Streaming-optimized markdown renderer for LLM output.
//!
//! Uses streamdown-parser for true incremental parsing - only processes
//! new lines as they arrive, maintaining parser state between chunks.

use ratatui::prelude::*;
use streamdown_parser::{InlineElement, InlineParser, ParseEvent, Parser};
use unicode_width::UnicodeWidthStr;

/// Height reserved for inline images (in terminal lines).
pub const IMAGE_HEIGHT: usize = 8;

/// Streaming markdown renderer with incremental parsing.
#[derive(Debug)]
pub struct StreamingMarkdown {
    /// The incremental parser (maintains state between chunks)
    parser: Parser,
    /// Inline parser for block contents the block parser leaves raw
    /// (list items, blockquote lines, table cells, headings)
    inline_parser: InlineParser,
    /// Raw content accumulated so far
    content: String,
    /// Number of complete lines already parsed
    parsed_line_count: usize,
    /// Rendered output lines
    rendered: Vec<Line<'static>>,
    /// Images found during parsing (alt, url)
    images: Vec<(String, String)>,
    /// Image positions: (url, `line_index`) for rendering at correct scroll position
    image_positions: Vec<(String, usize, String)>,
    /// Current incomplete line (not yet parsed)
    pending_line: String,
    /// Whether we're currently appending inline spans into an open visual line
    inline_open: bool,
    /// Whether terminal supports image rendering
    images_supported: bool,
    /// Index in `rendered` where the open table's first line sits; `None`
    /// when the block parser is not inside a table.
    table_start: Option<usize>,
    /// Lines the open table currently occupies in `rendered`.
    table_line_count: usize,
    /// Rendered cell spans of every row of the open table, header rows
    /// first, so a wider row streaming in later can re-align the rows
    /// already emitted.
    table_rows: Vec<Vec<Vec<Span<'static>>>>,
    /// Leading rows of `table_rows` emitted as `TableHeader`.
    table_header_rows: usize,
    /// Whether `TableSeparator` has arrived for the open table.
    table_has_separator: bool,
}

impl Default for StreamingMarkdown {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingMarkdown {
    /// Create a new streaming markdown renderer.
    /// Uses default (no image support). For image support, use `with_image_support()`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            parser: Parser::new(),
            inline_parser: InlineParser::new(),
            content: String::new(),
            parsed_line_count: 0,
            rendered: Vec::new(),
            images: Vec::new(),
            image_positions: Vec::new(),
            pending_line: String::new(),
            inline_open: false,
            images_supported: false,
            table_start: None,
            table_line_count: 0,
            table_rows: Vec::new(),
            table_header_rows: 0,
            table_has_separator: false,
        }
    }

    /// Create a new streaming markdown renderer with explicit image support flag.
    #[must_use]
    pub fn with_image_support(images_supported: bool) -> Self {
        Self {
            parser: Parser::new(),
            inline_parser: InlineParser::new(),
            content: String::new(),
            parsed_line_count: 0,
            rendered: Vec::new(),
            images: Vec::new(),
            image_positions: Vec::new(),
            pending_line: String::new(),
            inline_open: false,
            images_supported,
            table_start: None,
            table_line_count: 0,
            table_rows: Vec::new(),
            table_header_rows: 0,
            table_has_separator: false,
        }
    }

    /// Set whether images are supported (can be used to update after creation).
    pub const fn set_images_supported(&mut self, supported: bool) {
        self.images_supported = supported;
    }

    /// Append a chunk of markdown content.
    /// Only parses new complete lines incrementally.
    pub fn push(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }

        self.content.push_str(chunk);
        self.pending_line.push_str(chunk);

        // Process complete lines only
        while let Some(newline_pos) = self.pending_line.find('\n') {
            let line = self.pending_line[..newline_pos].to_string();
            self.pending_line = self.pending_line[newline_pos + 1..].to_string();
            self.parse_line(&line);
            self.parsed_line_count += 1;
        }
    }

    /// Parse a single line and render its events.
    fn parse_line(&mut self, line: &str) {
        let normalized = self.normalize_table_line(line);
        let line = normalized.as_deref().unwrap_or(line);
        for event in self.parser.parse_line(line) {
            self.render_event(event);
        }
    }

    /// streamdown-parser requires both outer pipes on a table row; GFM
    /// makes them optional and models drop the trailing one constantly.
    /// Add the missing pipes when the line is unambiguously part of a
    /// table — a pipe-bearing line inside an open table, or a pipe-led
    /// line the next unparsed line proves to be a header. Anything else
    /// passes through so it can still end the table.
    fn normalize_table_line(&self, line: &str) -> Option<String> {
        let trimmed = line.trim_end();
        let starts = trimmed.trim_start().starts_with('|');
        let ends = trimmed.ends_with('|') && trimmed.len() > 1;
        if trimmed.is_empty() || (starts && ends) {
            return None;
        }
        if self.table_start.is_some() {
            if !trimmed.contains('|') {
                return None;
            }
        } else {
            if !starts {
                return None;
            }
            let next = self.pending_line.lines().next().unwrap_or("");
            if !is_table_separator_line(next) {
                return None;
            }
        }
        let mut out = String::with_capacity(trimmed.len() + 4);
        if !starts {
            out.push_str("| ");
        }
        out.push_str(trimmed);
        if !ends {
            out.push_str(" |");
        }
        Some(out)
    }

    /// Clear all content and reset parser state.
    pub fn clear(&mut self) {
        self.parser = Parser::new();
        self.inline_parser.reset();
        self.content.clear();
        self.parsed_line_count = 0;
        self.rendered.clear();
        self.images.clear();
        self.image_positions.clear();
        self.pending_line.clear();
        self.inline_open = false;
        self.table_start = None;
        self.table_line_count = 0;
        self.table_rows.clear();
        self.table_header_rows = 0;
        self.table_has_separator = false;
    }

    /// Get the current raw content.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Flush any pending incomplete line.
    /// Call this after all content has been pushed to ensure the last line is parsed.
    pub fn flush(&mut self) {
        if !self.pending_line.is_empty() {
            let line = std::mem::take(&mut self.pending_line);
            self.parse_line(&line);
            self.parsed_line_count += 1;
        }
    }

    /// Get the rendered lines.
    /// Note: Call `flush()` first if you need to ensure incomplete lines are parsed.
    pub fn lines(&mut self) -> &[Line<'static>] {
        &self.rendered
    }

    /// Get images referenced in the content.
    #[must_use]
    pub fn images(&self) -> Vec<(String, String)> {
        self.images.clone()
    }

    /// Get image positions: (url, `line_index`, caption) for each image.
    /// The `line_index` is the position in the rendered output where the image should be displayed;
    /// `caption` is the path/URL shown under the zoomed image.
    #[must_use]
    pub fn image_positions(&self) -> &[(String, usize, String)] {
        &self.image_positions
    }

    fn push_inline_span(&mut self, span: Span<'static>) {
        if self.inline_open {
            if let Some(last) = self.rendered.last_mut() {
                last.spans.push(span);
            } else {
                self.rendered.push(Line::from(vec![span]));
            }
        } else {
            self.rendered.push(Line::from(vec![span]));
            self.inline_open = true;
        }
    }

    const fn end_inline_line(&mut self) {
        self.inline_open = false;
    }

    /// Record any images among `elements` in `images` for fetching. Inline
    /// images have no reserved rows and render as a link, so they are never
    /// registered in `image_positions` for the overlay.
    fn record_inline_images(&mut self, elements: &[InlineElement]) {
        for elem in elements {
            if let InlineElement::Image { alt, url } = elem {
                tracing::debug!("InlineElement::Image detected: alt={}, url={}", alt, url);
                self.images.push((alt.clone(), url.clone()));
            }
        }
    }

    /// Parse raw inline markdown carried by a block event into spans.
    ///
    /// Each element's own style is layered over `base_style`, so inline
    /// modifiers combine with the block's style and an element's own colour
    /// (link, inline code) wins over the block's. Images are recorded for
    /// the line that is about to be pushed (`self.rendered.len()`).
    fn block_inline_spans(&mut self, content: &str, base_style: Style) -> Vec<Span<'static>> {
        self.inline_parser.reset();
        let elements = self.inline_parser.parse(content);
        self.record_inline_images(&elements);
        elements
            .into_iter()
            .map(|elem| {
                let span = render_inline_element(elem);
                let style = base_style.patch(span.style);
                span.style(style)
            })
            .collect()
    }

    /// Render a parse event to output lines.
    fn render_event(&mut self, event: ParseEvent) {
        match event {
            ParseEvent::Text(text) => {
                self.push_inline_span(Span::raw(text));
            }
            ParseEvent::InlineCode(code) => {
                self.push_inline_span(Span::styled(
                    code,
                    Style::default().fg(Color::Yellow).bg(Color::DarkGray),
                ));
            }
            ParseEvent::Bold(text) => {
                self.push_inline_span(Span::styled(
                    text,
                    Style::default().add_modifier(Modifier::BOLD),
                ));
            }
            ParseEvent::Italic(text) => {
                self.push_inline_span(Span::styled(
                    text,
                    Style::default().add_modifier(Modifier::ITALIC),
                ));
            }
            ParseEvent::BoldItalic(text) => {
                self.push_inline_span(Span::styled(
                    text,
                    Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
                ));
            }
            ParseEvent::Strikeout(text) => {
                self.push_inline_span(Span::styled(
                    text,
                    Style::default().add_modifier(Modifier::CROSSED_OUT),
                ));
            }
            ParseEvent::Link { text, url } => {
                self.push_inline_span(Span::styled(
                    text,
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::UNDERLINED),
                ));
                self.push_inline_span(Span::styled(
                    format!(" ({url})"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ParseEvent::InlineElements(elements) => {
                self.record_inline_images(&elements);
                for elem in elements {
                    self.push_inline_span(render_inline_element(elem));
                }
            }
            ParseEvent::Image { alt, url } => {
                self.end_inline_line();
                tracing::debug!("ParseEvent::Image detected: alt={}, url={}", alt, url);
                self.images.push((alt.clone(), url.clone()));
                let line_index = self.rendered.len();
                self.image_positions
                    .push((url.clone(), line_index, url.clone()));
                self.render_image_placeholder(&alt, &url);
            }
            ParseEvent::Heading { level, content } => {
                self.end_inline_line();
                let style = match level {
                    1 => Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    2 => Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                    3 => Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                    _ => Style::default().add_modifier(Modifier::BOLD),
                };
                let spans = self.block_inline_spans(&content, style);
                self.rendered.push(Line::from(spans));
                self.rendered.push(Line::from(""));
            }
            ParseEvent::CodeBlockStart { language, .. } => {
                self.end_inline_line();
                if let Some(lang) = language {
                    self.rendered.push(Line::from(vec![
                        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            lang,
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]));
                }
            }
            ParseEvent::CodeBlockLine(line) => {
                self.end_inline_line();
                self.rendered.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        line,
                        Style::default().fg(Color::White).bg(Color::Rgb(30, 30, 30)),
                    ),
                ]));
            }
            ParseEvent::CodeBlockEnd
            | ParseEvent::ListEnd
            | ParseEvent::BlockquoteEnd
            | ParseEvent::EmptyLine => {
                self.end_inline_line();
                self.rendered.push(Line::from(""));
            }
            ParseEvent::TableEnd => {
                self.end_inline_line();
                self.table_start = None;
                self.table_line_count = 0;
                self.table_rows.clear();
                self.table_header_rows = 0;
                self.table_has_separator = false;
                self.rendered.push(Line::from(""));
            }
            ParseEvent::ListItem {
                indent,
                bullet,
                content,
            } => {
                self.end_inline_line();
                let indent_str = "  ".repeat(indent);
                let bullet_str = match bullet {
                    streamdown_parser::ListBullet::Dash
                    | streamdown_parser::ListBullet::Asterisk
                    | streamdown_parser::ListBullet::Plus
                    | streamdown_parser::ListBullet::PlusExpand => "• ".to_string(),
                    streamdown_parser::ListBullet::Ordered(n) => format!("{n}. "),
                };
                let mut spans = vec![Span::styled(
                    format!("{indent_str}{bullet_str}"),
                    Style::default().fg(Color::Cyan),
                )];
                spans.extend(self.block_inline_spans(&content, Style::default()));
                self.rendered.push(Line::from(spans));
            }
            ParseEvent::BlockquoteStart { .. } => {
                self.end_inline_line();
            }
            ParseEvent::BlockquoteLine(line) => {
                self.end_inline_line();
                let style = Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::ITALIC);
                let mut spans = vec![Span::styled("┃ ", Style::default().fg(Color::DarkGray))];
                spans.extend(self.block_inline_spans(&line, style));
                self.rendered.push(Line::from(spans));
            }
            ParseEvent::HorizontalRule => {
                self.end_inline_line();
                self.rendered.push(Line::from(Span::styled(
                    "─".repeat(40),
                    Style::default().fg(Color::DarkGray),
                )));
                self.rendered.push(Line::from(""));
            }
            ParseEvent::TableHeader(cells) => {
                self.end_inline_line();
                if self.table_start.is_none() {
                    self.table_start = Some(self.rendered.len());
                    self.table_line_count = 0;
                }
                let style = Style::default().add_modifier(Modifier::BOLD);
                let row = cells
                    .iter()
                    .map(|cell| self.block_inline_spans(cell, style))
                    .collect();
                self.table_rows.push(row);
                self.table_header_rows += 1;
                self.rebuild_table();
            }
            ParseEvent::TableSeparator => {
                self.end_inline_line();
                self.table_has_separator = true;
                self.rebuild_table();
            }
            ParseEvent::TableRow(cells) => {
                self.end_inline_line();
                let row = cells
                    .iter()
                    .map(|cell| self.block_inline_spans(cell, Style::default()))
                    .collect();
                self.table_rows.push(row);
                self.rebuild_table();
            }
            ParseEvent::Newline => {
                // Newline terminates the current logical line; it should not add
                // an extra blank visual row.
                self.end_inline_line();
            }
            _ => {}
        }
    }

    /// Render an image placeholder.
    /// When terminal supports images: reserves `IMAGE_HEIGHT` lines for the actual image.
    /// When no image support: shows as a styled link format.
    fn render_image_placeholder(&mut self, alt: &str, url: &str) {
        if self.images_supported {
            // Reserved blank rows the image overlay paints over.
            for _ in 0..IMAGE_HEIGHT {
                self.rendered.push(Line::from(""));
            }
        } else {
            // No image support - render as a styled link (compact, no reserved space)
            self.rendered.push(Line::from(vec![
                Span::styled(
                    alt.to_string(),
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                Span::styled(format!(" ({url})"), Style::default().fg(Color::DarkGray)),
            ]));
            self.rendered.push(Line::from(""));
        }
    }

    /// Re-render the open table's lines in place: every column is padded
    /// to the widest cell seen so far, so a wider row arriving later
    /// re-aligns the rows already emitted.
    fn rebuild_table(&mut self) {
        let Some(start) = self.table_start else {
            return;
        };
        let col_count = self.table_rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0usize; col_count];
        for row in &self.table_rows {
            for (col, cell) in row.iter().enumerate() {
                widths[col] = widths[col].max(cell_width(cell));
            }
        }

        let mut lines = Vec::with_capacity(self.table_rows.len() + 1);
        for (index, row) in self.table_rows.iter().enumerate() {
            if self.table_has_separator && index == self.table_header_rows {
                lines.push(table_separator_line(&widths));
            }
            lines.push(table_row_line(row, &widths));
        }
        if self.table_has_separator && self.table_rows.len() == self.table_header_rows {
            lines.push(table_separator_line(&widths));
        }

        let old_count = self.table_line_count;
        self.table_line_count = lines.len();
        self.rendered.splice(start..start + old_count, lines);
    }
}

/// Display width of a rendered cell.
fn cell_width(cell: &[Span<'static>]) -> usize {
    cell.iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// One table row line: `│ cell │ cell │`, cells padded to `widths`.
fn table_row_line(cells: &[Vec<Span<'static>>], widths: &[usize]) -> Line<'static> {
    let border = Style::default().fg(Color::DarkGray);
    let mut spans = vec![Span::styled("│ ", border)];
    for (col, width) in widths.iter().enumerate() {
        if col > 0 {
            spans.push(Span::styled(" │ ", border));
        }
        let filled = cells.get(col).map_or(0, |cell| {
            spans.extend(cell.iter().cloned());
            cell_width(cell)
        });
        if filled < *width {
            spans.push(Span::raw(" ".repeat(width - filled)));
        }
    }
    spans.push(Span::styled(" │", border));
    Line::from(spans)
}

/// The separator under the header, spanning the full table width:
/// `├──┼──┤`.
fn table_separator_line(widths: &[usize]) -> Line<'static> {
    let mut text = String::from("├");
    for (col, width) in widths.iter().enumerate() {
        if col > 0 {
            text.push('┼');
        }
        text.push_str(&"─".repeat(width + 2));
    }
    text.push('┤');
    Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
}

/// Whether `line` is a table delimiter row (`|---|---|`), pipes optional.
fn is_table_separator_line(line: &str) -> bool {
    let inner = line.trim();
    let inner = inner.strip_prefix('|').unwrap_or(inner);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner.contains('-')
        && inner
            .chars()
            .all(|c| matches!(c, '-' | ':' | '|' | ' ' | '\t'))
}

/// Render an inline element to a Span.
fn render_inline_element(elem: streamdown_parser::InlineElement) -> Span<'static> {
    match elem {
        streamdown_parser::InlineElement::Text(t) => Span::raw(t),
        streamdown_parser::InlineElement::Code(c) => {
            Span::styled(c, Style::default().fg(Color::Yellow).bg(Color::DarkGray))
        }
        streamdown_parser::InlineElement::Bold(t) => {
            Span::styled(t, Style::default().add_modifier(Modifier::BOLD))
        }
        streamdown_parser::InlineElement::Italic(t) => {
            Span::styled(t, Style::default().add_modifier(Modifier::ITALIC))
        }
        streamdown_parser::InlineElement::BoldItalic(t) => Span::styled(
            t,
            Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
        ),
        streamdown_parser::InlineElement::Strikeout(t) => {
            Span::styled(t, Style::default().add_modifier(Modifier::CROSSED_OUT))
        }
        streamdown_parser::InlineElement::Underline(t) => {
            Span::styled(t, Style::default().add_modifier(Modifier::UNDERLINED))
        }
        streamdown_parser::InlineElement::Link { text, url } => Span::styled(
            format!("{text} ({url})"),
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::UNDERLINED),
        ),
        streamdown_parser::InlineElement::Image { alt, url } => {
            // Inline images always render as link-style placeholder
            // (actual images are rendered as overlays, not inline)
            Span::styled(
                format!("{alt} ({url})"),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED),
            )
        }
        streamdown_parser::InlineElement::Footnote(f) => {
            Span::styled(format!("[^{f}]"), Style::default().fg(Color::DarkGray))
        }
    }
}

/// Extract images from markdown content.
#[must_use]
pub fn extract_images(content: &str) -> Vec<(String, String)> {
    let mut parser = Parser::new();
    let mut images = Vec::new();

    for line in content.lines() {
        for event in parser.parse_line(line) {
            if let ParseEvent::Image { alt, url } = event {
                images.push((alt, url));
            }
        }
    }

    images
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_rendering() {
        let mut md = StreamingMarkdown::new();
        md.push("# Hello\n\nWorld\n");
        let lines = md.lines();
        assert_ne!(lines, []);
    }

    #[test]
    fn test_incremental_streaming() {
        let mut md = StreamingMarkdown::new();
        md.push("# Head");
        // Incomplete line - not parsed yet
        assert_eq!(md.lines(), []);

        md.push("ing\n");
        // Now heading is complete
        assert_ne!(md.lines(), []);

        md.push("Para");
        md.push("graph\n");
        let lines = md.lines();
        assert!(lines.len() >= 2);
    }

    #[test]
    fn test_image_extraction() {
        let images = extract_images("![alt text](https://example.com/image.png)\n");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].0, "alt text");
        assert_eq!(images[0].1, "https://example.com/image.png");
    }

    #[test]
    fn test_image_rendering() {
        let mut md = StreamingMarkdown::new();
        md.push("![My Image](https://example.com/test.png)\n");
        let lines = md.lines();
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("My Image"));
        assert!(text.contains("https://example.com/test.png"));
    }

    #[test]
    fn test_horizontal_rule() {
        let mut md = StreamingMarkdown::new();
        md.push("Hello\n\n---\n\nWorld\n");
        md.flush();
        let lines = md.lines();
        let all_text: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        tracing::debug!("HR test lines: {:?}", all_text);
        let has_rule = all_text.iter().any(|t| t.contains('─'));
        assert!(has_rule, "Expected horizontal rule (─), got: {all_text:?}");
    }

    fn flattened_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect()
    }

    fn all_spans<'a>(lines: &'a [Line<'static>]) -> impl Iterator<Item = &'a Span<'static>> {
        lines.iter().flat_map(|l| l.spans.iter())
    }

    fn has_modifier(lines: &[Line<'static>], modifier: Modifier) -> bool {
        all_spans(lines).any(|s| s.style.add_modifier.contains(modifier))
    }

    #[test]
    fn test_list_item_inline_markdown() {
        let mut md = StreamingMarkdown::new();
        md.push("- **bold** and *italic* and `code` and [link](https://example.com)\n");
        let lines = md.lines();
        let text = flattened_text(lines);
        assert!(!text.contains("**"), "raw bold markers in: {text}");
        assert!(!text.contains('`'), "raw code markers in: {text}");
        assert!(text.contains("bold"), "missing bold text in: {text}");
        assert!(
            has_modifier(lines, Modifier::BOLD),
            "no BOLD span in: {lines:?}"
        );
        assert!(
            has_modifier(lines, Modifier::ITALIC),
            "no ITALIC span in: {lines:?}"
        );
        assert!(
            has_modifier(lines, Modifier::UNDERLINED),
            "no link span in: {lines:?}"
        );
    }

    #[test]
    fn test_list_item_cjk_bold() {
        let mut md = StreamingMarkdown::new();
        md.push("- **SpaceX**：上季营收\n");
        let lines = md.lines();
        let text = flattened_text(lines);
        assert!(!text.contains("**"), "raw bold markers in: {text}");
        assert!(text.contains("SpaceX"), "missing bold text in: {text}");
        assert!(text.contains("：上季营收"), "missing CJK text in: {text}");
        assert!(
            has_modifier(lines, Modifier::BOLD),
            "no BOLD span in: {lines:?}"
        );
    }

    #[test]
    fn test_blockquote_inline_markdown() {
        let mut md = StreamingMarkdown::new();
        md.push("> **quoted** text\n");
        let lines = md.lines();
        let text = flattened_text(lines);
        assert!(!text.contains("**"), "raw bold markers in: {text}");
        assert!(text.contains("quoted"), "missing bold text in: {text}");
        assert!(
            has_modifier(lines, Modifier::BOLD),
            "no BOLD span in: {lines:?}"
        );
        assert!(
            has_modifier(lines, Modifier::ITALIC),
            "no ITALIC base style in: {lines:?}"
        );
    }

    #[test]
    fn test_table_inline_markdown() {
        let mut md = StreamingMarkdown::new();
        md.push("| **head** | plain |\n|---|---|\n| *it* | `c` |\n");
        md.flush();
        let lines = md.lines();
        let text = flattened_text(lines);
        assert!(!text.contains("**"), "raw bold markers in: {text}");
        assert!(text.contains("head"), "missing header text in: {text}");
        assert!(
            has_modifier(lines, Modifier::BOLD),
            "no BOLD span in: {lines:?}"
        );
        assert!(
            has_modifier(lines, Modifier::ITALIC),
            "no ITALIC span in: {lines:?}"
        );
    }

    #[test]
    fn test_table_columns_aligned() {
        let mut md = StreamingMarkdown::new();
        md.push("| aa | b |\n|---|---|\n| c | dddd |\n");
        md.flush();
        let lines = md.lines();
        let rows: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(rows, ["│ aa │ b    │", "├────┼──────┤", "│ c  │ dddd │",]);
    }

    #[test]
    fn test_table_cjk_columns_aligned() {
        let mut md = StreamingMarkdown::new();
        md.push("| 子 issue | 状态 |\n|---|---|\n| #18 登录 | ❌ 无 |\n");
        md.flush();
        let widths: Vec<usize> = md
            .lines()
            .iter()
            .map(|l| {
                UnicodeWidthStr::width(
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                        .as_str(),
                )
            })
            .collect();
        assert!(
            widths[..3].windows(2).all(|w| w[0] == w[1]),
            "table rows misaligned: {widths:?}"
        );
    }

    #[test]
    fn test_table_missing_trailing_pipes() {
        // GFM makes outer pipes optional; models drop the trailing one.
        let mut md = StreamingMarkdown::new();
        md.push("| a | b\n|---|---\n| c | d\n");
        md.flush();
        let text = flattened_text(md.lines());
        assert!(!text.contains("| a | b"), "unparsed table line in: {text}");
        let rows: Vec<String> = md
            .lines()
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(rows[0], "│ a │ b │");
        assert_eq!(rows[1], "├───┼───┤");
        assert_eq!(rows[2], "│ c │ d │");
    }

    #[test]
    fn test_table_late_wide_row_realigns() {
        // A wider row streaming in later re-pads the rows already drawn.
        let mut md = StreamingMarkdown::new();
        md.push("| a | b |\n|---|---|\n| c | d |\n");
        md.push("| much longer | x |\n");
        md.flush();
        let rows: Vec<String> = md
            .lines()
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(rows[0], "│ a           │ b │");
        assert_eq!(rows[1], "├─────────────┼───┤");
        assert_eq!(rows[2], "│ c           │ d │");
        assert_eq!(rows[3], "│ much longer │ x │");
    }

    #[test]
    fn test_table_pipe_line_below_is_not_absorbed() {
        // A pipe-less paragraph after a table ends it instead of joining.
        let mut md = StreamingMarkdown::new();
        md.push("| a |\n|---|\n| b |\ntext after\n");
        md.flush();
        let rows: Vec<String> = md
            .lines()
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(rows[3], "");
        assert_eq!(rows[4], "text after");
    }

    #[test]
    fn test_heading_inline_markdown() {
        let mut md = StreamingMarkdown::new();
        md.push("## **Title** rest\n");
        let lines = md.lines();
        let text = flattened_text(lines);
        assert!(!text.contains("**"), "raw bold markers in: {text}");
        assert!(!text.contains('#'), "literal heading prefix in: {text}");
        assert!(text.contains("Title"), "missing heading text in: {text}");
        assert!(
            has_modifier(lines, Modifier::BOLD),
            "no BOLD span in: {lines:?}"
        );
    }

    #[test]
    fn test_block_inline_image_recorded() {
        let mut md = StreamingMarkdown::new();
        md.push("- ![diagram](https://example.com/diagram.png)\n");
        let images = md.images();
        assert_eq!(
            images,
            vec![(
                "diagram".to_string(),
                "https://example.com/diagram.png".to_string()
            )]
        );
        // Inline images render as a link — no reserved rows, so no overlay
        // position is registered.
        assert_eq!(md.image_positions(), []);
    }
}
