//! Codex rollout transcript (`rollout-*.jsonl`) → [`ImportedTranscript`].
//!
//! The file is an append-only log of `{timestamp, type, payload}` records. The
//! model-facing context lives in `response_item` records (Responses API items);
//! `compacted` records replace everything before them with their
//! `replacement_history`, so the effective context is the last compaction's
//! replacement list plus the items appended after it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use aither_core::llm::{ReasoningState, ToolCall, ToolResult};
use jiff::Timestamp;
use serde_json::Value;

use super::{
    ForeignAgent, ImportError, ImportedTranscript, assistant_turn, comment_call, derive_title,
    parse_ts, terminal_call, tool_end, user_text,
};
use crate::session::{AttachmentKind, ChatMessage, PersistedAttachment};

/// Pending content of one imported assistant turn.
#[derive(Default)]
struct Turn {
    timestamp: Option<Timestamp>,
    text: Vec<String>,
    reasoning: Vec<ReasoningState>,
    calls: Vec<ToolCall>,
    results: Vec<(String, ToolResult, Option<Timestamp>)>,
}

impl Turn {
    const fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.reasoning.is_empty()
            && self.calls.is_empty()
            && self.results.is_empty()
    }
}

pub(super) fn parse(path: &Path) -> Result<ImportedTranscript, ImportError> {
    let raw = std::fs::read_to_string(path).map_err(ImportError::Io)?;

    let mut items: Vec<(Option<String>, Value)> = Vec::new();
    let mut source_id = String::new();
    let mut cwd = None;
    let mut model = None;

    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line).map_err(|e| ImportError::Malformed {
            agent: ForeignAgent::Codex.label(),
            path: path.to_path_buf(),
            line: index + 1,
            reason: e.to_string(),
        })?;
        let timestamp = record
            .get("timestamp")
            .and_then(Value::as_str)
            .map(String::from);
        match record.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                let payload = &record["payload"];
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    source_id = id.to_string();
                }
                cwd = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or(cwd);
            }
            Some("turn_context") => {
                if let Some(m) = record["payload"].get("model").and_then(Value::as_str) {
                    model = Some(m.to_string());
                }
            }
            Some("compacted") => {
                // Compaction replaced the whole history: keep only the
                // replacement list as the base for what follows.
                items = record["payload"]["replacement_history"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .cloned()
                            .map(|item| (timestamp.clone(), item))
                            .collect()
                    })
                    .unwrap_or_default();
            }
            Some("response_item") => {
                items.push((timestamp, record["payload"].clone()));
            }
            _ => {}
        }
    }

    let mut messages: Vec<ChatMessage> = Vec::new();
    let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut emitted_calls: HashSet<String> = HashSet::new();
    let mut result_ids: HashSet<String> = HashSet::new();
    let mut turn = Turn::default();

    for (timestamp, item) in items {
        let ts = parse_ts(timestamp.as_deref());
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("");
                match role {
                    "assistant" => {
                        if turn.timestamp.is_none() {
                            turn.timestamp = ts;
                        }
                        for block in item["content"].as_array().into_iter().flatten() {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                turn.text.push(text.to_string());
                            }
                        }
                    }
                    "user" => {
                        flush(
                            &mut turn,
                            &mut messages,
                            &mut emitted_calls,
                            &mut result_ids,
                        );
                        let (text, images) = user_content(&item, path, &mut files);
                        if !text.is_empty() || !images.is_empty() {
                            messages.push(user_text(text, images, ts));
                        }
                    }
                    // developer messages carry instructions/environment context
                    // of the foreign runtime — not conversation.
                    _ => {
                        flush(
                            &mut turn,
                            &mut messages,
                            &mut emitted_calls,
                            &mut result_ids,
                        );
                    }
                }
            }
            Some("reasoning") => {
                if turn.timestamp.is_none() {
                    turn.timestamp = ts;
                }
                let payload = serde_json::json!({
                    "id": item.get("id"),
                    "encrypted_content": item.get("encrypted_content"),
                });
                turn.reasoning
                    .push(ReasoningState::new("openai", payload.to_string()));
            }
            Some("function_call" | "custom_tool_call") => {
                if turn.timestamp.is_none() {
                    turn.timestamp = ts;
                }
                let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
                let call_id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let input = item
                    .get("arguments")
                    .or_else(|| item.get("input"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                turn.calls.push(map_call(call_id, name, input));
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let output = item.get("output").map(output_text).unwrap_or_default();
                turn.results.push((call_id, ToolResult::text(output), ts));
            }
            _ => {}
        }
    }
    flush(
        &mut turn,
        &mut messages,
        &mut emitted_calls,
        &mut result_ids,
    );
    super::claude::backfill_missing_results(&mut messages, &result_ids);

    if messages.is_empty() {
        return Err(ImportError::Empty {
            agent: ForeignAgent::Codex.label(),
            path: path.to_path_buf(),
        });
    }
    Ok(ImportedTranscript {
        source_path: path.to_path_buf(),
        source_id,
        cwd,
        model,
        files,
        title: derive_title(ForeignAgent::Codex, &messages),
        messages,
    })
}

