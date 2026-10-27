//! Claude Code transcript (`~/.claude/projects/<dir>/<id>.jsonl`) →
//! [`ImportedTranscript`].
//!
//! Records form a tree via `uuid`/`parentUuid`; the live conversation is the
//! chain from the last record back to the root, which drops rewound/forked
//! branches. `isSidechain` records are Task-subagent internals and
//! `isMeta`/`type`-metadata records carry no conversation.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use aither_core::llm::{ReasoningState, ToolCall, ToolResult};
use serde_json::Value;

use super::{
    ForeignAgent, ImportError, ImportedTranscript, assistant_turn, comment_call, derive_title,
    parse_ts, terminal_call, tool_end, user_text,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::MessageRole;

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
}
