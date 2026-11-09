use std::path::{Path, PathBuf};

use askama::Template;
use async_fs;

use crate::db::{ChatDb, ChatDbError};

/// Error type for building prompt mention blocks.
#[derive(Debug, thiserror::Error)]
pub enum PromptMentionError {
    /// Failed to load session metadata.
    #[error("failed to load session metadata: {0}")]
    SessionLookup(#[from] ChatDbError),
    /// File mention path must be relative.
    #[error("file mention path must be relative")]
    AbsolutePath,
    /// File mention path traversal is not allowed.
    #[error("file mention path traversal is not allowed")]
    ParentTraversal,
}

struct SessionMention {
    id: String,
    title: String,
    model: String,
    recent_user_messages: Vec<String>,
}

#[derive(Template)]
#[template(path = "session_mentions.txt", escape = "none")]
struct SessionMentions {
    sessions: Vec<SessionMention>,
}

struct FileMention {
    path: String,
    name: String,
    content_type: Option<String>,
    preview: Option<String>,
}

#[derive(Template)]
#[template(path = "file_mentions.txt", escape = "none")]
struct FileMentions {
    files: Vec<FileMention>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum MentionKind {
    Generic,
    Session,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MentionCandidate {
    kind: MentionKind,
    value: String,
}

/// Build the `session_mentions` block for `@`-mentions found in `content`,
/// looking each up in the database. Returns `None` when no valid session
/// mentions are present.
///
/// # Errors
///
/// Returns [`PromptMentionError::SessionLookup`] when a session lookup fails.
///
/// # Panics
///
/// Like [`build_session_mentions`], but for daemon clients: session and
/// history rows come from REST instead of a local `history.db` handle.
///
/// # Errors
/// Returns [`PromptMentionError`] when a request fails.
///
/// Panics if the compiled template fails to render.
pub async fn build_session_mentions_remote(
    client: &crate::client::DaemonClient,
    content: &str,
) -> Result<Option<String>, PromptMentionError> {
    let mention_ids = extract_session_mention_ids(content);
    if mention_ids.is_empty() {
        return Ok(None);
    }

    let mut entries = Vec::new();
    for session_id in mention_ids {
        if let Ok(session) = client
            .get::<crate::client::WireSession>(&format!("/api/sessions/{session_id}"))
            .await
        {
            let title = if session.title.is_empty() {
                "Untitled".to_string()
            } else {
                session.title
            };
            let model = if session.model.is_empty() {
                "unknown".to_string()
            } else {
                session.model
            };
            let recent_user_messages = client
                .get::<Vec<crate::session::ChatMessage>>(&format!(
                    "/api/sessions/{session_id}/history"
                ))
                .await
                .unwrap_or_default()
                .into_iter()
                .rev()
                .filter(|message| message.role == crate::session::MessageRole::User)
                .take(4)
                .map(|message| truncate_preview_with_limit(&message.content, 160))
                .collect::<Vec<_>>();
            entries.push(SessionMention {
                id: session.id,
                title,
                model,
                recent_user_messages,
            });
        }
    }

    if entries.is_empty() {
        return Ok(None);
    }

    Ok(Some(
        SessionMentions { sessions: entries }
            .render()
            .expect("session mentions template must render"),
    ))
}

/// # Errors
/// Returns [`PromptMentionError::SessionLookup`] when a session lookup fails.
///
/// # Panics
/// Panics if the compiled template fails to render.
pub async fn build_session_mentions(
    db: &ChatDb,
    content: &str,
) -> Result<Option<String>, PromptMentionError> {
    let mention_ids = extract_session_mention_ids(content);
    if mention_ids.is_empty() {
        return Ok(None);
    }

    let mut entries = Vec::new();
    for session_id in mention_ids {
        if let Some(session) = db.get_session(&session_id).await? {
            let title = session.title.unwrap_or_else(|| "Untitled".to_string());
            let model = session.model.unwrap_or_else(|| "unknown".to_string());
            let recent_user_messages = db
                .get_messages(&session_id)
                .await?
                .into_iter()
                .rev()
                .filter(|message| message.role == "user")
                .take(4)
                .map(|message| truncate_preview_with_limit(&message.content, 160))
                .collect::<Vec<_>>();
            entries.push(SessionMention {
                id: session.id,
                title,
                model,
                recent_user_messages,
            });
        }
    }

    if entries.is_empty() {
        return Ok(None);
    }

    Ok(Some(
        SessionMentions { sessions: entries }
            .render()
            .expect("session mentions template must render"),
    ))
}

/// Build the `file_mentions` block for file `@`-mentions found in
/// `content`, resolving each against `files_dir`. Returns `None` when no valid
/// file mentions are present.
///
/// # Errors
///
/// Returns [`PromptMentionError::AbsolutePath`] or
/// [`PromptMentionError::ParentTraversal`] when a mention path is not a safe
/// relative path.
///
/// # Panics
///
/// Panics if the compiled template fails to render.
pub async fn build_file_mentions(
    files_dir: &Path,
    content: &str,
    preview_limit: usize,
) -> Result<Option<String>, PromptMentionError> {
    let relative_paths = extract_file_mention_paths(content);
    if relative_paths.is_empty() {
        return Ok(None);
    }

    let mut files = Vec::new();
    for relative_path in relative_paths {
        let validated = parse_relative_file_mention(relative_path.as_str())?;
        let full_path = files_dir.join(&validated);
        let Ok(metadata) = async_fs::metadata(&full_path).await else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }

        let content_type = mime_guess::from_path(&full_path)
            .first_raw()
            .map(ToOwned::to_owned);
        let preview = async_fs::read_to_string(&full_path)
            .await
            .ok()
            .map(|content| truncate_preview_with_limit(content.as_str(), preview_limit));
        let name = validated
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_else(|| validated.as_os_str().to_str().unwrap_or("file"))
            .to_string();
        files.push(FileMention {
            path: validated.to_string_lossy().replace('\\', "/"),
            name,
            content_type,
            preview,
        });
    }

    if files.is_empty() {
        return Ok(None);
    }

    Ok(Some(
        FileMentions { files }
            .render()
            .expect("file mentions template must render"),
    ))
}

/// Extract session `@`-mention IDs from `content`, skipping mentions inside
/// code spans, fenced code blocks, and inline HTML.
#[must_use]
pub fn extract_session_mention_ids(content: &str) -> Vec<String> {
    extract_mention_candidates(content)
        .into_iter()
        .filter(|candidate| candidate.kind != MentionKind::File)
        .map(|candidate| candidate.value)
        .collect()
}

/// Extract file `@`-mention paths from `content`, skipping mentions inside
/// code spans, fenced code blocks, and inline HTML.
#[must_use]
pub fn extract_file_mention_paths(content: &str) -> Vec<String> {
    extract_mention_candidates(content)
        .into_iter()
        .filter(|candidate| candidate.kind != MentionKind::Session)
        .map(|candidate| candidate.value)
        .collect()
}

/// Truncate `content` to at most `max_chars` characters, appending `...` when
/// truncated.
#[must_use]
pub fn truncate_preview_with_limit(content: &str, max_chars: usize) -> String {
    let trimmed = content.trim();
    match trimmed.char_indices().nth(max_chars) {
        Some((pos, _)) => {
            let mut preview = String::with_capacity(pos + 3);
            preview.push_str(&trimmed[..pos]);
            preview.push_str("...");
            preview
        }
        None => trimmed.to_string(),
    }
}

fn extract_mention_candidates(content: &str) -> Vec<MentionCandidate> {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    // Walk the CommonMark event stream so we only see prose text. This
    // makes `@mention`s inside fenced code blocks, inline code spans,
    // and inline HTML invisible to extraction, which matches the user's
    // mental model: copying a snippet into the prompt should not
    // accidentally materialize a mention just because the snippet
    // contains an `@`.
    let parser = Parser::new(content);
    let mut in_code_block = false;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();

    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(_)) => in_code_block = true,
            Event::End(TagEnd::CodeBlock) => in_code_block = false,
            Event::Text(text) if !in_code_block => {
                scan_text_for_mentions(text.as_ref(), &mut seen, &mut out);
            }
            _ => {}
        }
    }

    out
}

