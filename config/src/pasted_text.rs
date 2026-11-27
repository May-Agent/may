//! Model-facing block for text a user pasted into a prompt.

use std::path::Path;

use askama::Template;

/// Maximum character count for pasted text that inserts directly inline into
/// the composer rather than attaching as pasted text.
pub const INLINE_PASTE_CHAR_LIMIT: usize = 500;

/// Maximum character count for pasted text that can be inlined into the
/// composer by pasting the same text a second time.
pub const REPASTE_INLINE_CHAR_LIMIT: usize = 10_000;

/// A paste longer than this many characters is cut for the model and
/// expected to live in a saved file.
pub const LARGE_PASTE_THRESHOLD: usize = 4000;

/// Characters of a large paste shown in the model's context.
pub const PASTE_TRUNCATE_LIMIT: usize = 2000;

/// Normalize CRLF (`\r\n`) and bare CR (`\r`) line endings to LF (`\n`).
#[must_use]
pub fn normalize_line_endings(text: String) -> String {
    if !text.contains('\r') {
        return text;
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// Whether `text` has at most `limit` Unicode scalar characters. Walks at
/// most `limit + 1` characters regardless of input length.
#[must_use]
pub fn within_char_limit(text: &str, limit: usize) -> bool {
    text.char_indices().nth(limit).is_none()
}

/// Whether `text` fits inline in the composer without becoming an attachment
/// (at most [`INLINE_PASTE_CHAR_LIMIT`] characters).
#[must_use]
pub fn fits_inline(text: &str) -> bool {
    within_char_limit(text, INLINE_PASTE_CHAR_LIMIT)
}

/// Whether `text` is small enough to inline into the composer on a second
/// paste of the same content (at most [`REPASTE_INLINE_CHAR_LIMIT`]
/// characters).
#[must_use]
pub fn can_repaste_inline(text: &str) -> bool {
    within_char_limit(text, REPASTE_INLINE_CHAR_LIMIT)
}

/// Count characters in `text` up to `limit`, returning `Some(count)` when
/// `text` has at most `limit` characters and `None` when it exceeds `limit`.
/// Walks at most `limit + 1` characters regardless of input length.
#[must_use]
pub fn bounded_char_count(text: &str, limit: usize) -> Option<usize> {
    let mut count = 0;
    for _ in text.char_indices() {
        if count == limit {
            return None;
        }
        count += 1;
    }
    Some(count)
}

/// Whether `text` is longer than [`LARGE_PASTE_THRESHOLD`] characters. It
/// walks at most that many, however long the paste is.
#[must_use]
pub fn is_large(text: &str) -> bool {
    !within_char_limit(text, LARGE_PASTE_THRESHOLD)
}

#[derive(Template)]
#[template(path = "pasted_text.txt", escape = "none")]
struct PastedText<'a> {
    content: &'a str,
    /// The pasted text's length in characters when it is shown whole.
    chars: Option<usize>,
    /// [`PASTE_TRUNCATE_LIMIT`], named when the text is cut.
    limit: usize,
    full_path: Option<String>,
}

/// Build the pasted-text block a user message contributes to the prompt.
///
/// Content longer than [`LARGE_PASTE_THRESHOLD`] characters is cut to its
/// first [`PASTE_TRUNCATE_LIMIT`] and `saved_path` is named as holding the
/// full text.
///
/// # Panics
///
/// Panics if the compiled template fails to render.
#[must_use]
pub fn pasted_text_block(content: &str, saved_path: Option<&Path>) -> String {
    let (content, chars) = if is_large(content) {
        let cut = content
            .char_indices()
            .nth(PASTE_TRUNCATE_LIMIT)
            .map_or(content.len(), |(index, _)| index);
        (&content[..cut], None)
    } else {
        // At most LARGE_PASTE_THRESHOLD characters, so the count is bounded.
        (content, Some(content.char_indices().count()))
    };
    PastedText {
        content,
        chars,
        limit: PASTE_TRUNCATE_LIMIT,
        full_path: saved_path.map(crate::fs_utils::display_path),
    }
    .render()
    .expect("pasted text template must render")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_paste_is_wrapped_with_its_length() {
        assert_eq!(
            pasted_text_block("hello", None),
            "<pasted_text>\nhello\n</pasted_text>\nThe pasted text above is 5 characters."
        );
    }

    #[test]
    fn cjk_paste_is_cut_to_its_first_characters() {
        for unit in ["你好世界", "こんにちは", "안녕하세요", "👍🏽"] {
            let text = unit.repeat(LARGE_PASTE_THRESHOLD);
            assert!(is_large(&text));
            let block = pasted_text_block(&text, None);
            let shown: String = text.chars().take(PASTE_TRUNCATE_LIMIT).collect();
            assert!(block.starts_with(&format!("<pasted_text>\n{shown}\n</pasted_text>\n")));
            assert!(block.contains(&format!("first {PASTE_TRUNCATE_LIMIT} characters")));
        }
    }

    #[test]
    fn a_cjk_paste_within_the_threshold_is_shown_whole() {
        let text = "你好".repeat(LARGE_PASTE_THRESHOLD / 2);
        assert!(!is_large(&text));
        assert!(
            pasted_text_block(&text, None)
                .ends_with(&format!("is {LARGE_PASTE_THRESHOLD} characters."))
        );
    }

    #[test]
    fn long_paste_is_cut_and_names_the_saved_file() {
        let text = "x".repeat(LARGE_PASTE_THRESHOLD + 1);
        let block = pasted_text_block(&text, Some(Path::new("/s/files/paste.txt")));
        assert_eq!(
            block,
            format!(
                "<pasted_text>\n{}\n</pasted_text>\nThe pasted text above is cut to its first {PASTE_TRUNCATE_LIMIT} characters. The full text is saved at /s/files/paste.txt.",
                "x".repeat(PASTE_TRUNCATE_LIMIT)
            )
        );
    }

    #[test]
    fn normalize_line_endings_converts_crlf_and_cr_to_lf() {
        assert_eq!(
            normalize_line_endings("plain\nline".to_string()),
            "plain\nline"
        );
        assert_eq!(
            normalize_line_endings("你好\r\nこんにちは\r안녕하세요\r\n".to_string()),
            "你好\nこんにちは\n안녕하세요\n"
        );
    }

    #[test]
    fn inline_and_repaste_limits_count_unicode_chars_not_bytes() {
        // 500 CJK chars (1500 UTF-8 bytes) fits inline; 501 does not.
        let unit = "你好こんにちは안녕"; // 9 chars
        let exactly_inline: String = unit.chars().cycle().take(INLINE_PASTE_CHAR_LIMIT).collect();
        let over_inline: String = unit
            .chars()
            .cycle()
            .take(INLINE_PASTE_CHAR_LIMIT + 1)
            .collect();
        assert!(fits_inline(&exactly_inline));
        assert!(!fits_inline(&over_inline));
        assert_eq!(
            bounded_char_count(&exactly_inline, INLINE_PASTE_CHAR_LIMIT),
            Some(INLINE_PASTE_CHAR_LIMIT)
        );
        assert_eq!(
            bounded_char_count(&over_inline, INLINE_PASTE_CHAR_LIMIT),
            None
        );

        let exactly_repaste: String = unit
            .chars()
            .cycle()
            .take(REPASTE_INLINE_CHAR_LIMIT)
            .collect();
        let over_repaste: String = unit
            .chars()
            .cycle()
            .take(REPASTE_INLINE_CHAR_LIMIT + 1)
            .collect();
        assert!(can_repaste_inline(&exactly_repaste));
        assert!(!can_repaste_inline(&over_repaste));
        assert_eq!(
            bounded_char_count(&exactly_repaste, REPASTE_INLINE_CHAR_LIMIT),
            Some(REPASTE_INLINE_CHAR_LIMIT)
        );
        assert_eq!(
            bounded_char_count(&over_repaste, REPASTE_INLINE_CHAR_LIMIT),
            None
        );
    }
}
