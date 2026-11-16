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

/// Inserts `c` at the grapheme boundary `at` and returns the cursor after it.
///
/// A combining mark, ZWJ or second regional indicator joins the cluster
/// before it, so the cursor sits after the cluster the new char ended up in.
pub fn insert_char(text: &mut String, at: usize, c: char) -> usize {
    text.insert(at, c);
    next_boundary(text, prev_boundary(text, at + c.len_utf8()))
}

/// Inserts `s` at the grapheme boundary `at` and returns the cursor at the
/// boundary at or after its end: the text may join the cluster before or
/// after it.
pub fn insert_str(text: &mut String, at: usize, s: &str) -> usize {
    text.insert_str(at, s);
    next_boundary(text, prev_boundary(text, at + s.len()))
}

/// Deletes the grapheme before the boundary `at` and returns the cursor.
pub fn remove_before(text: &mut String, at: usize) -> usize {
    let start = prev_boundary(text, at);
    text.replace_range(start..at, "");
    start
}

/// Deletes the grapheme after the boundary `at`.
pub fn remove_after(text: &mut String, at: usize) {
    let end = next_boundary(text, at);
    text.replace_range(at..end, "");
}

/// The longest prefix of `text`, cut between grapheme clusters, that the
/// renderer draws in at most `max_cells` cells.
#[must_use]
pub fn prefix_within_width(text: &str, max_cells: usize) -> &str {
    let mut used = 0;
    for (at, cluster) in text.grapheme_indices(true) {
        used += drawn_width(cluster);
        if used > max_cells {
            return &text[..at];
        }
    }
    text
}

/// Terminal cells the renderer draws for `text` before the boundary `at`.
#[must_use]
pub fn width_before(text: &str, at: usize) -> usize {
    let end = if text.is_char_boundary(at) {
        at
    } else {
        off_boundary(text, at)
    };
    drawn_width(&text[..end])
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

    /// Chinese, Japanese and Korean text and a thumbs-up with a skin tone
    /// (two scalar values, one cluster).
    const TEXTS: [&str; 4] = ["你好世界", "こんにちは", "안녕하세요", "👍🏽"];

    #[test]
    fn typing_builds_the_text_with_the_cursor_at_its_end() {
        for text in TEXTS {
            let mut typed = String::new();
            let mut cursor = 0;
            for c in text.chars() {
                cursor = insert_char(&mut typed, cursor, c);
            }
            assert_eq!((typed.as_str(), cursor), (text, text.len()), "{text}");
        }
    }

    #[test]
    fn a_modifier_typed_after_its_base_joins_the_cluster() {
        let mut typed = String::new();
        let cursor = insert_char(&mut typed, 0, '\u{1F44D}');
        assert_eq!(cursor, 4);
        let cursor = insert_char(&mut typed, cursor, '\u{1F3FD}');
        assert_eq!((typed.as_str(), cursor), ("👍🏽", 8));
        assert_eq!(remove_before(&mut typed, cursor), 0);
        assert_eq!(typed, "");
    }

    #[test]
    fn removal_takes_one_cluster_on_either_side_of_the_cursor() {
        for text in TEXTS {
            let first = next_boundary(text, 0);
            let mut deleted = text.to_string();
            remove_after(&mut deleted, 0);
            assert_eq!(deleted, text[first..], "{text}");

            let last = prev_boundary(text, text.len());
            let mut backspaced = text.to_string();
            let cursor = remove_before(&mut backspaced, text.len());
            assert_eq!(
                (backspaced.as_str(), cursor),
                (&text[..last], last),
                "{text}"
            );
        }
    }

    #[test]
    fn insert_str_leaves_the_cursor_on_a_boundary() {
        let mut text = "👍".to_string();
        let cursor = insert_str(&mut text, 4, "\u{1F3FD}あ");
        assert_eq!((text.as_str(), cursor), ("👍🏽あ", text.len()));
    }

    #[test]
    fn width_before_is_the_cells_drawn_before_the_cursor() {
        for text in TEXTS {
            assert_eq!(width_before(text, 0), 0, "{text}");
            assert_eq!(width_before(text, text.len()), drawn_width(text), "{text}");
        }
        assert_eq!(width_before("你好世界", 6), 4);
        assert_eq!(width_before("a👍🏽b", 1 + 8), 3);
    }

    #[test]
    fn prefix_within_width_cuts_between_clusters() {
        assert_eq!(prefix_within_width("你好世界", 5), "你好");
        assert_eq!(prefix_within_width("こんにちは", 4), "こん");
        assert_eq!(prefix_within_width("안녕하세요", 1), "");
        assert_eq!(prefix_within_width("a👍🏽b", 2), "a");
        assert_eq!(prefix_within_width("a👍🏽b", 3), "a👍🏽");
        assert_eq!(prefix_within_width("日本語abc", 100), "日本語abc");
        assert_eq!(prefix_within_width("", 3), "");
    }
}
