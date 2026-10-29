//! DB message → API message mapping.

use serde_json::{Value, json};

use crate::session::{MessageRole, PersistedMessage, PersistedMessageError};

use super::terminal_parsing::update_terminal_snapshot_from_tool_event;
use super::types::{
    ApiMessage, RequestStats, SkillInfoDto, TerminalStateSnapshot, ToolEventRecord, ToolVariant,
};

/// Convert DB message rows to API messages, with native terminal execution tracking.
///
/// Tool rows (non-terminal tools such as `web_search`, `browser`, `task`) are attached
/// to an assistant message within the **same turn**. A turn is bounded by user rows:
/// tools that appear without a preceding assistant in the same turn attach to the
/// FOLLOWING assistant (the one about to speak); trailing tools that follow an
/// assistant attach to that preceding assistant. This preserves chronological
/// text↔tool interleave when the UI re-renders.
pub(super) fn map_db_messages(
    db_messages: &[crate::db::entity::messages::Model],
) -> Result<Vec<ApiMessage>, PersistedMessageError> {
    let mut out = Vec::new();
    let mut snapshot = TerminalStateSnapshot::default();
    let mut pending_tool_events: Vec<Value> = Vec::new();
    let mut last_assistant_in_turn: Option<usize> = None;

    for (idx, msg) in db_messages.iter().enumerate() {
        let persisted = PersistedMessage::try_from(msg)?;
        match persisted.role {
            MessageRole::Tool => {
                if let Ok(record) = serde_json::from_str::<ToolEventRecord>(&msg.content) {
                    update_terminal_snapshot_from_tool_event(&mut snapshot, &record);
                    pending_tool_events
                        .push(tool_event_to_api_json(&record, pending_tool_events.len()));
                }
            }
            MessageRole::PermissionRequest
            | MessageRole::PermissionResponse
            | MessageRole::WorkspaceRequest
            | MessageRole::WorkspaceResponse
            | MessageRole::AskUserRequest
            | MessageRole::AskUserResponse
            | MessageRole::HiddenContext => {}
            MessageRole::User => {
                flush_trailing_tools_in_turn(
                    &mut out,
                    &mut pending_tool_events,
                    last_assistant_in_turn,
                );
                last_assistant_in_turn = None;

                let timestamp = persisted.require_timestamp()?.to_string();
                out.push(ApiMessage {
                    id: format!("msg-{idx}"),
                    role: persisted.role.as_str().to_string(),
                    content: msg.content.clone(),
                    timestamp,
                    attachments: super::chat::api_attachments_from_persisted(
                        persisted.attachments.clone(),
                    ),
                    thinking: None,
                    is_streaming: Some(false),
                    is_thinking: Some(false),
                    tool_event: None,
                    terminal_executions: Vec::new(),
                    stats: map_db_message_stats(msg),
                });
            }
            MessageRole::Assistant | MessageRole::System => {
                let timestamp = persisted.require_timestamp()?.to_string();

                let tool_event = if persisted.role == MessageRole::Assistant
                    && !pending_tool_events.is_empty()
                {
                    Some(Value::Array(std::mem::take(&mut pending_tool_events)))
                } else {
                    None
                };

                let mut api_msg = ApiMessage {
                    id: format!("msg-{idx}"),
                    role: persisted.role.as_str().to_string(),
                    content: msg.content.clone(),
                    timestamp,
                    attachments: Vec::new(),
                    thinking: None,
                    is_streaming: Some(false),
                    is_thinking: Some(false),
                    tool_event,
                    terminal_executions: Vec::new(),
                    stats: map_db_message_stats(msg),
                };

                if persisted.role == MessageRole::Assistant
                    && (!snapshot.executions.is_empty() || !snapshot.running_by_call_id.is_empty())
                {
                    api_msg.terminal_executions = snapshot.all_executions();
                }

                out.push(api_msg);
                if persisted.role == MessageRole::Assistant {
                    last_assistant_in_turn = Some(out.len() - 1);
                }
            }
        }
    }

    flush_trailing_tools_in_turn(&mut out, &mut pending_tool_events, last_assistant_in_turn);

    Ok(out)
}

/// Flush tools that followed an assistant inside the just-ended turn onto that
/// assistant. Called on turn boundary (user row) and at end of iteration.
/// Tools with no assistant to attach to in the current turn are dropped.
fn flush_trailing_tools_in_turn(
    out: &mut [ApiMessage],
    pending: &mut Vec<Value>,
    last_assistant_idx: Option<usize>,
) {
    if pending.is_empty() {
        return;
    }
    match last_assistant_idx {
        Some(idx) => {
            let events = std::mem::take(pending);
            let target = &mut out[idx];
            match target.tool_event.take() {
                Some(Value::Array(mut existing)) => {
                    existing.extend(events);
                    target.tool_event = Some(Value::Array(existing));
                }
                _ => {
                    target.tool_event = Some(Value::Array(events));
                }
            }
        }
        None => {
            pending.clear();
        }
    }
}

/// Enrich a stored `ToolEventRecord` with `variant` + `seq` + `summary`
/// for the API shape that the frontend's persisted-assistant renderer
/// expects. Summary is projected server-side so refresh and live stream
/// surface identical labels.
fn tool_event_to_api_json(record: &ToolEventRecord, seq: usize) -> Value {
    match record {
        ToolEventRecord::Start {
            id,
            name,
            arguments,
        } => {
            let variant = ToolVariant::infer(name);
            let summary = super::tool_summary::project_tool_summary(arguments, variant);
            json!({
                "stage": "start",
                "id": id,
                "name": name,
                "variant": variant,
                "arguments": arguments,
                "seq": seq,
                "summary": summary,
            })
        }
        ToolEventRecord::End { id, name, result } => json!({
            "stage": "end",
            "id": id,
            "name": name,
            "variant": ToolVariant::infer(name),
            "result": crate::session::tool_result_wire::WireToolResult::from(result),
            "seq": seq,
        }),
    }
}

fn map_db_message_stats(msg: &crate::db::entity::messages::Model) -> Option<RequestStats> {
    let prompt_tokens = msg.prompt_tokens.and_then(|v| u32::try_from(v).ok());
    let completion_tokens = msg.completion_tokens.and_then(|v| u32::try_from(v).ok());
    let total_tokens = msg
        .total_tokens
        .and_then(|v| u32::try_from(v).ok())
        .or_else(|| prompt_tokens.zip(completion_tokens).map(|(p, c)| p + c));
    let cache_read_tokens = msg.cache_read_tokens.and_then(|v| u32::try_from(v).ok());
    let cache_write_tokens = msg.cache_write_tokens.and_then(|v| u32::try_from(v).ok());
    let context_window = msg.context_window.and_then(|v| u32::try_from(v).ok());
    let elapsed_ms = msg.elapsed_ms.and_then(|v| u64::try_from(v).ok());
    let provider_id = msg.provider_id.clone();

    if provider_id.is_none()
        && prompt_tokens.is_none()
        && completion_tokens.is_none()
        && total_tokens.is_none()
        && cache_read_tokens.is_none()
        && cache_write_tokens.is_none()
        && context_window.is_none()
        && elapsed_ms.is_none()
    {
        return None;
    }

    Some(RequestStats {
        provider_id,
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cache_read_tokens,
        cache_write_tokens,
        context_window,
        elapsed_ms,
    })
}

pub(super) fn map_skill(skill: crate::skill::SkillInfo) -> SkillInfoDto {
    SkillInfoDto {
        name: skill.name,
        description: skill.description,
        dir_name: skill.dir_name,
        source: skill.source,
    }
}