/// Emit the buffered assistant turn: one assistant row carrying text,
/// reasoning and calls, then tool-result rows for outputs whose calls were
/// actually emitted.
fn flush(
    turn: &mut Turn,
    messages: &mut Vec<ChatMessage>,
    emitted_calls: &mut HashSet<String>,
    result_ids: &mut HashSet<String>,
) {
    if turn.is_empty() {
        return;
    }
    let buffered = std::mem::take(turn);
    if !buffered.text.is_empty() || !buffered.reasoning.is_empty() || !buffered.calls.is_empty() {
        for call in &buffered.calls {
            emitted_calls.insert(call.id.clone());
        }
        messages.push(assistant_turn(
            buffered.text.join("\n"),
            buffered.reasoning,
            buffered.calls,
            buffered.timestamp,
        ));
    }
    for (call_id, result, ts) in buffered.results {
        if emitted_calls.contains(&call_id) {
            result_ids.insert(call_id.clone());
            messages.push(tool_end(&call_id, result, ts));
        }
    }
}

/// Extract user-visible text and image attachments from a `user` message item.
/// `environment_context` blocks belong to the foreign runtime and are dropped.
fn user_content(
    item: &Value,
    path: &Path,
    files: &mut Vec<(PathBuf, Vec<u8>)>,
) -> (String, Vec<PersistedAttachment>) {
    let mut text = Vec::new();
    let mut images = Vec::new();
    for block in item["content"].as_array().into_iter().flatten() {
        match block.get("type").and_then(Value::as_str) {
            Some("input_text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    let trimmed = t.trim_start();
                    if !trimmed.starts_with("<environment_context")
                        && !trimmed.starts_with("# AGENTS.md instructions")
                    {
                        text.push(t.to_string());
                    }
                }
            }
            Some("input_image") => {
                if let Some(url) = block.get("image_url").and_then(Value::as_str) {
                    let attachment = image_attachment(url, path, files);
                    images.push(attachment);
                }
            }
            _ => {}
        }
    }
    (text.join("\n"), images)
}

/// Turn an `input_image` payload into a persisted attachment, stashing decoded
/// bytes into `files` when the URL is a data URL.
fn image_attachment(
    url: &str,
    transcript: &Path,
    files: &mut Vec<(PathBuf, Vec<u8>)>,
) -> PersistedAttachment {
    use base64::Engine;
    if let Some((_mime, data)) = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
        .and_then(|(mime, data)| {
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .ok()
                .map(|bytes| (mime.to_string(), bytes))
        })
    {
        let index = files.len();
        let name = format!("imported-image-{index}");
        files.push((PathBuf::from(format!("files/{name}")), data));
        return PersistedAttachment {
            id: name.clone(),
            url: format!("file://{name}"),
            name,
            attachment_type: AttachmentKind::Image,
            upload_data_url: None,
            text_content: None,
        };
    }
    PersistedAttachment {
        id: url.to_string(),
        name: transcript
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("image")
            .to_string(),
        attachment_type: AttachmentKind::Image,
        url: url.to_string(),
        upload_data_url: None,
        text_content: None,
    }
}

/// Flatten a tool output field to plain text. Codex stores either a plain
/// string or a JSON array of `{type, text}` blocks.
fn output_text(value: &Value) -> String {
    match value {
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .as_ref()
            .map_or_else(|| s.clone(), blocks_text),
        other => blocks_text(other),
    }
}

