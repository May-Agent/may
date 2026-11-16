//! Model-facing block for text a user pasted into a prompt.

use std::path::Path;

use askama::Template;

/// A paste longer than this many characters is cut for the model and
/// expected to live in a saved file.
pub const LARGE_PASTE_THRESHOLD: usize = 4000;

/// Characters of a large paste shown in the model's context.
pub const PASTE_TRUNCATE_LIMIT: usize = 2000;

/// Whether `text` is longer than [`LARGE_PASTE_THRESHOLD`] characters. It
/// walks at most that many, however long the paste is.
#[must_use]
pub fn is_large(text: &str) -> bool {
    text.char_indices().nth(LARGE_PASTE_THRESHOLD).is_some()
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
}
