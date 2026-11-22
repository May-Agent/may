//! Streaming-optimized markdown renderer for LLM output.
//!
//! Uses streamdown-parser for true incremental parsing - only processes
//! new lines as they arrive, maintaining parser state between chunks.
//! [`IncrementalMarkdown`] renders a reply again after every append while
//! parsing each of its complete lines once.

use super::highlight::{Highlighting, LineHighlighter};
use super::wrap::wrap_line;
use ratatui::prelude::*;
use streamdown_parser::{InlineElement, InlineParser, ParseEvent, Parser};
use unicode_width::UnicodeWidthStr;

/// Height reserved for inline images (in terminal lines).
pub const IMAGE_HEIGHT: usize = 8;

/// Streaming markdown renderer with incremental parsing.
#[derive(Debug)]
pub struct StreamingMarkdown {
    /// Syntaxes and theme code blocks are highlighted with.
    highlighting: Highlighting,
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
    /// Display columns a table may span; its cells wrap to fit.
    /// `usize::MAX` when the output has no width bound.
    max_width: usize,
    /// Highlighter of the open fenced code block; `None` for a fence with
    /// no language or one syntect doesn't know. Holds parse state across
    /// streamed `CodeBlockLine`s.
    code_highlighter: Option<LineHighlighter>,
    /// The line, as the block parser received it, that opened the fenced
    /// code block the parser is in; `None` outside one. The parser keeps
    /// the fence it waits for private, so a [`Resume`] reopens the block
    /// by parsing this line again.
    code_fence_line: Option<String>,
    /// Calls into the block parser, for tests that check how often a line
    /// is parsed.
    #[cfg(test)]
    parsed_lines: usize,
    /// Code lines highlighted, for tests that check how often a line is
    /// highlighted.
    #[cfg(test)]
    highlighted_lines: usize,
}

impl StreamingMarkdown {
    /// Create a new streaming markdown renderer, highlighting code with
    /// `highlighting`. Uses default (no image support). For image support,
    /// use `with_image_support()`.
    #[must_use]
    pub fn new(highlighting: Highlighting) -> Self {
        Self {
            highlighting,
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
            max_width: usize::MAX,
            code_highlighter: None,
            code_fence_line: None,
            #[cfg(test)]
            parsed_lines: 0,
            #[cfg(test)]
            highlighted_lines: 0,
        }
    }

    /// Whether a [`Resume`] taken now renders every later line as this
    /// renderer would.
    ///
    /// It holds after every complete line except inside a table: the parser
    /// keeps whether it is in a table and past its delimiter row private,
    /// and a new row re-lays-out the rows before it. (`table_start` is set
    /// exactly while the parser is in a table: `TableHeader` opens both and
    /// `TableEnd` closes both.) No inline span is left open at the end of a
    /// line either, but a resume could not continue one.
    const fn resumable(&self) -> bool {
        !self.inline_open && self.table_start.is_none()
    }

    /// Where this renderer stands, for a renderer to continue from; only
    /// valid when [`Self::resumable`].
    fn resume(&self) -> Resume {
        Resume {
            parser: Parser::with_state(self.parser.state().clone()),
            code_fence_line: self.code_fence_line.clone(),
            code_highlighter: self.code_highlighter.clone(),
            parsed_any: true,
        }
    }

    /// Whether parsing `line` reads the line after it.
    ///
    /// A pipe-led line with no closing pipe outside a table becomes a table
    /// header only when the next line is a delimiter row (see
    /// [`Self::normalize_table_line`]).
    fn reads_next_line(&self, line: &str) -> bool {
        let pipes = Pipes::of(line);
        self.table_start.is_none() && pipes.starts && !pipes.ends
    }

    /// Create a new streaming markdown renderer with explicit image support flag.
    #[must_use]
    pub fn with_image_support(images_supported: bool, highlighting: Highlighting) -> Self {
        Self {
            images_supported,
            ..Self::new(highlighting)
        }
    }

    /// Bound tables to `width` display columns: a wider table wraps
    /// inside its cells, so no table line is wider than `width`.
    #[must_use]
    pub const fn with_width(mut self, width: usize) -> Self {
        self.max_width = width;
        self
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

        // Process complete lines only, walking the buffer by byte offset;
        // the unfinished tail is moved to the front once.
        let mut buffer = std::mem::take(&mut self.pending_line);
        let mut start = 0;
        while let Some(offset) = buffer[start..].find('\n') {
            let end = start + offset;
            self.parse_line(&buffer[start..end], &buffer[end + 1..]);
            self.parsed_line_count += 1;
            start = end + 1;
        }
        buffer.drain(..start);
        self.pending_line = buffer;
    }

