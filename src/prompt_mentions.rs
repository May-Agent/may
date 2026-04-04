use std::path::{Path, PathBuf};

use async_fs;
use serde::Serialize;

use crate::db::{ChatDb, ChatDbError};

#[derive(Debug, thiserror::Error)]
pub enum PromptMentionError {
    #[error("failed to load session metadata: {0}")]
    SessionLookup(#[from] ChatDbError),
    #[error("file mention path must be relative")]
    AbsolutePath,
    #[error("file mention path traversal is not allowed")]
    ParentTraversal,
    #[error("failed to serialize mention XML: {0}")]
    Serialize(#[from] quick_xml::SeError),
}

#[derive(Serialize)]
struct SessionMentionXml {
    #[serde(rename = "@id")]
    id: String,
    #[serde(rename = "@title")]
    title: String,
    #[serde(rename = "@model")]
    model: String,
    #[serde(rename = "recent_user_message", default)]
    recent_user_messages: Vec<TextXmlBlockOwned>,
}

#[derive(Serialize)]
struct SessionMentionsXml {
    #[serde(rename = "session")]
    sessions: Vec<SessionMentionXml>,
}

#[derive(Serialize)]
struct TextXmlBlockOwned {
    #[serde(rename = "$text")]
    content: String,
}

#[derive(Serialize)]
struct FileMentionXml {
    #[serde(rename = "@path")]
    path: String,
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@content_type", skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
    #[serde(rename = "preview", skip_serializing_if = "Option::is_none")]
    preview: Option<TextXmlBlockOwned>,
}

#[derive(Serialize)]
struct FileMentionsXml {
    #[serde(rename = "file")]
    files: Vec<FileMentionXml>,
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

pub async fn build_session_mentions_xml(
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
                .map(|message| TextXmlBlockOwned {
                    content: truncate_preview_with_limit(&message.content, 160),
                })
                .collect::<Vec<_>>();
            entries.push(SessionMentionXml {
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

    Ok(Some(crate::xml_fragment::serialize_tagged_xml(
        "session_mentions",
        &SessionMentionsXml { sessions: entries },
    )?))
}

pub async fn build_file_mentions_xml(
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
        let metadata = match async_fs::metadata(&full_path).await {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }

        let content_type = mime_guess::from_path(&full_path)
            .first_raw()
            .map(ToOwned::to_owned);
        let preview = match async_fs::read_to_string(&full_path).await {
            Ok(content) => Some(TextXmlBlockOwned {
                content: truncate_preview_with_limit(content.as_str(), preview_limit),
            }),
            Err(_) => None,
        };
        let name = validated
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(validated.as_os_str().to_str().unwrap_or("file"))
            .to_string();
        files.push(FileMentionXml {
            path: validated.to_string_lossy().replace('\\', "/"),
            name,
            content_type,
            preview,
        });
    }

    if files.is_empty() {
        return Ok(None);
    }

    Ok(Some(crate::xml_fragment::serialize_tagged_xml(
        "file_mentions",
        &FileMentionsXml { files },
    )?))
}

pub fn extract_session_mention_ids(content: &str) -> Vec<String> {
    extract_mention_candidates(content)
        .into_iter()
        .filter(|candidate| candidate.kind != MentionKind::File)
        .map(|candidate| candidate.value)
        .collect()
}

pub fn extract_file_mention_paths(content: &str) -> Vec<String> {
    extract_mention_candidates(content)
        .into_iter()
        .filter(|candidate| candidate.kind != MentionKind::Session)
        .map(|candidate| candidate.value)
        .collect()
}

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
    static MENTION_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(^|[\s(\[{<])@([^\s)\]}>"'`,!?;]+)"#)
            .expect("generic mention regex must be valid")
    });

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for captures in MENTION_RE.captures_iter(content) {
        let Some(matched) = captures.get(2) else {
            continue;
        };
        let Some(candidate) = normalize_mention_candidate(matched.as_str()) else {
            continue;
        };
        let dedupe_key = (candidate.kind.clone(), candidate.value.clone());
        if seen.insert(dedupe_key) {
            out.push(candidate);
        }
    }
    out
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
}
