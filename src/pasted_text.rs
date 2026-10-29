//! Model-facing block for text a user pasted into a prompt.

use std::path::Path;

use askama::Template;

/// Threshold (chars) above which a pasted-text attachment is truncated for
/// the model and expected to live in a saved file.
pub const LARGE_PASTE_THRESHOLD: usize = 4000;

/// Maximum chars shown in LLM context for large pastes.
pub const PASTE_TRUNCATE_LIMIT: usize = 2000;

#[derive(Template)]
#[template(path = "pasted_text.txt", escape = "none")]
struct PastedText {
    content: String,
    chars: usize,
    shown_chars: usize,
    truncated: bool,
    full_path: Option<String>,
}

/// Build the pasted-text block a user message contributes to the prompt.
///
/// Content longer than [`LARGE_PASTE_THRESHOLD`] chars is cut to
/// [`PASTE_TRUNCATE_LIMIT`] and `saved_path` is named as holding the full text.
///
/// # Panics
///
/// Panics if the compiled template fails to render.
#[must_use]
pub fn pasted_text_block(content: &str, saved_path: Option<&Path>) -> String {
    let chars = content.chars().count();
    let truncated = chars > LARGE_PASTE_THRESHOLD;
    let (content, shown_chars) = if truncated {
        (
            content.chars().take(PASTE_TRUNCATE_LIMIT).collect(),
            PASTE_TRUNCATE_LIMIT,
        )
    } else {
        (content.to_string(), chars)
    };
    PastedText {
        content,
        chars,
        shown_chars,
        truncated,
        full_path: saved_path.map(|path| path.display().to_string()),
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
    fn long_paste_is_cut_and_names_the_saved_file() {
        let text = "x".repeat(LARGE_PASTE_THRESHOLD + 1);
        let block = pasted_text_block(&text, Some(Path::new("/s/files/paste.txt")));
        assert_eq!(
            block,
            format!(
                "<pasted_text>\n{}\n</pasted_text>\nThe pasted text above is cut to its first 2000 of 4001 characters. The full text is saved at /s/files/paste.txt.",
                "x".repeat(PASTE_TRUNCATE_LIMIT)
            )
        );
    }
}
