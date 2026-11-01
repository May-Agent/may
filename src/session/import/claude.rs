//! Claude Code transcript (`~/.claude/projects/<dir>/<id>.jsonl`) →
//! [`ImportedTranscript`].
//!
//! Records form a tree via `uuid`/`parentUuid`; the live conversation is the
//! chain from the last record back to the root, which drops rewound/forked
//! branches. `isSidechain` records are Task-subagent internals and
//! `isMeta`/`type`-metadata records carry no conversation.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::SystemTime;

use aither_core::llm::{ReasoningState, ToolCall, ToolResult};
use jiff::Timestamp;
use serde_json::Value;

use super::{
    ForeignAgent, ForeignSessionSummary, ImportError, ImportedTranscript, assistant_turn,
    comment_call, derive_title, is_title_user_text, parse_ts, terminal_call, tool_end, user_text,
};
use crate::session::ChatMessage;

/// Quote `s` as a single-quoted shell word.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Map a Claude `tool_use` to a `terminal` call with a shell-equivalent script.
fn map_call(id: &str, name: &str, input: &Value) -> ToolCall {
    let s = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("");
    let call = |desc: &str, script: &str| terminal_call(id.to_string(), desc, script);
    match name {
        "Bash" => call(
            input
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("shell command"),
            s("command"),
        ),
        "Read" => {
            let path = s("file_path");
            let script = match (
                input.get("offset").and_then(Value::as_u64),
                input.get("limit").and_then(Value::as_u64),
            ) {
                (Some(offset), Some(limit)) => {
                    format!("sed -n '{},{}p' {}", offset + 1, offset + limit, shq(path))
                }
                (Some(offset), None) => format!("sed -n '{},$p' {}", offset + 1, shq(path)),
                _ => format!("cat -n {}", shq(path)),
            };
            call(&format!("read {path}"), &script)
        }
        "Write" => call(
            &format!("write {}", s("file_path")),
            &format!(
                "cat > {} <<'MAY_IMPORT_EOF'\n{}\nMAY_IMPORT_EOF",
                shq(s("file_path")),
                s("content")
            ),
        ),
        "Edit" => edit_script(
            id,
            s("file_path"),
            &[Edit {
                old: s("old_string"),
                new: s("new_string"),
                all: input
                    .get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }],
        ),
        "MultiEdit" => {
            let edits: Vec<Edit> = input["edits"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .map(|e| Edit {
                            old: e.get("old_string").and_then(Value::as_str).unwrap_or(""),
                            new: e.get("new_string").and_then(Value::as_str).unwrap_or(""),
                            all: e
                                .get("replace_all")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                        })
                        .collect()
                })
                .unwrap_or_default();
            edit_script(id, s("file_path"), &edits)
        }
        "Grep" => {
            let mut script = format!("rg --line-number {}", shq(s("pattern")));
            if !s("glob").is_empty() {
                script.push_str(" --glob ");
                script.push_str(&shq(s("glob")));
            }
            if input.get("-i").and_then(Value::as_bool).unwrap_or(false) {
                script.push_str(" -i");
            }
            if !s("path").is_empty() {
                script.push(' ');
                script.push_str(&shq(s("path")));
            }
            call(&format!("grep {}", s("pattern")), &script)
        }
        "Glob" => call(
            &format!("glob {}", s("pattern")),
            &format!("rg --files {} --glob {}", shq(s("path")), shq(s("pattern"))),
        ),
        "WebSearch" => call("websearch", &format!("websearch {}", shq(s("query")))),
        "WebFetch" => call("webfetch", &format!("webfetch {}", shq(s("url")))),
        _ => comment_call(id.to_string(), name, input),
    }
}

struct Edit<'a> {
    old: &'a str,
    new: &'a str,
    all: bool,
}