    /// Parse a single line and render its events; `rest` is the text after
    /// the line.
    fn parse_line(&mut self, line: &str, rest: &str) {
        #[cfg(test)]
        {
            self.parsed_lines += 1;
        }
        let normalized = self.normalize_table_line(line, rest);
        let line = normalized.as_deref().unwrap_or(line);
        for event in self.parser.parse_line(line) {
            match event {
                ParseEvent::CodeBlockStart { .. } => self.code_fence_line = Some(line.to_string()),
                ParseEvent::CodeBlockEnd => self.code_fence_line = None,
                _ => {}
            }
            self.render_event(event);
        }
    }

    /// streamdown-parser requires both outer pipes on a table row; GFM
    /// makes them optional and models drop the trailing one constantly.
    /// Add the missing pipes when the line is unambiguously part of a
    /// table — a pipe-bearing line inside an open table, or a pipe-led
    /// line the next unparsed line proves to be a header. Anything else
    /// passes through so it can still end the table.
    fn normalize_table_line(&self, line: &str, rest: &str) -> Option<String> {
        let Pipes {
            trimmed,
            starts,
            ends,
        } = Pipes::of(line);
        if trimmed.is_empty() || (starts && ends) {
            return None;
        }
        if self.table_start.is_some() {
            if !trimmed.contains('|') {
                return None;
            }
        } else if !self.reads_next_line(line)
            || !is_table_separator_line(rest.lines().next().unwrap_or(""))
        {
            return None;
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
        self.code_highlighter = None;
        self.code_fence_line = None;
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
            self.parse_line(&line, "");
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
                self.push_inline_span(Span::styled(code, Style::default().fg(Color::Yellow)));
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
                self.code_highlighter = language
                    .as_deref()
                    .and_then(|language| self.highlighting.highlighter(language));
            }
            ParseEvent::CodeBlockLine(line) => {
                self.end_inline_line();
                let highlighted = self
                    .code_highlighter
                    .as_mut()
                    .map(|highlighter| self.highlighting.line_spans(highlighter, &line));
                #[cfg(test)]
                {
                    self.highlighted_lines += usize::from(highlighted.is_some());
                }
                let spans = highlighted.unwrap_or_else(|| vec![Span::raw(line)]);
                self.rendered.push(Line::from(spans));
            }
            ParseEvent::CodeBlockEnd => {
                self.end_inline_line();
                self.code_highlighter = None;
                self.rendered.push(Line::from(""));
            }
            ParseEvent::ListEnd | ParseEvent::BlockquoteEnd | ParseEvent::EmptyLine => {
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

    /// Re-render the open table's lines in place, laid out to `max_width`:
    /// column widths are recomputed from every row seen so far, so a wider
    /// row arriving later re-aligns the rows already emitted, and a cell
    /// that does not fit its column wraps inside it.
    fn rebuild_table(&mut self) {
        let Some(start) = self.table_start else {
            return;
        };
        let widths = column_widths(&self.table_rows, self.max_width);

        let mut lines = Vec::with_capacity(self.table_rows.len() + 1);
        for (index, row) in self.table_rows.iter().enumerate() {
            if self.table_has_separator && index == self.table_header_rows {
                lines.push(table_separator_line(&widths));
            }
            lines.extend(table_row_lines(row, &widths));
        }
        if self.table_has_separator && self.table_rows.len() == self.table_header_rows {
            lines.push(table_separator_line(&widths));
        }

        let old_count = self.table_line_count;
        self.table_line_count = lines.len();
        self.rendered.splice(start..start + old_count, lines);
    }
}

/// The pipes around a line, as a table row needs them.
struct Pipes<'a> {
    /// The line without trailing whitespace.
    trimmed: &'a str,
    /// The line starts with a pipe.
    starts: bool,
    /// The line ends with a pipe that is not its only character.
    ends: bool,
}

impl<'a> Pipes<'a> {
    fn of(line: &'a str) -> Self {
        let trimmed = line.trim_end();
        Self {
            trimmed,
            starts: trimmed.trim_start().starts_with('|'),
            ends: trimmed.ends_with('|') && trimmed.len() > 1,
        }
    }
}

/// Where a renderer stood after a complete line.
///
/// It is enough to render every later line as that renderer would. The
/// block parser cannot be cloned. Its state record can, and between
/// lines the parser carries nothing else but three private fields: the
/// fence an open fenced code block waits for, restored by parsing the
/// line that opened the block again; whether the previous line was empty,
/// restored by parsing a blank line again; and its table state, which
/// [`StreamingMarkdown::resumable`] rules out. Its inline parser starts
/// every line afresh.
#[derive(Debug)]
struct Resume {
    /// A parser holding the state record to continue from; only the record
    /// is read.
    parser: Parser,
    code_fence_line: Option<String>,
    code_highlighter: Option<LineHighlighter>,
    /// Whether a line was parsed: a fresh state record says the previous
    /// line was empty, which a fresh parser does not act on.
    parsed_any: bool,
}

impl Resume {
    /// Where a renderer of no text stands.
    fn start() -> Self {
        Self {
            parser: Parser::new(),
            code_fence_line: None,
            code_highlighter: None,
            parsed_any: false,
        }
    }