/// Scan a single prose-only text fragment for `@`-prefixed mentions and
/// push deduplicated candidates into `out`.
fn scan_text_for_mentions(
    text: &str,
    seen: &mut std::collections::HashSet<(MentionKind, String)>,
    out: &mut Vec<MentionCandidate>,
) {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        // Only treat `@` as a mention start if it either sits at the
        // beginning of the fragment or follows an ASCII whitespace or
        // common opening-bracket delimiter. This mirrors the old regex
        // boundary `(^|[\s(\[{<])` so email addresses like
        // `alice@example.com` do not get picked up.
        let ch = bytes[index];
        if ch != b'@' || !is_mention_boundary(bytes, index) {
            index += 1;
            continue;
        }
        let start = index + 1;
        let end = text[start..]
            .find(is_mention_terminator)
            .map_or(text.len(), |offset| start + offset);
        let raw = &text[start..end];
        if !raw.is_empty()
            && let Some(candidate) = normalize_mention_candidate(raw)
        {
            let dedupe_key = (candidate.kind.clone(), candidate.value.clone());
            if seen.insert(dedupe_key) {
                out.push(candidate);
            }
        }
        index = end.max(index + 1);
    }
}

const fn is_mention_boundary(bytes: &[u8], at_index: usize) -> bool {
    if at_index == 0 {
        return true;
    }
    let prev = bytes[at_index - 1];
    matches!(
        prev,
        b' ' | b'\t' | b'\n' | b'\r' | b'(' | b'[' | b'{' | b'<'
    )
}