/// Render an Edit/MultiEdit as a python heredoc performing the exact
/// replacement — a real shell-runnable equivalent.
fn edit_script(id: &str, path: &str, edits: &[Edit]) -> ToolCall {
    let mut body = format!(
        "python3 - <<'MAY_EDIT_EOF'\nfrom pathlib import Path\np = Path({})\ns = p.read_text()\n",
        serde_json::to_string(path).unwrap_or_default()
    );
    for edit in edits {
        let count = if edit.all { "-1" } else { "1" };
        let _ = writeln!(
            body,
            "s = s.replace({}, {}, {count})",
            serde_json::to_string(edit.old).unwrap_or_default(),
            serde_json::to_string(edit.new).unwrap_or_default(),
        );
    }
    body.push_str("p.write_text(s)\nMAY_EDIT_EOF");
    terminal_call(id.to_string(), &format!("edit {path}"), &body)
}

/// Flatten a `tool_result` block's content to text.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                Some("image") => "[image]".to_string(),
                _ => serde_json::to_string(b).unwrap_or_default(),
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Conversation records eligible for the live branch.
fn is_conversation(record: &Value) -> bool {
    matches!(
        record.get("type").and_then(Value::as_str),
        Some("user" | "assistant")
    ) && !record
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && !record
            .get("isMeta")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

/// Providers reject an assistant `tool_use` that has no matching
/// `tool_result` — append an explicit error result for every imported call
/// whose result is absent from the live branch (dropped forks, interrupted
/// tail). The call record is kept; only its missing output is marked.
pub(super) fn backfill_missing_results(
    messages: &mut Vec<ChatMessage>,
    result_ids: &HashSet<String>,
) {
    let mut missing: Vec<(usize, String)> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        for call in &message.tool_calls {
            if !result_ids.contains(&call.id) {
                missing.push((index, call.id.clone()));
            }
        }
    }
    for (index, call_id) in missing.into_iter().rev() {
        let ts = messages[index].timestamp;
        messages.insert(
            index + 1,
            tool_end(
                &call_id,
                ToolResult::error("[tool result missing in transcript]"),
                ts,
            ),
        );
    }
}

