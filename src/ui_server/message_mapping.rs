//! DB message → API message mapping.

use crate::session::history::MessageRole;

use super::terminal_parsing::update_terminal_snapshot_from_tool_event;
use super::types::{
    ApiMessage, RequestStats, SkillInfoDto, TerminalStateSnapshot, ToolEventRecord,
};

/// Convert DB message rows to API messages, with native terminal execution tracking.
pub(super) fn map_db_messages(
    db_messages: &[crate::db::entity::messages::Model],
) -> Vec<ApiMessage> {
    let mut out = Vec::new();
    let mut snapshot = TerminalStateSnapshot::default();

    for (idx, msg) in db_messages.iter().enumerate() {
        let Ok(role) = MessageRole::from_str(&msg.role) else {
            continue;
        };
        match role {
            MessageRole::Tool => {
                if let Ok(record) = serde_json::from_str::<ToolEventRecord>(&msg.content) {
                    update_terminal_snapshot_from_tool_event(&mut snapshot, &record);
                }
                continue;
            }
            MessageRole::PermissionRequest
            | MessageRole::PermissionResponse
            | MessageRole::WorkspaceRequest
            | MessageRole::WorkspaceResponse
            | MessageRole::AskUserRequest
            | MessageRole::AskUserResponse
            | MessageRole::HiddenContext => continue,
            MessageRole::User | MessageRole::Assistant | MessageRole::System => {
                let timestamp = msg
                    .timestamp
                    .as_deref()
                    .and_then(|t| jiff::Timestamp::strptime("%Y-%m-%dT%H:%M:%S%.fZ", t).ok())
                    .unwrap_or_else(jiff::Timestamp::now)
                    .to_string();

                let mut api_msg = ApiMessage {
                    id: format!("msg-{idx}"),
                    role: role.as_str().to_string(),
                    content: msg.content.clone(),
                    timestamp,
                    attachments: Vec::new(),
                    thinking: None,
                    is_streaming: Some(false),
                    is_thinking: Some(false),
                    tool_event: None,
                    terminal_executions: Vec::new(),
                    stats: map_db_message_stats(msg),
                };

                if role == MessageRole::Assistant
                    && (!snapshot.executions.is_empty() || !snapshot.running_by_call_id.is_empty())
                {
                    let mut executions = snapshot.executions.clone();
                    executions.extend(snapshot.running_by_call_id.values().cloned());
                    api_msg.terminal_executions = executions;
                }

                out.push(api_msg);
            }
        }
    }

    out
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