const fn is_mention_terminator(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            ')' | ']' | '}' | '>' | '"' | '\'' | '`' | ',' | '!' | '?' | ';'
        )
}

fn normalize_mention_candidate(raw: &str) -> Option<MentionCandidate> {
    let trimmed = trim_terminal_mention_punctuation(raw.trim());
    if trimmed.is_empty() {
        return None;
    }

    if let Some(value) = trimmed
        .strip_prefix("session:")
        .or_else(|| trimmed.strip_prefix("session/"))
    {
        return normalize_prefixed_candidate(MentionKind::Session, value);
    }

    if let Some(value) = trimmed
        .strip_prefix("file:")
        .or_else(|| trimmed.strip_prefix("file/"))
    {
        return normalize_prefixed_candidate(MentionKind::File, value);
    }

    Some(MentionCandidate {
        kind: MentionKind::Generic,
        value: trimmed.to_string(),
    })
}

fn normalize_prefixed_candidate(kind: MentionKind, raw: &str) -> Option<MentionCandidate> {
    let value = trim_terminal_mention_punctuation(raw.trim());
    if value.is_empty() {
        return None;
    }
    Some(MentionCandidate {
        kind,
        value: value.to_string(),
    })
}

fn trim_terminal_mention_punctuation(raw: &str) -> &str {
    raw.trim_end_matches(['.', ',', '!', '?', ';', ':'])
}

