//! The highlight that sweeps across the title of a working session.

use ratatui::style::Style;
use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation as _;

/// Characters the highlight covers.
const SWEEP_WIDTH: usize = 3;
/// Animation frames the highlight rests on one position.
const FRAMES_PER_STEP: usize = 2;

/// `(start, end)` grapheme indices of the highlight window at `frame` over `len`
/// graphemes.
#[must_use]
pub fn sweep_window(len: usize, frame: usize) -> (usize, usize) {
    if len == 0 {
        return (0, 0);
    }
    let head = (frame / FRAMES_PER_STEP) % (len + SWEEP_WIDTH);
    let start = head.saturating_sub(SWEEP_WIDTH).min(len);
    let end = head.min(len);
    (start, end)
}

/// `text` as spans, with a highlight moving from left to right and repeating.
///
/// It enters at the left edge, leaves at the right one, then starts over.
/// `frame` is the TUI's animation counter; the spans are computed from it
/// alone, so painting needs no state of its own.
#[must_use]
pub fn sweep_spans(text: &str, frame: usize, base: Style, highlight: Style) -> Vec<Span<'static>> {
    // Byte offset of each grapheme: the highlight steps over whole clusters.
    let offsets: Vec<usize> = text.grapheme_indices(true).map(|(at, _)| at).collect();
    let len = offsets.len();
    if len == 0 {
        return Vec::new();
    }
    let (start, end) = sweep_window(len, frame);
    let byte = |index: usize| offsets.get(index).copied().unwrap_or(text.len());
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

/// Apply the sweeping `highlight` over `spans` whose text forms a subslice of
/// a `total_graphemes`-long stage starting at `grapheme_offset`, keeping each
/// span's existing style outside the highlight window.
#[must_use]
pub fn sweep_styled_slice(
    spans: &[Span<'static>],
    grapheme_offset: usize,
    total_graphemes: usize,
    frame: usize,
    highlight: Style,
) -> (Vec<Span<'static>>, usize) {
    let (win_start, win_end) = sweep_window(total_graphemes, frame);
    let mut cursor = grapheme_offset;
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        let text = span.content.as_ref();
        if text.is_empty() {
            continue;
        }
        let offsets: Vec<usize> = text.grapheme_indices(true).map(|(at, _)| at).collect();
        let span_len = offsets.len();
        let span_start = cursor;
        let span_end = cursor + span_len;
        cursor = span_end;

        let lit_start = win_start.max(span_start);
        let lit_end = win_end.min(span_end);
        if lit_start >= lit_end {
            out.push(span.clone());
            continue;
        }
        let byte = |local: usize| offsets.get(local).copied().unwrap_or(text.len());
        let b_start = byte(lit_start - span_start);
        let b_end = byte(lit_end - span_start);
        for (part, style) in [
            (&text[..b_start], span.style),
            (&text[b_start..b_end], highlight),
            (&text[b_end..], span.style),
        ] {
            if !part.is_empty() {
                out.push(Span::styled(part.to_string(), style));
            }
        }
    }
    (out, cursor)
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

    #[test]
    fn the_highlight_steps_over_whole_grapheme_clusters() {
        // The skin tone modifier and the combining accent stay with their base.
        assert_eq!(lit("a👍🏽e\u{301}b", 3 * FRAMES_PER_STEP), "a👍🏽e\u{301}");
        assert_eq!(lit("こんにちは", 2 * FRAMES_PER_STEP), "こん");
        assert_eq!(lit("안녕하세요", 4 * FRAMES_PER_STEP), "녕하세");
    }

    #[test]
    fn sweep_styled_slice_preserves_syntax_style_outside_window() {
        let base_a = Style::default().fg(Color::Cyan);
        let base_b = Style::default().fg(Color::Green);
        let highlight = Style::default().fg(Color::White);
        let spans = vec![Span::styled("cargo ", base_a), Span::styled("检查", base_b)];
        // "cargo 检查" has 8 graphemes; at frame = 7 * FRAMES_PER_STEP,
        // head = 7 -> window is [4..7) -> "o 检".
        let (out, end_offset) = sweep_styled_slice(&spans, 0, 8, 7 * FRAMES_PER_STEP, highlight);
        assert_eq!(end_offset, 8);
        let lit: String = out
            .iter()
            .filter(|s| s.style == highlight)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(lit, "o 检");
        assert_eq!(out[0].style, base_a);
        assert_eq!(out[0].content.as_ref(), "carg");
        assert_eq!(out.last().unwrap().style, base_b);
        assert_eq!(out.last().unwrap().content.as_ref(), "查");
    }
}