pub(super) fn parse(path: &Path) -> Result<ImportedTranscript, ImportError> {
    let raw = std::fs::read_to_string(path).map_err(ImportError::Io)?;

    // All records are needed: the parentUuid chain runs through meta and
    // sidechain records too, even though only conversation records emit.
    let mut records: Vec<Value> = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line).map_err(|e| ImportError::Malformed {
            agent: ForeignAgent::Claude.label(),
            path: path.to_path_buf(),
            line: index + 1,
            reason: e.to_string(),
        })?;
        records.push(record);
    }

    // The live branch: walk parentUuid from the last conversation record.
    let mut by_id: HashMap<&str, &Value> = HashMap::new();
    let mut leaf = None;
    for record in &records {
        if let Some(uuid) = record.get("uuid").and_then(Value::as_str) {
            by_id.insert(uuid, record);
        }
        if is_conversation(record) {
            leaf = Some(record);
        }
    }
    let mut chain: Vec<&Value> = Vec::new();
    let mut cursor = leaf;
    let mut seen = HashSet::new();
    while let Some(record) = cursor {
        let uuid = record.get("uuid").and_then(Value::as_str).unwrap_or("");
        if !seen.insert(uuid.to_string()) {
            break; // parent loop — stop rather than emit a cycle
        }
        chain.push(record);
        cursor = record
            .get("parentUuid")
            .and_then(Value::as_str)
            .and_then(|parent| by_id.get(parent).copied());
    }
    chain.reverse();

    let mut messages: Vec<ChatMessage> = Vec::new();
    let mut emitted_calls: HashSet<String> = HashSet::new();
    let mut result_ids: HashSet<String> = HashSet::new();
    let mut cwd = None;
    let mut model = None;
    let mut source_id = String::new();

    for record in chain {
        if !is_conversation(record) {
            continue;
        }
        let ts = parse_ts(record.get("timestamp").and_then(Value::as_str));
        if let Some(c) = record.get("cwd").and_then(Value::as_str) {
            cwd = Some(c.to_string());
        }
        if source_id.is_empty()
            && let Some(id) = record.get("sessionId").and_then(Value::as_str)
        {
            source_id = id.to_string();
        }
        let message = &record["message"];
        match record.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                if let Some(m) = message.get("model").and_then(Value::as_str) {
                    model = Some(m.to_string());
                }
                let mut text = Vec::new();
                let mut reasoning = Vec::new();
                let mut calls = Vec::new();
                for block in message["content"].as_array().into_iter().flatten() {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(Value::as_str) {
                                text.push(t.to_string());
                            }
                        }
                        // thinking/redacted_thinking blocks replay verbatim as
                        // anthropic-tagged reasoning state.
                        Some("thinking" | "redacted_thinking") => {
                            reasoning.push(ReasoningState::new("anthropic", block.to_string()));
                        }
                        Some("tool_use") => {
                            let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                            emitted_calls.insert(id.to_string());
                            calls.push(map_call(id, name, &block["input"]));
                        }
                        _ => {}
                    }
                }
                if !text.is_empty() || !reasoning.is_empty() || !calls.is_empty() {
                    messages.push(assistant_turn(text.join("\n"), reasoning, calls, ts));
                }
            }
            Some("user") => {
                let content = &message["content"];
                let mut text = Vec::new();
                match content {
                    Value::String(s) => text.push(s.clone()),
                    Value::Array(blocks) => {
                        for block in blocks {
                            match block.get("type").and_then(Value::as_str) {
                                Some("text") => {
                                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                                        text.push(t.to_string());
                                    }
                                }
                                Some("tool_result") => {
                                    let id = block
                                        .get("tool_use_id")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    result_ids.insert(id.to_string());
                                    if emitted_calls.contains(id) {
                                        let body = result_text(&block["content"]);
                                        let result = if block
                                            .get("is_error")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false)
                                        {
                                            ToolResult::error(body)
                                        } else {
                                            ToolResult::text(body)
                                        };
                                        messages.push(tool_end(id, result, ts));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                let text = text.join("\n");
                let text = text.trim();
                if !text.is_empty() {
                    messages.push(user_text(text.to_string(), Vec::new(), ts));
                }
            }
            _ => {}
        }
    }

    backfill_missing_results(&mut messages, &result_ids);

    if messages.is_empty() {
        return Err(ImportError::Empty {
            agent: ForeignAgent::Claude.label(),
            path: path.to_path_buf(),
        });
    }
    Ok(ImportedTranscript {
        source_path: path.to_path_buf(),
        source_id,
        cwd,
        model,
        files: Vec::new(),
        title: derive_title(ForeignAgent::Claude, &messages),
        messages,
    })
}

/// Bytes of a transcript to read when building a picker row. Enough for
/// cwd + the first user prompt; the rest of a large jsonl is ignored.
const LIST_PEEK_BYTES: u64 = 64 * 1024;

/// Top-level `<project>/<id>.jsonl` transcripts under `projects_root`,
/// newest mtime first. Nested `subagents/` jsonl is skipped.
pub(super) fn list(projects_root: &Path) -> Vec<ForeignSessionSummary> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(projects_root) else {
        return out;
    };
    for project in projects.flatten() {
        let project_path = project.path();
        if !project_path.is_dir() {
            continue;
        }
        let slug = project.file_name().to_string_lossy().into_owned();
        let Ok(entries) = std::fs::read_dir(&project_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(summary) = summarize(&path, &slug) {
                out.push(summary);
            }
        }
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.modified));
    out
}

fn summarize(path: &Path, project_slug: &str) -> Option<ForeignSessionSummary> {
    let id = path.file_stem()?.to_str()?.to_string();
    let modified = std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(system_time_to_ts);
    let peeked = peek_records(path);
    let mut cwd = peeked.cwd;
    if cwd.is_none() {
        cwd = Some(decode_project_slug(project_slug));
    }
    let title = peeked
        .title
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "untitled".to_string());
    Some(ForeignSessionSummary {
        id,
        path: path.to_path_buf(),
        cwd,
        title,
        modified,
    })
}

