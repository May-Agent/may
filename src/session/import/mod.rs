//! Import foreign agent transcripts (Codex, Claude Code) into May sessions.
//!
//! Parsers are pure and offline: they read a transcript file and produce the
//! [`ChatMessage`] rows a new session is seeded with, plus the metadata the
//! session row should carry (title, model, source description).

pub mod claude;
pub mod codex;

use std::path::{Path, PathBuf};

use aither_core::llm::{ReasoningState, ToolCall};
use jiff::Timestamp;

use super::{ChatMessage, MessageRole};

/// The foreign agent whose transcript is being imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignAgent {
    /// `~/.codex/sessions/**/rollout-*.jsonl`.
    Codex,
    /// `~/.claude/projects/<cwd-slug>/<session>.jsonl`.
    Claude,
}

impl ForeignAgent {
    /// Slash command name for this source.
    #[must_use]
    pub const fn command(&self) -> &'static str {
        match self {
            Self::Codex => "/resume-codex",
            Self::Claude => "/resume-claude",
        }
    }

    /// Short source label used in titles and notices.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

/// Failure to locate or translate a foreign transcript.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// Filesystem I/O failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// No transcript matched the given session id or path.
    #[error("no {agent} transcript matches '{query}'")]
    NotFound {
        /// Source label.
        agent: &'static str,
        /// The session id or path the user gave.
        query: String,
    },
    /// More than one transcript matched; the id prefix is ambiguous.
    #[error("ambiguous {agent} session id '{query}', matches:\n{}", .matches.join("\n"))]
    Ambiguous {
        /// Source label.
        agent: &'static str,
        /// The session id prefix the user gave.
        query: String,
        /// Matching transcript paths.
        matches: Vec<String>,
    },
    /// The transcript parsed but produced no importable conversation.
    #[error("{agent} transcript at {} has no conversation to import", .path.display())]
    Empty {
        /// Source label.
        agent: &'static str,
        /// Transcript path that was parsed.
        path: PathBuf,
    },
    /// A record that cannot be skipped safely is malformed.
    #[error("{agent} transcript at {} line {line}: {reason}", .path.display())]
    Malformed {
        /// Source label.
        agent: &'static str,
        /// Transcript path that was parsed.
        path: PathBuf,
        /// 1-based line number.
        line: usize,
        /// What went wrong.
        reason: String,
    },
}

/// A transcript translated into May's session format.
#[derive(Debug)]
pub struct ImportedTranscript {
    /// Path of the source transcript.
    pub source_path: PathBuf,
    /// Foreign session id.
    pub source_id: String,
    /// Working directory the foreign session ran in, when recorded.
    pub cwd: Option<String>,
    /// Session title derived from the first real user prompt.
    pub title: String,
    /// Model the foreign session last ran on, when recorded.
    pub model: Option<String>,
    /// Messages to append to the fresh session (in order).
    pub messages: Vec<ChatMessage>,
    /// Image payloads to persist into the session dir before appending; the
    /// messages' attachment URLs already point at their final paths.
    pub files: Vec<(PathBuf, Vec<u8>)>,
}

/// Locate the transcript for `query` and translate it.
///
/// `query` is either a session id (exact or unambiguous prefix) or a direct
/// filesystem path to the transcript file.
///
/// # Errors
/// Returns [`ImportError`] when the transcript cannot be found, is ambiguous,
/// or fails to parse.
pub fn import(source: ForeignAgent, query: &str) -> Result<ImportedTranscript, ImportError> {
    let query = query.trim();
    let path = resolve_transcript_path(source, query)?;
    match source {
        ForeignAgent::Codex => codex::parse(&path),
        ForeignAgent::Claude => claude::parse(&path),
    }
}

/// Resolve `query` (a session id/prefix or a filesystem path) to a transcript
/// file.
fn resolve_transcript_path(source: ForeignAgent, query: &str) -> Result<PathBuf, ImportError> {
    if query.is_empty() {
        return Err(ImportError::NotFound {
            agent: source.label(),
            query: query.to_string(),
        });
    }
    let as_path = Path::new(query);
    if as_path.is_file() {
        return Ok(as_path.to_path_buf());
    }
    let home = dirs::home_dir().ok_or_else(|| ImportError::NotFound {
        agent: source.label(),
        query: query.to_string(),
    })?;
    let matches = match source {
        ForeignAgent::Codex => find_prefixed(&home.join(".codex/sessions"), query, true),
        ForeignAgent::Claude => find_prefixed(&home.join(".claude/projects"), query, false),
    };
    match matches.len() {
        0 => Err(ImportError::NotFound {
            agent: source.label(),
            query: query.to_string(),
        }),
        1 => Ok(matches.into_iter().next().expect("len == 1")),
        _ => Err(ImportError::Ambiguous {
            agent: source.label(),
            query: query.to_string(),
            matches: matches.iter().map(|p| p.display().to_string()).collect(),
        }),
    }
}