    /// A renderer, with nothing rendered yet, that continues from here.
    fn renderer(
        &self,
        images_supported: bool,
        max_width: usize,
        highlighting: &Highlighting,
    ) -> StreamingMarkdown {
        let mut state = self.parser.state().clone();
        let reopen = self.code_fence_line.as_deref();
        if reopen.is_some() {
            // Inside the block the record is unchanged since the opening
            // line, so leaving the block and parsing that line again sets
            // the private fence, and the "previous line empty" flag the
            // opening line cleared.
            state.exit_code_block();
        }
        let replay = if reopen.is_some() {
            reopen
        } else {
            (self.parsed_any && state.last_line_empty).then_some("")
        };
        let mut parser = Parser::with_state(state);
        if let Some(line) = replay {
            // Only the parser's state matters: the line rendered before.
            parser.parse_line(line);
        }
        StreamingMarkdown {
            parser,
            code_highlighter: self.code_highlighter.clone(),
            code_fence_line: self.code_fence_line.clone(),
            #[cfg(test)]
            parsed_lines: usize::from(replay.is_some()),
            ..StreamingMarkdown::with_image_support(images_supported, highlighting.clone())
                .with_width(max_width)
        }
    }
}

/// A reply rendered again after every append, each complete line parsed
/// once.
///
/// The text arrives as settled text, which no later frame changes,
/// followed by a tail that may still change. A frame resumes a renderer
/// where the last committed line left it (`Resume`) and parses the
/// settled lines not committed yet, then the tail. A complete settled line
/// is committed — its rendered lines kept, the renderer's state saved to
/// resume from — once the renderer is resumable after it
/// (`StreamingMarkdown::resumable`) and the line does not read a next line
/// that is unfinished (`StreamingMarkdown::reads_next_line`). Every
/// complete line is committed as it completes, fenced code lines included
/// (each is parsed and highlighted once), except the lines of an open
/// table, parsed again each frame until a line ends the table, and a
/// pipe-led line, which waits for the line after it.
///
/// The committed lines followed by the open lines are the render of the
/// whole text in one pass.
#[derive(Debug)]
pub struct IncrementalMarkdown {
    highlighting: Highlighting,
    images_supported: bool,
    max_width: usize,
    /// Settled text after the last committed line.
    settled: String,
    /// The rendered lines of every committed line.
    lines: Vec<Line<'static>>,
    /// `(url, line index, caption)` of the images among `lines`.
    image_positions: Vec<(String, usize, String)>,
    /// Where the renderer stood after the last committed line.
    resume: Resume,
    /// Calls into the block parser so far.
    #[cfg(test)]
    parsed_lines: usize,
    /// Code lines highlighted so far.
    #[cfg(test)]
    highlighted_lines: usize,
}

/// One frame of an [`IncrementalMarkdown`].
#[derive(Debug)]
pub struct MarkdownFrame<'a> {
    /// Rendered lines no later text changes.
    pub committed: &'a [Line<'static>],
    /// `(url, line index, caption)` of the images among `committed`.
    pub committed_images: &'a [(String, usize, String)],
    /// The rendered lines after `committed`.
    pub open: Vec<Line<'static>>,
    /// The images among `open`, their line indices counted from the first
    /// committed line.
    pub open_images: Vec<(String, usize, String)>,
}

impl IncrementalMarkdown {
    /// A renderer of an empty reply; tables are laid out to `max_width`
    /// display columns.
    #[must_use]
    pub fn new(images_supported: bool, max_width: usize, highlighting: Highlighting) -> Self {
        Self {
            highlighting,
            images_supported,
            max_width,
            settled: String::new(),
            lines: Vec::new(),
            image_positions: Vec::new(),
            resume: Resume::start(),
            #[cfg(test)]
            parsed_lines: 0,
            #[cfg(test)]
            highlighted_lines: 0,
        }
    }

