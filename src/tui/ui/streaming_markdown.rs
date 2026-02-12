//! Streaming-optimized markdown renderer for LLM output.
//!
//! Uses streamdown-parser for true incremental parsing - only processes
//! new lines as they arrive, maintaining parser state between chunks.

use ratatui::prelude::*;
use streamdown_parser::{ParseEvent, Parser};

/// Height reserved for inline images (in terminal lines).
pub const IMAGE_HEIGHT: usize = 8;

/// Streaming markdown renderer with incremental parsing.
pub struct StreamingMarkdown {
    /// The incremental parser (maintains state between chunks)
    parser: Parser,
    /// Raw content accumulated so far
    content: String,
    /// Number of complete lines already parsed
    parsed_line_count: usize,
    /// Rendered output lines
    rendered: Vec<Line<'static>>,
    /// Images found during parsing (alt, url)
    images: Vec<(String, String)>,
    /// Image positions: (url, line_index) for rendering at correct scroll position
    image_positions: Vec<(String, usize)>,
    /// Current incomplete line (not yet parsed)
    pending_line: String,
    /// Whether we're currently appending inline spans into an open visual line
    inline_open: bool,
    /// Whether terminal supports image rendering
    images_supported: bool,
}

impl Default for StreamingMarkdown {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingMarkdown {
    /// Create a new streaming markdown renderer.
    /// Uses default (no image support). For image support, use `with_image_support()`.
    pub fn new() -> Self {
        Self {
            parser: Parser::new(),
            content: String::new(),
            parsed_line_count: 0,
            rendered: Vec::new(),
            images: Vec::new(),
            image_positions: Vec::new(),
            pending_line: String::new(),
            inline_open: false,
            images_supported: false,
        }
    }

    /// Create a new streaming markdown renderer with explicit image support flag.
    pub fn with_image_support(images_supported: bool) -> Self {
        Self {
            parser: Parser::new(),
            content: String::new(),
            parsed_line_count: 0,
            rendered: Vec::new(),
            images: Vec::new(),
            image_positions: Vec::new(),
            pending_line: String::new(),
            inline_open: false,
            images_supported,
        }
    }

    /// Set whether images are supported (can be used to update after creation).
    pub fn set_images_supported(&mut self, supported: bool) {
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
        for event in self.parser.parse_line(line) {
            self.render_event(event);
        }
    }

    /// Clear all content and reset parser state.
    pub fn clear(&mut self) {
        self.parser = Parser::new();
        self.content.clear();
        self.parsed_line_count = 0;
        self.rendered.clear();
        self.images.clear();
        self.image_positions.clear();
        self.pending_line.clear();
        self.inline_open = false;
    }

    /// Get the current raw content.
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
    pub fn images(&self) -> Vec<(String, String)> {
        self.images.clone()
    }