fn blocks_text(value: &Value) -> String {
    if let Some(blocks) = value.as_array() {
        let parts: Vec<&str> = blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    value.as_str().unwrap_or_default().to_string()
}

/// Map a Codex call to a `terminal` call with a shell-equivalent script.
fn map_call(call_id: String, name: &str, input: &str) -> ToolCall {
    let args: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    let arg = |key: &str| args.get(key).and_then(Value::as_str);
    match name {
        "exec_command" | "shell" | "local_shell" | "container.exec" => {
            let command = arg("cmd")
                .or_else(|| arg("command"))
                .unwrap_or(input)
                .to_string();
            terminal_call(call_id, name, command)
        }
        "write_stdin" => terminal_call(
            call_id,
            "write to a running terminal session",
            arg("chars").unwrap_or(input).to_string(),
        ),
        "apply_patch" => terminal_call(
            call_id,
            "apply_patch",
            format!(
                "apply_patch <<'PATCH'\n{}\nPATCH",
                arg("input").unwrap_or(input)
            ),
        ),
        "exec" => {
            // The codex `exec` tool runs a JS orchestration script verbatim.
            terminal_call(
                call_id,
                "exec orchestration script",
                input.to_string(),
            )
        }
        _ => comment_call(call_id, name, &args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_messages_reasoning_calls_and_skips_developer() {
        let fixture = concat!(
            r#"{"timestamp":"2026-10-26T02:50:00.000Z","type":"session_meta","payload":{"id":"abc-123","cwd":"/tmp/x"}}"#,
            "\n",
            r#"{"timestamp":"2026-10-26T02:50:01.000Z","type":"turn_context","payload":{"model":"gpt-5-codex"}}"#,
            "\n",
            r#"{"timestamp":"2026-10-26T02:50:02.000Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"secret instructions"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-10-26T02:50:03.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x</environment_context>"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-10-26T02:50:04.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello there"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-10-26T02:50:05.000Z","type":"response_item","payload":{"type":"reasoning","id":"rs_1","encrypted_content":"AAAA","summary":[]}}"#,
            "\n",
            r#"{"timestamp":"2026-10-26T02:50:06.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"ls -la\"}","call_id":"call_1"}}"#,
            "\n",
            r#"{"timestamp":"2026-10-26T02:50:07.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"total 0"}}"#,
            "\n",
            r#"{"timestamp":"2026-10-26T02:50:08.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
            "\n",
        );
        let dir = std::env::temp_dir();
        let path = dir.join("codex-test.jsonl");
        std::fs::write(&path, fixture).unwrap();
        let out = parse(&path).unwrap();
        assert_eq!(out.source_id, "abc-123");
        assert_eq!(out.model.as_deref(), Some("gpt-5-codex"));
        // developer + env-context user dropped; user, assistant, tool end remain
        assert_eq!(out.messages.len(), 3);
        assert_eq!(out.messages[0].role, crate::session::MessageRole::User);
        assert_eq!(out.messages[0].content, "hello there");
        let assistant = &out.messages[1];
        assert_eq!(assistant.content, "done");
        assert_eq!(assistant.reasoning.len(), 1);
        let state: Value = serde_json::from_str(assistant.reasoning[0].payload()).unwrap();
        assert_eq!(state["id"], "rs_1");
        assert_eq!(state["encrypted_content"], "AAAA");
        assert_eq!(assistant.tool_calls.len(), 1);
        assert_eq!(assistant.tool_calls[0].name, "terminal");
        assert_eq!(assistant.tool_calls[0].arguments["script"], "ls -la");
        assert_eq!(assistant.tool_calls[0].id, "call_1");
        let end = &out.messages[2];
        assert_eq!(end.role, crate::session::MessageRole::Tool);
        assert_eq!(end.tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn compacted_record_replaces_prior_items() {
        let fixture = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"dropped"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"compacted","payload":{"replacement_history":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"summary"}]}]}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:02.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"after"}]}}"#,
            "\n",
        );
        let path = std::env::temp_dir().join("codex-compact.jsonl");
        std::fs::write(&path, fixture).unwrap();
        let out = parse(&path).unwrap();
        let texts: Vec<_> = out.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(texts, ["summary", "after"]);
    }
}