/// Collect transcript files matching a session id or prefix.
///
/// Codex embeds the id in the rollout filename, so a name-contains match works;
/// Claude names files `<id>.jsonl`, so a file-stem prefix match works there.
fn find_prefixed(root: &Path, query: &str, name_contains: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return out;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let hit = if name_contains {
                name.contains(query)
            } else {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|stem| stem == query || stem.starts_with(query))
            };
            if hit {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Parse an RFC3339/ISO-8601 timestamp field; foreign transcripts always use
/// ISO timestamps, so a parse failure keeps `None` rather than failing import.
fn parse_ts(value: Option<&str>) -> Option<Timestamp> {
    value.and_then(|v| v.parse::<Timestamp>().ok())
}

/// Build a `terminal` tool call for an imported call.
pub(super) fn terminal_call(id: String, description: &str, script: &str) -> ToolCall {
    ToolCall::new(
        id,
        "terminal",
        serde_json::json!({
            "description": description,
            "script": script,
            "timeout": 600,
        }),
    )
}

/// A `terminal` tool call whose script is an inert comment carrying the
/// original tool name and arguments — used when there is no honest shell
/// equivalent for the foreign call.
pub(super) fn comment_call(id: String, name: &str, arguments: &serde_json::Value) -> ToolCall {
    let args = serde_json::to_string(arguments).unwrap_or_default();
    terminal_call(
        id,
        &format!("{name} (imported, not re-runnable)"),
        &format!("# {name} {args}"),
    )
}

/// A completed `terminal` tool event row.
pub(super) fn tool_end(
    call_id: &str,
    result: aither_core::llm::ToolResult,
    timestamp: Option<Timestamp>,
) -> ChatMessage {
    let record = super::ToolEventRecord::End {
        id: call_id.to_string(),
        name: "terminal".to_string(),
        result,
    };
    ChatMessage {
        id: None,
        role: MessageRole::Tool,
        content: serde_json::to_string(&record).unwrap_or_default(),
        attachments: Vec::new(),
        timestamp,
        tool_call_id: Some(call_id.to_string()),
        stats: None,
        reasoning: Vec::new(),
        tool_calls: Vec::new(),
    }
}

/// An assistant row carrying text, reasoning and tool calls of one turn.
pub(super) const fn assistant_turn(
    text: String,
    reasoning: Vec<ReasoningState>,
    tool_calls: Vec<ToolCall>,
    timestamp: Option<Timestamp>,
) -> ChatMessage {
    ChatMessage {
        id: None,
        role: MessageRole::Assistant,
        content: text,
        attachments: Vec::new(),
        timestamp,
        tool_call_id: None,
        stats: None,
        reasoning,
        tool_calls,
    }
}

/// A user message row.
pub(super) const fn user_text(
    text: String,
    attachments: Vec<crate::session::PersistedAttachment>,
    timestamp: Option<Timestamp>,
) -> ChatMessage {
    ChatMessage {
        id: None,
        role: MessageRole::User,
        content: text,
        attachments,
        timestamp,
        tool_call_id: None,
        stats: None,
        reasoning: Vec::new(),
        tool_calls: Vec::new(),
    }
}

/// Derive a session title from the first real user message.
///
/// A compaction-continuation preamble is what the model sees, not what the
/// human asked — it is skipped for titling.
pub(super) fn derive_title(source: ForeignAgent, messages: &[ChatMessage]) -> String {
    let first = messages
        .iter()
        .find(|m| {
            m.role == MessageRole::User
                && ![
                    "This session is being continued from a previous conversation",
                    "<task-notification",
                    "<local-command",
                    "<command-name",
                    "<command-message",
                    "<command-args",
                ]
                .iter()
                .any(|prefix| m.content.starts_with(prefix))
        })
        .map_or("imported session", |m| m.content.as_str());
    let line = first.lines().next().unwrap_or(first).trim();
    let truncated: String = line.chars().take(60).collect();
    let truncated = if line.chars().count() > 60 {
        format!("{truncated}…")
    } else {
        truncated
    };
    format!("[{}] {truncated}", source.label())
}
