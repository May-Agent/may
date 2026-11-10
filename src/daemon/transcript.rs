//! `GET /api/sessions/{id}/transcript` — the session's stored rows folded
//! into stable, paginated transcript entries (tool calls merged by call id).

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use skyzen::extract::Query;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Error as SkyError, Result as SkyResult};

use super::error::{bad_request_error, internal_error};
use super::state::DaemonState;
use crate::session::history::{ChatMessage, ChatMessageStats, MessageRole, ToolEventRecord};
use crate::session::runtime::PersistedAttachment;

#[derive(Debug, Deserialize, skyzen::ToSchema)]
pub(super) struct TranscriptQuery {
    limit: Option<u32>,
    before: Option<String>,
}

#[derive(Debug, Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct TranscriptPage {
    entries: Vec<TranscriptEntry>,
    has_more: bool,
    cursor: String,
}

/// One foldable unit of the conversation; `id` is the stored row id of the
/// entry's first row.
#[derive(Debug, Serialize, Clone, skyzen::ToSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(super) enum TranscriptEntry {
    User {
        id: String,
        timestamp: Option<String>,
        text: String,
        #[schema(value_type = Vec<Object>)]
        attachments: Vec<PersistedAttachment>,
    },
    Assistant {
        id: String,
        timestamp: Option<String>,
        text: String,
        thinking: Option<String>,
        #[schema(value_type = Object)]
        translation: Option<crate::session::history::MessageTranslation>,
        #[schema(value_type = Object)]
        stats: Option<ChatMessageStats>,
    },
    ToolCall {
        id: String,
        timestamp: Option<String>,
        call_id: String,
        name: String,
        arguments: String,
        status: &'static str,
        #[schema(value_type = Object)]
        result: Option<aither_core::llm::ToolResult>,
        /// How the call's script was read when it started; absent for a row
        /// stored without one.
        #[schema(value_type = Option<Object>)]
        reading: Option<crate::command_reading::CommandReading>,
    },
    InteractionRequest {
        id: String,
        timestamp: Option<String>,
        interaction: &'static str,
        content: String,
    },
    InteractionResponse {
        id: String,
        timestamp: Option<String>,
        interaction: &'static str,
        content: String,
    },
    HiddenContext {
        id: String,
        timestamp: Option<String>,
        text: String,
    },
    Recap {
        id: String,
        timestamp: Option<String>,
        content: String,
    },
    System {
        id: String,
        timestamp: Option<String>,
        text: String,
    },
}

impl TranscriptEntry {
    fn id(&self) -> &str {
        match self {
            Self::User { id, .. }
            | Self::Assistant { id, .. }
            | Self::ToolCall { id, .. }
            | Self::InteractionRequest { id, .. }
            | Self::InteractionResponse { id, .. }
            | Self::HiddenContext { id, .. }
            | Self::Recap { id, .. }
            | Self::System { id, .. } => id,
        }
    }
    const fn is_user(&self) -> bool {
        matches!(self, Self::User { .. })
    }
}

const fn interaction_kind(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::AskUserRequest | MessageRole::AskUserResponse => "askUser",
        MessageRole::PermissionRequest | MessageRole::PermissionResponse => "permission",
        MessageRole::WorkspaceRequest | MessageRole::WorkspaceResponse => "workspace",
        _ => "unknown",
    }
}