    /// Render the reply with `settled` appended to its settled text.
    ///
    /// `tail` follows the settled text and replaces the previous frame's.
    pub fn render(&mut self, settled: &str, tail: &str) -> MarkdownFrame<'_> {
        self.settled.push_str(settled);
        let settled_len = self.settled.len();
        let mut text = String::with_capacity(settled_len + tail.len());
        text.push_str(&self.settled);
        text.push_str(tail);

        let mut renderer =
            self.resume
                .renderer(self.images_supported, self.max_width, &self.highlighting);
        let mut committing = true;
        let mut committed = 0;
        let mut start = 0;
        while let Some(offset) = text[start..].find('\n') {
            let end = start + offset;
            let line = &text[start..end];
            committing = committing
                && end < settled_len
                && (!renderer.reads_next_line(line) || text[end + 1..settled_len].contains('\n'));
            renderer.parse_line(line, &text[end + 1..]);
            start = end + 1;
            if committing && renderer.resumable() {
                self.commit(&mut renderer);
                committed = start;
            }
        }
        if start < text.len() {
            renderer.parse_line(&text[start..], "");
        }
        self.settled.drain(..committed);
        #[cfg(test)]
        {
            self.parsed_lines += renderer.parsed_lines;
            self.highlighted_lines += renderer.highlighted_lines;
        }

        let offset = self.lines.len();
        MarkdownFrame {
            committed: &self.lines,
            committed_images: &self.image_positions,
            open: renderer.rendered,
            open_images: renderer
                .image_positions
                .into_iter()
                .map(|(url, line, caption)| (url, line + offset, caption))
                .collect(),
        }
    }

    /// Keep what `renderer` rendered and continue from where it stands.
    fn commit(&mut self, renderer: &mut StreamingMarkdown) {
        let offset = self.lines.len();
        self.lines.append(&mut renderer.rendered);
        self.image_positions.extend(
            renderer
                .image_positions
                .drain(..)
                .map(|(url, line, caption)| (url, line + offset, caption)),
        );
        renderer.images.clear();
        self.resume = renderer.resume();
    }

    /// Calls into the block parser so far.
    #[cfg(test)]
    const fn parsed_lines(&self) -> usize {
        self.parsed_lines
    }

    /// Code lines highlighted so far.
    #[cfg(test)]
    const fn highlighted_lines(&self) -> usize {
        self.highlighted_lines
    }
}

