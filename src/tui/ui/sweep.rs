//! The highlight that sweeps across the title of a working session.

use ratatui::style::Style;
use ratatui::text::Span;

/// Characters the highlight covers.
const SWEEP_WIDTH: usize = 3;
/// Animation frames the highlight rests on one position.
const FRAMES_PER_STEP: usize = 2;

/// `text` as spans, with a highlight moving from left to right and repeating.
///
/// It enters at the left edge, leaves at the right one, then starts over.
/// `frame` is the TUI's animation counter; the spans are computed from it
/// alone, so painting needs no state of its own.
#[must_use]
pub fn sweep_spans(text: &str, frame: usize, base: Style, highlight: Style) -> Vec<Span<'static>> {
    let len = text.chars().count();
    if len == 0 {
        return Vec::new();
    }
    let head = (frame / FRAMES_PER_STEP) % (len + SWEEP_WIDTH);
    let start = head.saturating_sub(SWEEP_WIDTH).min(len);
    let end = head.min(len);
    let byte = |chars: usize| {
        text.char_indices()
            .nth(chars)
            .map_or(text.len(), |(offset, _)| offset)
    };
    let (start, end) = (byte(start), byte(end));
    [
        (&text[..start], base),
        (&text[start..end], highlight),
        (&text[end..], base),
    ]
    .into_iter()
    .filter(|(part, _)| !part.is_empty())
    .map(|(part, style)| Span::styled(part.to_string(), style))
    .collect()
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;

    fn lit(text: &str, frame: usize) -> String {
        let highlight = Style::default().fg(Color::White);
        sweep_spans(text, frame, Style::default(), highlight)
            .into_iter()
            .filter(|span| span.style == highlight)
            .map(|span| span.content.into_owned())
            .collect()
    }

    #[test]
    fn the_highlight_moves_from_left_to_right_and_repeats() {
        assert_eq!(lit("abcdef", 0), "");
        assert_eq!(lit("abcdef", FRAMES_PER_STEP), "a");
        assert_eq!(lit("abcdef", 3 * FRAMES_PER_STEP), "abc");
        assert_eq!(lit("abcdef", 4 * FRAMES_PER_STEP), "bcd");
        assert_eq!(lit("abcdef", 8 * FRAMES_PER_STEP), "f");
        // One full pass later it starts over at the left edge.
        assert_eq!(lit("abcdef", 9 * FRAMES_PER_STEP), "");
        assert_eq!(lit("abcdef", 10 * FRAMES_PER_STEP), "a");
    }

    #[test]
    fn multibyte_titles_split_on_character_boundaries() {
        assert_eq!(lit("会话标题です한국어", 4 * FRAMES_PER_STEP), "话标题");
        let spans = sweep_spans("会话", 0, Style::default(), Style::default());
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn an_empty_title_has_no_spans() {
        assert_eq!(sweep_spans("", 7, Style::default(), Style::default()), []);
    }
}
