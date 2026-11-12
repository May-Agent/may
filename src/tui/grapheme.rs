//! Grapheme-cluster cursor arithmetic for the composer.
//!
//! The composer's cursor is a byte offset into its text that always sits on
//! an extended grapheme cluster boundary, so one keystroke moves over or
//! deletes what the user sees as one character (a ZWJ emoji, a flag, a base
//! letter with its combining marks) and costs time proportional to that
//! cluster, not to the text.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Byte offset of the grapheme boundary before `at`; `0` when `at` is `0`.
///
/// `at` must be a grapheme boundary of `text`; an offset that is not even a
/// char boundary panics in a debug build.
#[must_use]
pub fn prev_boundary(text: &str, at: usize) -> usize {
    let Some(before) = text.get(..at) else {
        return off_boundary(text, at);
    };
    before
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(start, _)| start)
}

/// Byte offset of the grapheme boundary after `at`; `text.len()` when `at`
/// is the end.
///
/// `at` must be a grapheme boundary of `text`; see [`prev_boundary`].
#[must_use]
pub fn next_boundary(text: &str, at: usize) -> usize {
    let Some(after) = text.get(at..) else {
        return off_boundary(text, at);
    };
    after
        .graphemes(true)
        .next()
        .map_or(at, |cluster| at + cluster.len())
}

/// A cursor off a char boundary is a contract violation of the composer;
/// a release build recovers to the char boundary at or before it.
fn off_boundary(text: &str, at: usize) -> usize {
    debug_assert!(
        false,
        "composer cursor {at} is not a char boundary of a {}-byte input",
        text.len()
    );
    tracing::error!(
        at,
        len = text.len(),
        "composer cursor is not a char boundary"
    );
    text.floor_char_boundary(at)
}

/// Terminal cells the renderer draws for one grapheme cluster.
#[must_use]
pub fn drawn_width(cluster: &str) -> usize {
    UnicodeWidthStr::width(cluster)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ZWJ family emoji, a flag, a base letter plus combining accent, and
    /// Chinese, Japanese and Korean text.
    const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    const FLAG: &str = "\u{1F1E8}\u{1F1E6}";
    const ACCENT: &str = "e\u{0301}";

    #[test]
    fn zwj_flag_and_combining_sequences_are_one_step() {
        for cluster in [FAMILY, FLAG, ACCENT] {
            let text = format!("a{cluster}b");
            let after = 1 + cluster.len();
            assert_eq!(next_boundary(&text, 1), after, "{cluster:?}");
            assert_eq!(prev_boundary(&text, after), 1, "{cluster:?}");
        }
    }

    #[test]
    fn two_flags_split_into_two_clusters() {
        let text = format!("{FLAG}{FLAG}");
        assert_eq!(next_boundary(&text, 0), FLAG.len());
        assert_eq!(prev_boundary(&text, text.len()), FLAG.len());
    }

    #[test]
    fn cjk_steps_one_character_at_a_time() {
        // Each of these characters is three bytes of UTF-8.
        for text in ["中文", "日本語", "한국어"] {
            assert_eq!(next_boundary(text, 0), 3, "{text}");
            assert_eq!(prev_boundary(text, text.len()), text.len() - 3, "{text}");
        }
    }

    #[test]
    fn ends_are_fixed_points() {
        assert_eq!(prev_boundary("ab", 0), 0);
        assert_eq!(next_boundary("ab", 2), 2);
    }

    #[test]
    fn drawn_width_is_what_the_renderer_draws() {
        assert_eq!(drawn_width(FAMILY), 2);
        assert_eq!(drawn_width(FLAG), 2);
        assert_eq!(drawn_width(ACCENT), 1);
        assert_eq!(drawn_width("中"), 2);
        assert_eq!(drawn_width("a"), 1);
    }
}