fn system_time_to_ts(time: SystemTime) -> Option<Timestamp> {
    Timestamp::try_from(time).ok()
}

/// Claude encodes a cwd as a directory name by replacing `/` with `-`.
fn decode_project_slug(slug: &str) -> String {
    slug.strip_prefix('-').map_or_else(
        || slug.replace('-', "/"),
        |rest| format!("/{path}", path = rest.replace('-', "/")),
    )
}

struct Peeked {
    cwd: Option<String>,
    title: Option<String>,
}

fn peek_records(path: &Path) -> Peeked {
    let mut peeked = Peeked {
        cwd: None,
        title: None,
    };
    let Ok(file) = File::open(path) else {
        return peeked;
    };
    let mut buf = Vec::new();
    let Ok(n) = file.take(LIST_PEEK_BYTES).read_to_end(&mut buf) else {
        return peeked;
    };
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if u64::try_from(n).unwrap_or(0) == LIST_PEEK_BYTES
        && let Some(cut) = text.rfind('\n')
    {
        text.truncate(cut);
    }
    let mut fallback_title = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if peeked.cwd.is_none()
            && let Some(cwd) = record.get("cwd").and_then(Value::as_str)
            && !cwd.is_empty()
        {
            peeked.cwd = Some(cwd.to_string());
        }
        if peeked.title.is_some() {
            continue;
        }
        let Some(text) = user_prompt_text(&record) else {
            continue;
        };
        if !is_title_user_text(&text) {
            continue;
        }
        let preview = first_line_preview(&text);
        if preview.is_empty() {
            continue;
        }
        if preview.starts_with('/') {
            if fallback_title.is_none() {
                fallback_title = Some(preview);
            }
            continue;
        }
        peeked.title = Some(preview);
    }
    if peeked.title.is_none() {
        peeked.title = fallback_title;
    }
    peeked
}

