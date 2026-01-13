//! Text selection for the chat view.
//!
//! This module provides text selection functionality including:
//! - Selection state tracking (anchor and cursor positions)
//! - Coordinate mapping from screen to content positions
//! - Text extraction from selection range

use ratatui::prelude::*;
use unicode_width::UnicodeWidthStr;

/// Position in rendered content (visual coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Visual line index (0-indexed, after wrapping)
    pub line: usize,
    /// Character offset within the line (0-indexed)
    pub col: usize,
}

impl PartialOrd for Position {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Position {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match self.line.cmp(&other.line) {
            std::cmp::Ordering::Equal => self.col.cmp(&other.col),
            other => other,
        }
    }
}

/// Text selection state.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    /// Anchor point (where drag started)
    anchor: Option<Position>,
    /// Current cursor (where drag is now)
    cursor: Option<Position>,
    /// Whether mouse button is currently held
    dragging: bool,
}

impl Selection {
    /// Create a new empty selection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a new selection at the given position.
    pub fn start(&mut self, pos: Position) {
        self.anchor = Some(pos);
        self.cursor = Some(pos);
        self.dragging = true;
    }

    /// Extend selection to the given position.
    pub fn extend(&mut self, pos: Position) {
        if self.dragging {
            self.cursor = Some(pos);
        }
    }

    /// Finish the selection (mouse released).
    pub fn finish(&mut self) {
        self.dragging = false;
    }

    /// Clear the selection.
    pub fn clear(&mut self) {
        self.anchor = None;
        self.cursor = None;
        self.dragging = false;
    }

    /// Get normalized range (start always before end).
    /// Returns None if no selection.
    pub fn range(&self) -> Option<(Position, Position)> {
        let anchor = self.anchor?;
        let cursor = self.cursor?;
        if anchor <= cursor {
            Some((anchor, cursor))
        } else {
            Some((cursor, anchor))
        }
    }

    /// Check if selection is active (has content).
    pub fn is_active(&self) -> bool {
        if let Some((start, end)) = self.range() {
            start != end
        } else {
            false
        }
    }

    /// Check if currently dragging.
    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    /// Check if a position is within the selection.
    pub fn contains(&self, line: usize, col: usize) -> bool {
        if let Some((start, end)) = self.range() {
            let pos = Position { line, col };
            pos >= start && pos <= end
        } else {
            false
        }
    }
}

/// Info about a visual line for coordinate mapping.
#[derive(Debug, Clone)]
struct VisualLineInfo {
    /// The text content of this visual line
    text: String,
}

/// Maps screen coordinates to content positions.
///
/// This is rebuilt each render frame to account for scrolling and wrapping.
#[derive(Debug, Default)]
pub struct CoordinateMapper {
    /// Info for each visual line
    lines: Vec<VisualLineInfo>,
    /// Current scroll offset
    scroll: usize,
    /// Render width in columns
    width: usize,
}