pub(super) fn build_entries(rows: &[ChatMessage]) -> Vec<TranscriptEntry> {
    let mut entries = Vec::new();
    let mut open_tools: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let id = row.id.map_or_else(String::new, |i| i.to_string());
        let ts = row.timestamp.map(|t| t.to_string());
        match row.role {
            MessageRole::User => entries.push(TranscriptEntry::User {
                id,
                timestamp: ts,
                text: row.content.clone(),
                attachments: row.attachments.clone(),
            }),
            MessageRole::Assistant => entries.push(TranscriptEntry::Assistant {
                id,
                timestamp: ts,
                text: row.content.clone(),
                thinking: row.thinking.clone(),
                translation: row.translation.clone(),
                stats: row.stats.clone(),
            }),
            MessageRole::Tool => {
                let Ok(record) = serde_json::from_str::<ToolEventRecord>(&row.content) else {
                    continue;
                };
                match record {
                    ToolEventRecord::Start {
                        id: call_id,
                        name,
                        arguments,
                        reading,
                    } => {
                        open_tools.insert(call_id.clone(), entries.len());
                        entries.push(TranscriptEntry::ToolCall {
                            id,
                            timestamp: ts,
                            call_id,
                            name,
                            arguments,
                            status: "running",
                            result: None,
                            reading,
                        });
                    }
                    ToolEventRecord::End {
                        id: call_id,
                        result,
                        ..
                    } => {
                        if let Some(&i) = open_tools.get(&call_id)
                            && let TranscriptEntry::ToolCall {
                                status,
                                result: slot,
                                ..
                            } = &mut entries[i]
                        {
                            *status = "done";
                            *slot = Some(result);
                        }
                    }
                    ToolEventRecord::Stopped { id: call_id, .. } => {
                        if let Some(&i) = open_tools.get(&call_id)
                            && let TranscriptEntry::ToolCall { status, .. } = &mut entries[i]
                        {
                            *status = "stopped";
                        }
                    }
                }
            }
            MessageRole::HiddenContext => entries.push(TranscriptEntry::HiddenContext {
                id,
                timestamp: ts,
                text: row.content.clone(),
            }),
            MessageRole::Recap => entries.push(TranscriptEntry::Recap {
                id,
                timestamp: ts,
                content: row.content.clone(),
            }),
            MessageRole::System => entries.push(TranscriptEntry::System {
                id,
                timestamp: ts,
                text: row.content.clone(),
            }),
            ref role @ (MessageRole::AskUserRequest
            | MessageRole::PermissionRequest
            | MessageRole::WorkspaceRequest) => entries.push(TranscriptEntry::InteractionRequest {
                id,
                timestamp: ts,
                interaction: interaction_kind(role),
                content: row.content.clone(),
            }),
            ref role @ (MessageRole::AskUserResponse
            | MessageRole::PermissionResponse
            | MessageRole::WorkspaceResponse) => {
                entries.push(TranscriptEntry::InteractionResponse {
                    id,
                    timestamp: ts,
                    interaction: interaction_kind(role),
                    content: row.content.clone(),
                });
            }
        }
    }
    entries
}

pub(super) async fn get_transcript(
    params: Params,
    Query(query): Query<TranscriptQuery>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<TranscriptPage>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let cursor = state.session_cursor(&id);
    let history = crate::session::load_history_from_db(&state.db(), &id)
        .await
        .map_err(internal_error)?;
    let entries = build_entries(history.messages());
    let end = match &query.before {
        Some(before) => entries
            .iter()
            .position(|e| e.id() == before)
            .ok_or_else(|| bad_request_error("unknown `before` entry id"))?,
        None => entries.len(),
    };
    let limit = query.limit.map_or(end, |l| (l as usize).min(end));
    let start = end - limit;
    Ok(Json(TranscriptPage {
        has_more: start > 0,
        entries: entries[start..end].to_vec(),
        cursor,
    }))
}

/// Resolve `entryId` to the row index `index` would have been, walking back
/// over directly preceding `hidden_context` rows — the TUI's rewind walk.
pub(super) fn entry_cut_index(
    rows: &[crate::db::entity::messages::Model],
    entries: &[TranscriptEntry],
    entry_id: &str,
) -> Result<usize, SkyError> {
    let entry = entries
        .iter()
        .find(|e| e.id() == entry_id)
        .ok_or_else(|| bad_request_error("unknown `entryId`"))?;
    if !entry.is_user() {
        return Err(bad_request_error("entryId must name a user entry"));
    }
    let row_idx = rows
        .iter()
        .position(|r| r.id.to_string() == entry.id())
        .ok_or_else(|| bad_request_error("unknown `entryId`"))?;
    let mut cut = row_idx;
    while cut > 0 && rows[cut - 1].role == "hidden_context" {
        cut -= 1;
    }
    Ok(cut)
}