fn user_prompt_text(record: &Value) -> Option<String> {
    if record.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    if record
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || record
            .get("isMeta")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return None;
    }
    match &record["message"]["content"] {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = block.get("text").and_then(Value::as_str)
                    && !t.is_empty()
                {
                    parts.push(t);
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

fn first_line_preview(text: &str) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    let truncated: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::MessageRole;
    use std::path::Path;

    #[test]
    fn walks_live_branch_and_skips_sidechains_and_meta() {
        let fixture = concat!(
            r#"{"type":"summary","summary":"old leaf"}"#,
            "\n",
            // dead branch: record b1 replaces a2 as the live child of a1
            r#"{"type":"user","uuid":"a1","parentUuid":null,"timestamp":"2026-10-25T10:00:00.000Z","cwd":"/repo","sessionId":"s-1","message":{"role":"user","content":"first prompt"}}"#,
            "\n",
            r#"{"type":"user","uuid":"a2","parentUuid":"a1","timestamp":"2026-10-25T10:01:00.000Z","message":{"role":"user","content":"dead branch"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"b1","parentUuid":"a1","timestamp":"2026-10-25T10:02:00.000Z","message":{"role":"assistant","model":"claude-x","content":[{"type":"thinking","thinking":"hmm","signature":"sig1"},{"type":"text","text":"reply"},{"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"/f.rs"}}]}}"#,
            "\n",
            // sidechain must be skipped even though it is a conversation record
            r#"{"type":"assistant","uuid":"b2","parentUuid":"b1","isSidechain":true,"timestamp":"2026-10-25T10:02:30.000Z","message":{"role":"assistant","content":[{"type":"text","text":"subagent"}]}}"#,
            "\n",
            // meta record must be skipped
            r#"{"type":"user","uuid":"b3","parentUuid":"b2","isMeta":true,"timestamp":"2026-10-25T10:02:40.000Z","message":{"role":"user","content":"<local-command/>"}}"#,
            "\n",
            r#"{"type":"user","uuid":"b4","parentUuid":"b1","timestamp":"2026-10-25T10:03:00.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"file body"}]}}"#,
            "\n",
        );
        let path = std::env::temp_dir().join("claude-test.jsonl");
        std::fs::write(&path, fixture).unwrap();
        let out = parse(&path).unwrap();
        assert_eq!(out.source_id, "s-1");
        assert_eq!(out.cwd.as_deref(), Some("/repo"));
        assert_eq!(out.model.as_deref(), Some("claude-x"));
        // user, assistant (thinking+text+call), tool end — dead branch a2, sidechain, meta dropped
        assert_eq!(out.messages.len(), 3);
        assert_eq!(out.messages[0].role, MessageRole::User);
        let a = &out.messages[1];
        assert_eq!(a.role, MessageRole::Assistant);
        assert_eq!(a.content, "reply");
        let thinking: Value = serde_json::from_str(a.reasoning[0].payload()).unwrap();
        assert_eq!(thinking["type"], "thinking");
        assert_eq!(thinking["signature"], "sig1");
        assert_eq!(a.tool_calls.len(), 1);
        assert_eq!(a.tool_calls[0].arguments["script"], "cat -n '/f.rs'");
        let end = &out.messages[2];
        assert_eq!(end.tool_call_id.as_deref(), Some("tu_1"));
        let record: Value = serde_json::from_str(&end.content).unwrap();
        assert_eq!(record["stage"], "end");
    }

    fn write_jsonl(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    fn set_mtime(path: &Path, secs: u64) {
        let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(false)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn list_skips_subagents_and_sorts_newest_first() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("-Users-lexoliu-Coding-may");
        std::fs::create_dir_all(project.join("subagents")).unwrap();

        let older = project.join("aaaa.jsonl");
        let newer = project.join("bbbb.jsonl");
        let nested = project.join("subagents").join("cccc.jsonl");
        write_jsonl(
            &older,
            r#"{"type":"user","cwd":"/Users/dev/Coding/may","message":{"role":"user","content":"older prompt"}}"#,
        );
        write_jsonl(
            &newer,
            r#"{"type":"user","cwd":"/Users/dev/Coding/may","message":{"role":"user","content":"newer prompt"}}"#,
        );
        write_jsonl(
            &nested,
            r#"{"type":"user","cwd":"/x","message":{"role":"user","content":"subagent"}}"#,
        );
        set_mtime(&older, 1_700_000_000);
        set_mtime(&newer, 1_700_000_100);
        set_mtime(&nested, 1_700_000_200);

        let listed = list(root.path());
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "bbbb");
        assert_eq!(listed[0].title, "newer prompt");
        assert_eq!(listed[1].id, "aaaa");
        assert_eq!(listed[1].title, "older prompt");
        assert_eq!(listed[0].cwd.as_deref(), Some("/Users/dev/Coding/may"));
    }

    #[test]
    fn list_prefers_real_prompt_over_slash_and_decodes_slug_cwd() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("-tmp-work");
        std::fs::create_dir_all(&project).unwrap();
        write_jsonl(
            &project.join("s1.jsonl"),
            concat!(
                r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command/>"}}"#,
                "\n",
                r#"{"type":"user","message":{"role":"user","content":"/help"}}"#,
                "\n",
                r#"{"type":"user","message":{"role":"user","content":"real question"}}"#,
                "\n",
            ),
        );

        let listed = list(root.path());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "real question");
        assert_eq!(listed[0].cwd.as_deref(), Some("/tmp/work"));
    }

    #[test]
    fn list_falls_back_to_slash_title_when_that_is_all() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        write_jsonl(
            &project.join("s2.jsonl"),
            r#"{"type":"user","message":{"role":"user","content":"/compact"}}"#,
        );

        let listed = list(root.path());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "/compact");
    }
}