fn parse_relative_file_mention(raw: &str) -> Result<PathBuf, PromptMentionError> {
    let rel = PathBuf::from(raw.trim());
    if rel.is_absolute() {
        return Err(PromptMentionError::AbsolutePath);
    }
    for comp in rel.components() {
        if matches!(comp, std::path::Component::ParentDir) {
            return Err(PromptMentionError::ParentTraversal);
        }
    }
    Ok(rel)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        MentionCandidate, MentionKind, extract_file_mention_paths, extract_session_mention_ids,
        parse_relative_file_mention,
    };

    #[test]
    fn session_mentions_read_as_sentences() {
        let text = askama::Template::render(&super::SessionMentions {
            sessions: vec![super::SessionMention {
                id: "s1".to_string(),
                title: "Plan".to_string(),
                model: "m1".to_string(),
                recent_user_messages: vec!["first".to_string(), "second".to_string()],
            }],
        })
        .expect("render");
        assert_eq!(
            text,
            "<session_mentions>\nSession s1 titled \"Plan\" (model: m1).\n  Recent user message: first\n  Recent user message: second\n</session_mentions>"
        );
    }

    #[test]
    fn file_mentions_name_the_file_and_show_its_preview() {
        let text = askama::Template::render(&super::FileMentions {
            files: vec![
                super::FileMention {
                    path: "notes/a.md".to_string(),
                    name: "a.md".to_string(),
                    content_type: Some("text/markdown".to_string()),
                    preview: Some("# A".to_string()),
                },
                super::FileMention {
                    path: "b.bin".to_string(),
                    name: "b.bin".to_string(),
                    content_type: None,
                    preview: None,
                },
            ],
        })
        .expect("render");
        assert_eq!(
            text,
            "<file_mentions>\nFile notes/a.md (name: a.md, type: text/markdown).\nPreview:\n# A\nFile b.bin (name: b.bin).\n</file_mentions>"
        );
    }

    #[test]
    fn extracts_generic_session_mentions() {
        assert_eq!(
            extract_session_mention_ids("Check @session-regression-20260324 and @another-session."),
            vec![
                "session-regression-20260324".to_string(),
                "another-session".to_string(),
            ]
        );
    }

    #[test]
    fn extracts_generic_file_mentions() {
        assert_eq!(
            extract_file_mention_paths("Please inspect @slides.md and @docs/guide.txt"),
            vec!["slides.md".to_string(), "docs/guide.txt".to_string()]
        );
    }

    #[test]
    fn keeps_prefixed_mentions_typed() {
        let expected = vec![
            MentionCandidate {
                kind: MentionKind::Session,
                value: "abc123".to_string(),
            },
            MentionCandidate {
                kind: MentionKind::File,
                value: "notes/today.md".to_string(),
            },
        ];
        assert_eq!(
            super::extract_mention_candidates("see @session:abc123 and @file:notes/today.md"),
            expected
        );
    }

    #[test]
    fn parse_relative_file_mention_rejects_parent_traversal() {
        let error = parse_relative_file_mention("../secrets.txt")
            .expect_err("parent traversal must be rejected");
        assert_eq!(
            error.to_string(),
            "file mention path traversal is not allowed"
        );
    }

    #[test]
    fn parse_relative_file_mention_accepts_nested_relative_paths() {
        let path = parse_relative_file_mention("docs/guide.md")
            .expect("nested relative file mention must be accepted");
        assert_eq!(path, PathBuf::from("docs/guide.md"));
    }

    #[test]
    fn ignores_mentions_inside_fenced_code_blocks() {
        let content = "\
Please look at @real-session for context.

```
# command reference
send to @alice@example.com
or @ignored-session
```

After the block, @another-session also matters.";

        let ids = extract_session_mention_ids(content);
        assert_eq!(
            ids,
            vec!["real-session".to_string(), "another-session".to_string()]
        );
    }

    #[test]
    fn ignores_mentions_inside_inline_code_spans() {
        let content =
            "Use `@stale-session` as a literal example, but pay attention to @active-session.";
        let ids = extract_session_mention_ids(content);
        assert_eq!(ids, vec!["active-session".to_string()]);
    }

    #[test]
    fn ignores_mentions_after_non_boundary_characters() {
        let content = "Reach me at bob@example.org and ping @new-session.";
        let ids = extract_session_mention_ids(content);
        assert_eq!(ids, vec!["new-session".to_string()]);
    }
}