/// Display width of a rendered cell.
fn cell_width(cell: &[Span<'static>]) -> usize {
    cell.iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// Content width of each column of a table laid out to `max_width`
/// display columns.
///
/// Columns get their widest cell when the whole table fits. Otherwise the
/// space left after the borders is shared out narrowest column first: a
/// column whose cells fit its fair share keeps its natural width and the
/// rest is split among the wider ones, whose cells then wrap. A column is
/// never narrower than its widest grapheme, so only an area too narrow
/// for the borders and one glyph per column can exceed `max_width`.
fn column_widths(rows: &[Vec<Vec<Span<'static>>>], max_width: usize) -> Vec<usize> {
    let col_count = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut natural = vec![0usize; col_count];
    let mut floor = vec![1usize; col_count];
    for row in rows {
        for (col, cell) in row.iter().enumerate() {
            natural[col] = natural[col].max(cell_width(cell));
            let widest = cell
                .iter()
                .flat_map(|span| span.styled_graphemes(Style::default()))
                .map(|g| g.symbol.width())
                .max()
                .unwrap_or(0);
            floor[col] = floor[col].max(widest);
        }
    }
    // `│ ` + ` │ ` between columns + ` │`.
    let frame = 3 * col_count + 1;
    let available = max_width.saturating_sub(frame);
    if natural.iter().sum::<usize>() <= available {
        return natural;
    }
    let mut order: Vec<usize> = (0..col_count).collect();
    order.sort_by_key(|&col| natural[col]);
    let mut widths = vec![0usize; col_count];
    let mut remaining = available;
    for (placed, col) in order.into_iter().enumerate() {
        let share = remaining / (col_count - placed);
        let width = natural[col].min(share).max(floor[col]);
        widths[col] = width;
        remaining = remaining.saturating_sub(width);
    }
    widths
}

/// The display lines of one table row: `│ cell │ cell │`, every cell
/// wrapped (by [`wrap_line`]) to and padded to its column's width, so a
/// row too wide for the area becomes several lines whose borders stay
/// aligned.
fn table_row_lines(cells: &[Vec<Span<'static>>], widths: &[usize]) -> Vec<Line<'static>> {
    let border = Style::default().fg(Color::DarkGray);
    let wrapped: Vec<Vec<Line<'static>>> = widths
        .iter()
        .enumerate()
        .map(|(col, &width)| {
            cells.get(col).map_or_else(Vec::new, |cell| {
                let width = u16::try_from(width).unwrap_or(u16::MAX);
                wrap_line(&Line::from(cell.clone()), width)
            })
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(0).max(1);
    (0..height)
        .map(|row| {
            let mut spans = vec![Span::styled("│ ", border)];
            for (col, width) in widths.iter().enumerate() {
                if col > 0 {
                    spans.push(Span::styled(" │ ", border));
                }
                let filled = wrapped[col].get(row).map_or(0, |part| {
                    spans.extend(part.spans.iter().cloned());
                    part.width()
                });
                if filled < *width {
                    spans.push(Span::raw(" ".repeat(width - filled)));
                }
            }
            spans.push(Span::styled(" │", border));
            Line::from(spans)
        })
        .collect()
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
            Span::styled(c, Style::default().fg(Color::Yellow))
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
        let mut md = StreamingMarkdown::new(Highlighting::default());
        md.push("# Hello\n\nWorld\n");
        let lines = md.lines();
        assert_ne!(lines, []);
    }

    #[test]
    fn test_incremental_streaming() {
        let mut md = StreamingMarkdown::new(Highlighting::default());
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
        let mut md = StreamingMarkdown::new(Highlighting::default());
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
        let mut md = StreamingMarkdown::new(Highlighting::default());
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
        let mut md = StreamingMarkdown::new(Highlighting::default());
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
        let mut md = StreamingMarkdown::new(Highlighting::default());
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
        let mut md = StreamingMarkdown::new(Highlighting::default());
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
        let mut md = StreamingMarkdown::new(Highlighting::default());
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
        let mut md = StreamingMarkdown::new(Highlighting::default());
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
        let mut md = StreamingMarkdown::new(Highlighting::default());
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
        let mut md = StreamingMarkdown::new(Highlighting::default());
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
        let mut md = StreamingMarkdown::new(Highlighting::default());
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

    /// A three-column table of CJK cells and a long inline code span, the
    /// shape that overflowed the chat column.
    const WIDE_TABLE: &str = "| 项目 | 说明 | 命令 |\n|---|---|---|\n| 构建 | 编译整个工作区并运行所有单元测试，确认没有回归 | `cargo test --workspace --all-features --locked` |\n| 格式化 | 检查代码风格 | `cargo fmt --all -- --check` |\n";

    /// Display column of every border glyph on `line`.
    fn border_columns(line: &Line<'static>) -> Vec<usize> {
        let mut columns = Vec::new();
        let mut x = 0;
        for g in line
            .spans
            .iter()
            .flat_map(|s| s.styled_graphemes(Style::default()))
        {
            if matches!(g.symbol, "│" | "├" | "┼" | "┤") {
                columns.push(x);
            }
            x += g.symbol.width();
        }
        columns
    }

    /// Every line of the table `lines` fits `width`, puts its borders at
    /// the header's columns, and exactly one separator rule is drawn.
    fn assert_table_layout(lines: &[Line<'static>], width: usize) {
        assert_open_table_layout(lines, width, 1);
    }

    /// [`assert_table_layout`] for a table that may still be streaming:
    /// `rules` separator rules are expected.
    fn assert_open_table_layout(lines: &[Line<'static>], width: usize, rules: usize) {
        let table: Vec<&Line<'static>> = lines
            .iter()
            .filter(|l| !border_columns(l).is_empty())
            .collect();
        assert!(!table.is_empty(), "no table lines in {lines:?}");
        let header = border_columns(table[0]);
        for line in &table {
            assert!(
                line.width() <= width,
                "line wider than {width}: {} ({line:?})",
                line.width()
            );
            assert_eq!(border_columns(line), header, "misaligned: {line:?}");
        }
        let drawn = table
            .iter()
            .filter(|l| l.spans.first().is_some_and(|s| s.content.starts_with('├')))
            .count();
        assert_eq!(drawn, rules, "separator rules in {lines:?}");
    }

    /// Text of each column of the table `lines`, its wrapped pieces
    /// joined, rows in order.
    fn column_text(lines: &[Line<'static>], col: usize) -> String {
        lines
            .iter()
            .filter(|l| l.spans.first().is_some_and(|s| s.content.starts_with('│')))
            .map(|l| {
                let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                text.split('│')
                    .nth(col + 1)
                    .unwrap_or("")
                    .trim()
                    .to_string()
            })
            .collect::<String>()
            .replace(' ', "")
    }

    #[test]
    fn test_table_wraps_cells_to_width() {
        for width in [40, 56, 72] {
            let mut md = StreamingMarkdown::new(Highlighting::default()).with_width(width);
            md.push(WIDE_TABLE);
            md.flush();
            let lines = md.lines().to_vec();
            assert_table_layout(&lines, width);
            // Rows wrapped into several lines: more lines than rows + rule.
            assert!(lines.iter().filter(|l| l.width() > 0).count() > 4);
            // No cell text is lost or spilled outside its column.
            assert_eq!(column_text(&lines, 0), "项目构建格式化");
            assert_eq!(
                column_text(&lines, 1),
                "说明编译整个工作区并运行所有单元测试，确认没有回归检查代码风格"
            );
            assert_eq!(
                column_text(&lines, 2),
                "命令cargotest--workspace--all-features--lockedcargofmt--all----check"
            );
        }
    }

    #[test]
    fn test_table_wider_area_keeps_one_line_rows() {
        let mut md = StreamingMarkdown::new(Highlighting::default()).with_width(200);
        md.push(WIDE_TABLE);
        md.flush();
        let lines: Vec<Line<'static>> = md
            .lines()
            .iter()
            .filter(|l| l.width() > 0)
            .cloned()
            .collect();
        assert_table_layout(&lines, 200);
        // Header, rule, two rows: nothing wrapped.
        assert_eq!(lines.len(), 4, "{lines:?}");
        let natural = lines[0].width();
        assert!(natural < 200);
        assert!(lines.iter().all(|l| l.width() == natural));
    }

    #[test]
    fn test_streaming_table_relayouts_open_table() {
        // Each arriving row re-lays-out the whole open table to the width.
        let width = 48;
        let mut md = StreamingMarkdown::new(Highlighting::default()).with_width(width);
        let mut lines_seen = 0;
        for (index, line) in WIDE_TABLE.split_inclusive('\n').enumerate() {
            md.push(line);
            if md.lines().is_empty() {
                continue;
            }
            assert_open_table_layout(md.lines(), width, usize::from(index > 0));
            assert!(md.lines().len() >= lines_seen);
            lines_seen = md.lines().len();
        }
        md.push("\nafter\n");
        md.flush();
        assert_table_layout(md.lines(), width);
        let text = flattened_text(md.lines());
        assert!(text.ends_with("after"), "{text}");
    }

    #[test]
    fn test_table_pipe_line_below_is_not_absorbed() {
        // A pipe-less paragraph after a table ends it instead of joining.
        let mut md = StreamingMarkdown::new(Highlighting::default());
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
        let mut md = StreamingMarkdown::new(Highlighting::default());
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
    fn test_code_block_highlighted() {
        let mut md = StreamingMarkdown::new(Highlighting::default());
        md.push("```rust\nlet x = 1;\n```\n");
        md.flush();
        let lines = md.lines();
        let code = &lines[0];
        // More than one styled region: keywords got colored.
        assert!(code.spans.len() > 1, "not highlighted: {code:?}");
        assert!(
            code.spans
                .iter()
                .all(|s| s.style.bg.is_none() && s.content.as_ref() != "│ "),
            "code line carries a gutter or a background: {code:?}"
        );
        // The `rust` fence name is not rendered as a label line.
        let text = flattened_text(lines);
        assert!(!text.contains("rust"), "language label leaked: {text}");
    }

    #[test]
    fn test_code_block_unknown_language_plain() {
        let mut md = StreamingMarkdown::new(Highlighting::default());
        md.push("```notalang\nlet x = 1;\n```\n");
        md.flush();
        let code = &md.lines()[0];
        assert_eq!(code.spans.len(), 1, "expected one plain span: {code:?}");
        assert_eq!(code.spans[0].style, Style::default());
    }

    #[test]
    fn test_code_block_highlight_state_across_lines() {
        // A comment opened on one line stays open into the next: the
        // second line's code spans all carry the comment color, which
        // differs from the default white of an unhighlighted block.
        let mut md = StreamingMarkdown::new(Highlighting::default());
        md.push("```rust\n/* start\nend */\n```\n");
        md.flush();
        let lines = md.lines();
        let colors: Vec<Color> = lines[1].spans.iter().filter_map(|s| s.style.fg).collect();
        assert_ne!(colors, [] as [Color; 0]);
        assert!(
            colors.iter().all(|c| *c == colors[0]),
            "comment color broke across lines: {colors:?}"
        );
        assert_ne!(colors[0], Color::White);
    }

    #[test]
    fn test_block_inline_image_recorded() {
        let mut md = StreamingMarkdown::new(Highlighting::default());
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

    /// A reply with a heading, CJK text (Chinese, Japanese, Korean), a
    /// link, inline code, a list, a table whose rows drop the trailing pipe
    /// (so a row's rendering waits for the delimiter line after it), an
    /// image, a fenced code block holding a blank line, and an unclosed
    /// bold that closes in a later paragraph.
    const REPLY: &str = "# 概要 Overview\n\
\n\
中文、日本語、한국어 mixed with a [link](https://example.com/路径) and `code`.\n\
\n\
- 第一 item **bold**\n\
- 二つ目 item\n\
- 세 번째 item\n\
\n\
| 名前 | 値\n\
|---|---|\n\
| あ | 1\n\
| 가 | 2\n\
\n\
![図](https://example.com/d.png)\n\
\n\
```rust\n\
fn main() {\n\
\x20   let s = \"你好\";\n\
\n\
\x20   println!(\"{s}\");\n\
}\n\
```\n\
\n\
An **unclosed bold 한국어\n\
\n\
closes here** and ends.\n\
\n";

    /// Lines of `REPLY`'s table, parsed again each frame while it is open.
    const TABLE_LINES: usize = 4;

    /// Block-parser calls a frame makes besides the lines it completes and
    /// an open table's: the unfinished line, and the line replayed to
    /// resume (a blank line, or the line that opened a fence).
    const FRAME_OVERHEAD: usize = 2;

    /// Text exercising every way a renderer resumes: a first line that
    /// sets the indent, a fence inside a list, a `~~~` fence holding a
    /// backtick fence, a quote, a think block, a table without closing
    /// pipes followed directly by a fence, and a pipe-led line that is not
    /// a table.
    const EDGES: &str = "  Indented 先頭 첫 首行\n\
- item one\n\
```sh\n\
echo \"in list\"\n\
```\n\
- item two\n\
\n\
~~~\n\
```\n\
still code 코드\n\
~~~\n\
> quoted **강조** 引用\n\
> more\n\
\n\
<think>\n\
hidden 考え\n\
</think>\n\
| a | b\n\
|---|---\n\
| 1 | 2\n\
```\n\
after table fence\n\
```\n\
\n\
| lone pipe line\n\
plain 普通\n";

    const WIDTH: usize = 40;

    /// `text` cut every `size` characters, always at a character boundary.
    fn chunks(text: &str, size: usize) -> Vec<&str> {
        let mut out = Vec::new();
        let mut start = 0;
        let mut taken = 0;
        for (index, _) in text.char_indices() {
            if taken == size {
                out.push(&text[start..index]);
                start = index;
                taken = 0;
            }
            taken += 1;
        }
        out.push(&text[start..]);
        out
    }

    /// Rendered lines and image positions.
    type Rendered = (Vec<Line<'static>>, Vec<(String, usize, String)>);

    /// The whole render a frame describes.
    fn whole(frame: &MarkdownFrame<'_>) -> Rendered {
        let mut lines = frame.committed.to_vec();
        lines.extend(frame.open.iter().cloned());
        let mut images = frame.committed_images.to_vec();
        images.extend(frame.open_images.iter().cloned());
        (lines, images)
    }

    /// A reply streamed into an [`IncrementalMarkdown`] the way a reply's
    /// text reaches it: every complete line settled, the unfinished line
    /// as the tail.
    struct Stream {
        highlighting: Highlighting,
        markdown: IncrementalMarkdown,
        text: String,
        settled: usize,
    }

    impl Stream {
        fn new(images_supported: bool) -> Self {
            let highlighting = Highlighting::default();
            Self {
                markdown: IncrementalMarkdown::new(images_supported, WIDTH, highlighting.clone()),
                highlighting,
                text: String::new(),
                settled: 0,
            }
        }

        /// The render of the text so far in one pass.
        fn one_shot(&self, images_supported: bool) -> Rendered {
            let mut md =
                StreamingMarkdown::with_image_support(images_supported, self.highlighting.clone())
                    .with_width(WIDTH);
            md.push(&self.text);
            md.flush();
            (md.lines().to_vec(), md.image_positions().to_vec())
        }

        /// Append `chunk` and render.
        fn push(&mut self, chunk: &str) -> Rendered {
            self.text.push_str(chunk);
            let cut = self.text.rfind('\n').map_or(0, |at| at + 1);
            let frame = self
                .markdown
                .render(&self.text[self.settled..cut], &self.text[cut..]);
            self.settled = cut;
            whole(&frame)
        }
    }

    #[test]
    fn push_walks_lines_without_losing_the_tail() {
        let mut md = StreamingMarkdown::new(Highlighting::default());
        md.push("a\nb\nc");
        assert_eq!(md.parsed_line_count, 2);
        assert_eq!(md.pending_line, "c");
        md.push("d\n\ne\n");
        assert_eq!(md.parsed_line_count, 5);
        assert_eq!(md.pending_line, "");
    }

    #[test]
    fn incremental_render_equals_one_shot_and_parses_each_line_once() {
        let reply = REPLY.repeat(3);
        let lines_in_reply = reply.lines().count();
        for images_supported in [false, true] {
            for size in [3, 5, 7] {
                let mut stream = Stream::new(images_supported);
                let mut naive_parses = 0;
                let mut last_parses = 0;
                let mut frames = 0;
                let mut shown = Vec::new();
                for chunk in chunks(&reply, size) {
                    let (lines, images) = stream.push(chunk);
                    let (expected_lines, expected_images) = stream.one_shot(images_supported);
                    assert_eq!(
                        lines,
                        expected_lines,
                        "lines differ after {} bytes (chunks of {size})",
                        stream.text.len()
                    );
                    assert_eq!(images, expected_images);

                    // A chunk parses the lines it completes, an open
                    // table's lines and the frame's overhead; re-rendering
                    // the whole text would parse every line.
                    let parses = stream.markdown.parsed_lines() - last_parses;
                    last_parses = stream.markdown.parsed_lines();
                    assert!(
                        parses <= size + TABLE_LINES + FRAME_OVERHEAD,
                        "{parses} lines parsed for one append of {size} characters"
                    );
                    naive_parses += stream.text.lines().count();
                    frames += 1;
                    shown = lines;
                }
                let parsed = stream.markdown.parsed_lines();
                assert!(
                    parsed <= lines_in_reply + frames * (TABLE_LINES + FRAME_OVERHEAD),
                    "{parsed} parses for {lines_in_reply} lines in {frames} frames"
                );
                assert!(
                    parsed * 4 < naive_parses,
                    "{parsed} lines parsed against {naive_parses} for whole-text renders"
                );
                let shown = flattened_text(&shown);
                assert!(shown.contains("https://example.com/路径"));
                assert!(shown.contains("한국어"));
            }
        }
    }

    #[test]
    fn incremental_render_resumes_inside_every_block() {
        for size in [1, 2, 3] {
            let mut stream = Stream::new(false);
            for chunk in chunks(EDGES, size) {
                let rendered = stream.push(chunk);
                assert_eq!(
                    rendered,
                    stream.one_shot(false),
                    "render differs after {:?} (chunks of {size})",
                    stream.text
                );
            }
        }
    }

    #[test]
    fn open_fence_parses_and_highlights_each_code_line_once() {
        use std::fmt::Write as _;

        const CODE_LINES: usize = 300;
        const SIZE: usize = 4;
        let mut reply = String::from("Intro 说明 説明 설명:\n\n```rust\n");
        for index in 0..CODE_LINES {
            writeln!(
                reply,
                "    let value_{index} = compute({index}); // 行 {index} 줄"
            )
            .expect("writing to a String cannot fail");
        }
        reply.push_str("```\n\nDone.\n");
        let lines_in_reply = reply.lines().count();

        let mut stream = Stream::new(false);
        let mut frames = 0;
        let mut last_parses = 0;
        let mut last_highlights = 0;
        let pieces = chunks(&reply, SIZE);
        for (index, chunk) in pieces.iter().enumerate() {
            let rendered = stream.push(chunk);
            frames += 1;
            let parses = stream.markdown.parsed_lines() - last_parses;
            let highlights = stream.markdown.highlighted_lines() - last_highlights;
            last_parses = stream.markdown.parsed_lines();
            last_highlights = stream.markdown.highlighted_lines();
            // A chunk of `SIZE` characters completes at most `SIZE` lines;
            // besides those only the unfinished line is highlighted, and
            // parsed with the fence line that resumes the block.
            assert!(
                parses <= SIZE + FRAME_OVERHEAD,
                "{parses} lines parsed for one append inside the fence"
            );
            assert!(
                highlights <= SIZE + 1,
                "{highlights} lines highlighted for one append inside the fence"
            );
            // A one-pass render costs the whole text: compare a sample.
            if index % 97 == 0 || index + 1 == pieces.len() {
                assert_eq!(rendered, stream.one_shot(false));
            }
        }
        let parsed = stream.markdown.parsed_lines();
        let highlighted = stream.markdown.highlighted_lines();
        assert!(
            parsed <= lines_in_reply + frames * FRAME_OVERHEAD,
            "{parsed} parses for {lines_in_reply} lines in {frames} frames"
        );
        assert!(
            (CODE_LINES..=CODE_LINES + frames).contains(&highlighted),
            "{highlighted} highlights for {CODE_LINES} code lines in {frames} frames"
        );
    }
}