impl CoordinateMapper {
    /// Create a new empty coordinate mapper.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild the coordinate mapper from rendered lines.
    ///
    /// This should be called after rendering to update the mapping.
    pub fn rebuild(&mut self, lines: &[Line<'_>], scroll: usize, width: usize) {
        self.lines.clear();
        self.scroll = scroll;
        self.width = width;

        for line in lines {
            // Extract text from spans
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

            // For now, store each line as-is
            // Wrapping is handled by ratatui's Paragraph, so we need to simulate it
            self.add_line_with_wrapping(&text, width);
        }
    }

    /// Add a line, splitting into multiple visual lines if it wraps.
    fn add_line_with_wrapping(&mut self, text: &str, width: usize) {
        if width == 0 {
            self.lines.push(VisualLineInfo {
                text: text.to_string(),
            });
            return;
        }

        let mut current_width = 0;
        let mut current_text = String::new();

        for c in text.chars() {
            let char_width = UnicodeWidthStr::width(c.to_string().as_str());

            if current_width + char_width > width && !current_text.is_empty() {
                // Wrap to next line
                self.lines.push(VisualLineInfo {
                    text: std::mem::take(&mut current_text),
                });
                current_width = 0;
            }

            current_text.push(c);
            current_width += char_width;
        }

        // Push remaining text
        self.lines.push(VisualLineInfo {
            text: current_text,
        });
    }

    /// Convert screen coordinates to content position.
    ///
    /// `x` and `y` are terminal coordinates relative to the chat area.
    pub fn screen_to_position(&self, x: u16, y: u16) -> Option<Position> {
        let visual_line = self.scroll.saturating_add(y as usize);
        let info = self.lines.get(visual_line)?;

        // Convert x coordinate to character offset using unicode width
        let col = self.x_to_char_offset(&info.text, x as usize);

        Some(Position {
            line: visual_line,
            col,
        })
    }

    /// Convert x position to character offset in text.
    fn x_to_char_offset(&self, text: &str, x: usize) -> usize {
        let mut current_width = 0;

        for (i, c) in text.chars().enumerate() {
            let char_width = UnicodeWidthStr::width(c.to_string().as_str());
            if current_width >= x {
                return i;
            }
            current_width += char_width;
        }

        // Beyond end of line
        text.chars().count()
    }

    /// Extract text from a selection range.
    pub fn extract(&self, start: Position, end: Position) -> String {
        if start.line >= self.lines.len() {
            return String::new();
        }

        let mut result = String::new();

        for line_idx in start.line..=end.line.min(self.lines.len().saturating_sub(1)) {
            let Some(info) = self.lines.get(line_idx) else {
                continue;
            };

            let line_start = if line_idx == start.line { start.col } else { 0 };
            let line_end = if line_idx == end.line {
                end.col
            } else {
                info.text.chars().count()
            };

            // Extract substring by character indices
            let extracted: String = info
                .text
                .chars()
                .skip(line_start)
                .take(line_end.saturating_sub(line_start))
                .collect();

            if !result.is_empty() && !extracted.is_empty() {
                result.push('\n');
            }
            result.push_str(&extracted);
        }

        result
    }

    /// Get the number of visual lines.
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
}

/// Apply selection highlight to rendered lines.
///
/// This modifies the lines in place to add selection styling.
/// Converts lines to 'static lifetime by cloning content.
pub fn apply_selection_highlight(
    lines: &mut Vec<Line<'_>>,
    start: Position,
    end: Position,
) {
    let highlight_style = Style::default()
        .bg(Color::Rgb(70, 130, 180)) // Steel blue
        .fg(Color::White);

    for (line_idx, line) in lines.iter_mut().enumerate() {
        if line_idx < start.line || line_idx > end.line {
            continue;
        }

        let line_start = if line_idx == start.line { start.col } else { 0 };
        let line_end = if line_idx == end.line {
            end.col
        } else {
            usize::MAX // To end of line
        };

        *line = highlight_line_range(std::mem::take(line), line_start, line_end, highlight_style);
    }
}

/// Highlight a range within a line by splitting spans as needed.
/// Converts all content to owned strings to allow splitting.
fn highlight_line_range(line: Line<'_>, start_col: usize, end_col: usize, style: Style) -> Line<'static> {
    let mut new_spans: Vec<Span<'static>> = Vec::new();
    let mut current_col = 0;

    for span in line.spans {
        let span_len = span.content.chars().count();
        let span_end = current_col + span_len;

        if span_end <= start_col || current_col >= end_col {
            // Span is entirely outside selection - convert to owned
            new_spans.push(Span::styled(span.content.to_string(), span.style));
        } else if current_col >= start_col && span_end <= end_col {
            // Span is entirely inside selection
            new_spans.push(Span::styled(span.content.to_string(), span.style.patch(style)));
        } else {
            // Span partially overlaps selection - need to split
            let chars: Vec<char> = span.content.chars().collect();

            // Before selection
            let before_start = 0;
            let before_end = start_col.saturating_sub(current_col).min(chars.len());
            if before_end > before_start {
                let before: String = chars[before_start..before_end].iter().collect();
                new_spans.push(Span::styled(before, span.style));
            }

            // Inside selection
            let inside_start = start_col.saturating_sub(current_col).min(chars.len());
            let inside_end = end_col.saturating_sub(current_col).min(chars.len());
            if inside_end > inside_start {
                let inside: String = chars[inside_start..inside_end].iter().collect();
                new_spans.push(Span::styled(inside, span.style.patch(style)));
            }

            // After selection
            let after_start = end_col.saturating_sub(current_col).min(chars.len());
            let after_end = chars.len();
            if after_end > after_start {
                let after: String = chars[after_start..after_end].iter().collect();
                new_spans.push(Span::styled(after, span.style));
            }
        }

        current_col = span_end;
    }

    Line::from(new_spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_ordering() {
        let a = Position { line: 0, col: 5 };
        let b = Position { line: 0, col: 10 };
        let c = Position { line: 1, col: 0 };

        assert!(a < b);
        assert!(b < c);
        assert!(a < c);
    }

    #[test]
    fn test_selection_range() {
        let mut sel = Selection::new();

        // Forward selection
        sel.start(Position { line: 0, col: 5 });
        sel.extend(Position { line: 1, col: 10 });

        let (start, end) = sel.range().unwrap();
        assert_eq!(start, Position { line: 0, col: 5 });
        assert_eq!(end, Position { line: 1, col: 10 });

        // Backward selection
        sel.start(Position { line: 1, col: 10 });
        sel.extend(Position { line: 0, col: 5 });

        let (start, end) = sel.range().unwrap();
        assert_eq!(start, Position { line: 0, col: 5 });
        assert_eq!(end, Position { line: 1, col: 10 });
    }

    #[test]
    fn test_extract_text() {
        let mut mapper = CoordinateMapper::new();
        let lines = vec![
            Line::from("Hello World"),
            Line::from("Second line"),
            Line::from("Third line"),
        ];
        mapper.rebuild(&lines, 0, 80);

        // Extract from middle of first line to middle of second
        let text = mapper.extract(
            Position { line: 0, col: 6 },
            Position { line: 1, col: 6 },
        );
        assert_eq!(text, "World\nSecond");
    }

    #[test]
    fn test_highlight_line_range() {
        let line = Line::from(vec![
            Span::raw("Hello "),
            Span::raw("World"),
        ]);

        let highlighted = highlight_line_range(
            line,
            3,
            8,
            Style::default().bg(Color::Blue),
        );

        assert_eq!(highlighted.spans.len(), 4); // "Hel", "lo ", "Wo", "rld"
    }
}