    /// Get image positions: (url, line_index) for each image.
    /// The line_index is the position in the rendered output where the image should be displayed.
    pub fn image_positions(&self) -> &[(String, usize)] {
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

    fn end_inline_line(&mut self) {
        self.inline_open = false;
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
                    format!(" ({})", url),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ParseEvent::InlineElements(elements) => {
                // Track inline images at the current visual line.
                let line_index = if self.inline_open {
                    self.rendered.len().saturating_sub(1)
                } else {
                    self.rendered.len()
                };
                for elem in &elements {
                    if let streamdown_parser::InlineElement::Image { alt, url } = elem {
                        tracing::debug!("InlineElement::Image detected: alt={}, url={}", alt, url);
                        self.images.push((alt.clone(), url.clone()));
                        self.image_positions.push((url.clone(), line_index));
                    }
                }
                for elem in elements {
                    self.push_inline_span(render_inline_element(elem));
                }
            }
            ParseEvent::Image { alt, url } => {
                self.end_inline_line();
                tracing::debug!("ParseEvent::Image detected: alt={}, url={}", alt, url);
                self.images.push((alt.clone(), url.clone()));
                let line_index = self.rendered.len();
                self.image_positions.push((url.clone(), line_index));
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
                let prefix = if level > 1 {
                    format!("{} ", "#".repeat(level as usize))
                } else {
                    String::new()
                };
                self.rendered.push(Line::from(vec![
                    Span::styled(prefix, style.fg(Color::DarkGray)),
                    Span::styled(content, style),
                ]));
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
            ParseEvent::CodeBlockEnd => {
                self.end_inline_line();
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
                    streamdown_parser::ListBullet::Ordered(n) => format!("{}. ", n),
                };
                self.rendered.push(Line::from(vec![
                    Span::styled(
                        format!("{}{}", indent_str, bullet_str),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw(content),
                ]));
            }
            ParseEvent::ListEnd => {
                self.end_inline_line();
                self.rendered.push(Line::from(""));
            }
            ParseEvent::BlockquoteStart { .. } => {
                self.end_inline_line();
            }
            ParseEvent::BlockquoteLine(line) => {
                self.end_inline_line();
                self.rendered.push(Line::from(vec![
                    Span::styled("┃ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        line,
                        Style::default()
                            .fg(Color::Gray)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ]));
            }
            ParseEvent::BlockquoteEnd => {
                self.end_inline_line();
                self.rendered.push(Line::from(""));
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
                let spans: Vec<Span> = cells
                    .into_iter()
                    .map(|c| {
                        Span::styled(
                            format!("│ {} ", c),
                            Style::default().add_modifier(Modifier::BOLD),
                        )
                    })
                    .collect();
                self.rendered.push(Line::from(spans));
            }
            ParseEvent::TableSeparator => {
                self.end_inline_line();
                self.rendered.push(Line::from(Span::styled(
                    "├────────────────────────────────────────┤",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            ParseEvent::TableRow(cells) => {
                self.end_inline_line();
                let spans: Vec<Span> = cells
                    .into_iter()
                    .map(|c| Span::raw(format!("│ {} ", c)))
                    .collect();
                self.rendered.push(Line::from(spans));
            }
            ParseEvent::TableEnd => {
                self.end_inline_line();
                self.rendered.push(Line::from(""));
            }
            ParseEvent::EmptyLine => {
                self.end_inline_line();
                self.rendered.push(Line::from(""));
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
    /// When terminal supports images: reserves IMAGE_HEIGHT lines for the actual image.
    /// When no image support: shows as a styled link format.
    fn render_image_placeholder(&mut self, alt: &str, url: &str) {
        if self.images_supported {
            // Terminal supports images - reserve space for inline image
            // Line 0: Header with alt text only (image renders below)
            self.rendered.push(Line::from(vec![
                Span::styled("🖼 ", Style::default()),
                Span::styled(
                    alt.to_string(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            // Lines 1 to IMAGE_HEIGHT-1: Reserved space for actual image rendering
            // These will be overwritten by the image overlay in layout.rs
            for _ in 1..IMAGE_HEIGHT {
                self.rendered.push(Line::from(""));
            }
        } else {
            // No image support - render as a styled link (compact, no reserved space)
            self.rendered.push(Line::from(vec![
                Span::styled("🖼 ", Style::default()),
                Span::styled(
                    alt.to_string(),
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                Span::styled(format!(" ({})", url), Style::default().fg(Color::DarkGray)),
            ]));
            self.rendered.push(Line::from(""));
        }
    }
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
            format!("{} ({})", text, url),
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::UNDERLINED),
        ),
        streamdown_parser::InlineElement::Image { alt, url } => {
            // Inline images always render as link-style placeholder
            // (actual images are rendered as overlays, not inline)
            Span::styled(
                format!("🖼 {} ({})", alt, url),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED),
            )
        }
        streamdown_parser::InlineElement::Footnote(f) => {
            Span::styled(format!("[^{}]", f), Style::default().fg(Color::DarkGray))
        }
    }
}

/// Extract images from markdown content.
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
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_incremental_streaming() {
        let mut md = StreamingMarkdown::new();
        md.push("# Head");
        // Incomplete line - not parsed yet
        assert!(md.lines().is_empty());

        md.push("ing\n");
        // Now heading is complete
        assert!(!md.lines().is_empty());

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
        assert!(text.contains("🖼"));
        assert!(text.contains("My Image"));
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
        assert!(
            has_rule,
            "Expected horizontal rule (─), got: {:?}",
            all_text
        );
    }
}
