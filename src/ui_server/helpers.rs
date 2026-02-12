use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use serde::Serialize;
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use crate::agent::generate_session_title;
use crate::ask_user::AnswerValue;
use crate::config::MayConfig;
use crate::session::{ChatMessage, MessageRole, SessionId};

use super::state::UiServerState;
use super::types::{
    ApiAttachment, ApiBashExecution, ApiMessage, BashStateSnapshot,
};

pub(super) fn internal_error(err: impl std::fmt::Display) -> SkyError {
    SkyError::msg(err.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
}

pub(super) fn bad_request_error(err: impl std::fmt::Display) -> SkyError {
    SkyError::msg(err.to_string()).set_status(StatusCode::BAD_REQUEST)
}

pub(super) fn xml_escape(value: &str) -> String {
    quick_xml::escape::escape(value).into_owned()
}

pub(super) async fn send_event<T: Serialize>(
    sender: &skyzen::responder::sse::Sender,
    kind: &str,
    data: &T,
) {
    if let Ok(payload) = serde_json::to_string(data) {
        let _ = sender.send(skyzen::responder::sse::Event::data(payload).event(kind)).await;
    }
}

pub(super) async fn append_history_message(
    state: &Arc<UiServerState>,
    session_id: &SessionId,
    message: ChatMessage,
) -> anyhow::Result<()> {
    let mut manager = state.session_manager.lock().await;
    // Ensure the correct session is loaded as current
    if manager.current().map(|s| &s.id) != Some(session_id) {
        manager
            .load_session(session_id)
            .await
            .map_err(anyhow::Error::from)?;
    }
    // Single INSERT into DB + in-memory push
    manager
        .append_message(message)
        .await
        .map_err(anyhow::Error::from)
}

pub(super) fn preferred_advanced_model(config: &MayConfig) -> Option<String> {
    config.models.advanced.first().cloned()
}

/// Resolve the effective model: prefer `preferred`, then `last_model`, then first advanced model.
pub(super) fn resolve_default_model(
    config: &MayConfig,
    preferred: Option<String>,
) -> Option<String> {
    preferred
        .or_else(|| config.last_model.clone())
        .or_else(|| preferred_advanced_model(config))
}

pub(super) async fn persist_last_model(
    state: &UiServerState,
    model: &str,
) -> anyhow::Result<()> {
    let mut config = state.config.write().await;
    let strict = config.preferences.strict_model_matching;

    let normalized_existing = config
        .last_model
        .as_deref()
        .map(|m| {
            if strict {
                m.to_string()
            } else {
                crate::config::normalize_model_id_for_relaxed_match(m)
            }
        });
    let normalized_new = if strict {
        model.to_string()
    } else {
        crate::config::normalize_model_id_for_relaxed_match(model)
    };

    if normalized_existing
        .as_deref()
        .is_some_and(|existing| existing == normalized_new.as_str())
    {
        return Ok(());
    }

    config.last_model = Some(model.to_string());
    config.save().map_err(anyhow::Error::from)
}

pub(super) fn parse_arg_value(args: &[String], key: &str) -> Option<String> {
    for (idx, arg) in args.iter().enumerate() {
        if let Some(value) = arg.strip_prefix(&format!("{key}=")) {
            return Some(value.to_string());
        }
        if arg == key
            && let Some(value) = args.get(idx + 1) {
                return Some(value.clone());
            }
    }
    None
}

pub(super) fn parse_ui_answer_value(value: serde_json::Value) -> AnswerValue {
    match value {
        serde_json::Value::Null => AnswerValue::Skipped,
        serde_json::Value::String(s) => {
            if s.trim().is_empty() {
                AnswerValue::Skipped
            } else {
                AnswerValue::Custom(s)
            }
        }
        serde_json::Value::Array(items) => {
            let selected = items
                .into_iter()
                .filter_map(|item| item.as_str().map(ToString::to_string))
                .collect::<Vec<_>>();
            if selected.is_empty() {
                AnswerValue::Skipped
            } else {
                AnswerValue::Selected(selected)
            }
        }
        _ => AnswerValue::Skipped,
    }
}

/// Convert `Duration::as_millis()` (u128) to u64 with saturation.
pub(super) fn elapsed_ms_saturating(started_at: &std::time::Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

const PREVIEW_MAX_CHARS: usize = 100;

pub(super) fn truncate_preview(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return "New conversation started".to_string();
    }

    // Single-pass truncation using char_indices for O(1) slicing.
    match trimmed.char_indices().nth(PREVIEW_MAX_CHARS) {
        Some((pos, _)) => format!("{}...", &trimmed[..pos]),
        None => trimmed.to_string(),
    }
}

/// Backoff base duration for LLM retry attempts (multiplied by attempt number).
pub(super) const RETRY_BACKOFF_MS: u64 = 400;

/// Retryable error patterns for LLM requests.
const RETRYABLE_PATTERNS: &[&str] = &[
    "transport error",
    "connection error",
    "connection reset",
    "timed out",
    "timeout",
    "temporarily unavailable",
    "503",
    "502",
    "504",
];

pub(super) fn is_retryable_llm_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    RETRYABLE_PATTERNS.iter().any(|pat| lower.contains(pat))
}

// ── Tool event parsing ────────────────────────────────────────

pub(super) fn parse_tool_json_string(value: &serde_json::Value) -> Option<serde_json::Value> {
    value
        .as_object()
        .and_then(|o| {
            o.get("Ok")
                .or_else(|| o.get("ok"))
                .or_else(|| o.get("Err"))
                .or_else(|| o.get("err"))
        })
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
}

pub(super) fn parse_tool_mode_from_args(arguments: &str) -> Option<String> {
    let parsed = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    parsed
        .get("mode")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

pub(super) fn parse_bash_start(arguments: &str) -> (String, Option<String>) {
    let parsed = serde_json::from_str::<serde_json::Value>(arguments).ok();
    let script = parsed
        .as_ref()
        .and_then(|v| v.get("script"))
        .and_then(|v| v.as_str())
        .unwrap_or(arguments)
        .to_string();
    let shell_id = parsed
        .as_ref()
        .and_then(|v| v.get("shell_id"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    (script, shell_id)
}

pub(super) fn parse_open_shell_end(
    result: &serde_json::Value,
) -> (Option<String>, Option<String>) {
    let Some(parsed) = parse_tool_json_string(result) else {
        return (None, None);
    };
    let shell_id = parsed
        .get("shell_id")
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let mode = parsed
        .get("mode")
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    (shell_id, mode)
}

pub(super) fn parse_bash_end_output(result: &serde_json::Value) -> Option<String> {
    let parsed = parse_tool_json_string(result)?;
    if let Some(text) = parsed
        .get("stdout")
        .and_then(|v| v.get("content"))
        .and_then(|v| v.get("text"))
        .and_then(|v| v.as_str())
    {
        return Some(text.to_string());
    }
    Some(parsed.to_string())
}

pub(super) fn update_bash_snapshot_from_tool_event(
    snapshot: &mut BashStateSnapshot,
    tool_event: &serde_json::Value,
) {
    let name = tool_event
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let stage = tool_event
        .get("stage")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let call_id = tool_event
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    if name == "open_shell" {
        if stage == "start" {
            if let Some(mode) = tool_event
                .get("arguments")
                .and_then(|v| v.as_str())
                .and_then(parse_tool_mode_from_args)
            {
                snapshot
                    .shell_mode_by_id
                    .insert("pending".to_string(), mode);
            }
        } else if stage == "end"
            && let Some(result) = tool_event.get("result") {
                let (shell_id, mode) = parse_open_shell_end(result);
                if let (Some(shell_id), Some(mode)) = (shell_id, mode) {
                    snapshot.shell_mode_by_id.insert(shell_id, mode);
                }
            }
        return;
    }

    if name == "bash" {
        if stage == "start" {
            let arguments = tool_event
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let (script, shell_id) = parse_bash_start(arguments);
            let mode = shell_id
                .as_deref()
                .and_then(|id| snapshot.shell_mode_by_id.get(id))
                .cloned()
                .or_else(|| snapshot.shell_mode_by_id.get("pending").cloned())
                .unwrap_or_else(|| "sandboxed".to_string());
            snapshot.running_by_call_id.insert(
                call_id.clone(),
                ApiBashExecution {
                    id: call_id,
                    input: script,
                    output: None,
                    status: "running".to_string(),
                    mode,
                },
            );
        } else if stage == "end" {
            let mut exec = snapshot
                .running_by_call_id
                .remove(&call_id)
                .unwrap_or(ApiBashExecution {
                    id: call_id,
                    input: "(bash)".to_string(),
                    output: None,
                    status: "done".to_string(),
                    mode: "sandboxed".to_string(),
                });

            if let Some(result) = tool_event.get("result") {
                let output = parse_bash_end_output(result);
                let raw = result.to_string().to_ascii_lowercase();
                let failed = raw.contains("\"err\"") || raw.contains("error");
                exec.output = output;
                exec.status = if failed { "failed" } else { "done" }.to_string();
            }

            snapshot.executions.push(exec);
        }
    }
}

// ── Message mapping ────────────────────────────────────────

pub(super) fn map_history_messages(
    messages: &[crate::session::ChatMessage],
) -> Vec<ApiMessage> {
    let mut out = Vec::new();
    let mut snapshot = BashStateSnapshot::default();

    for (idx, msg) in messages.iter().enumerate() {
        match msg.role {
            MessageRole::Tool => {
                let tool_event =
                    serde_json::from_str::<serde_json::Value>(&msg.content).ok();
                if let Some(event) = tool_event.as_ref() {
                    update_bash_snapshot_from_tool_event(&mut snapshot, event);
                }
                continue;
            }
            // Interaction messages are not shown as chat messages
            MessageRole::PermissionRequest
            | MessageRole::PermissionResponse
            | MessageRole::AskUserRequest
            | MessageRole::AskUserResponse => continue,
            MessageRole::User | MessageRole::Assistant | MessageRole::System => {
                let role = match msg.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::System => "system",
                    _ => unreachable!(),
                }
                .to_string();

                let mut api_msg = ApiMessage {
                    id: format!("msg-{idx}"),
                    role,
                    content: msg.content.clone(),
                    timestamp: msg
                        .timestamp
                        .unwrap_or_else(jiff::Timestamp::now)
                        .to_string(),
                    attachments: Vec::new(),
                    thinking: None,
                    is_streaming: Some(false),
                    is_thinking: Some(false),
                    tool_event: None,
                    bash_executions: Vec::new(),
                    stats: None,
                };

                if matches!(msg.role, MessageRole::Assistant)
                    && (!snapshot.executions.is_empty()
                        || !snapshot.running_by_call_id.is_empty())
                {
                    let mut executions = snapshot.executions.clone();
                    executions.extend(snapshot.running_by_call_id.values().cloned());
                    api_msg.bash_executions = executions;
                }

                out.push(api_msg);
            }
        }
    }

    out
}

/// Convert DB message rows to API messages, with bash execution tracking.
pub(super) fn map_db_messages(
    db_messages: &[crate::db::entity::messages::Model],
) -> Vec<ApiMessage> {
    let mut out = Vec::new();
    let mut snapshot = BashStateSnapshot::default();

    for (idx, msg) in db_messages.iter().enumerate() {
        match msg.role.as_str() {
            "tool" => {
                let tool_event =
                    serde_json::from_str::<serde_json::Value>(&msg.content).ok();
                if let Some(event) = tool_event.as_ref() {
                    update_bash_snapshot_from_tool_event(&mut snapshot, event);
                }
                continue;
            }
            "permission_request" | "permission_response" | "ask_user_request"
            | "ask_user_response" => continue,
            role @ ("user" | "assistant" | "system") => {
                let timestamp = msg
                    .timestamp
                    .as_deref()
                    .and_then(|t| {
                        jiff::Timestamp::strptime("%Y-%m-%dT%H:%M:%S%.fZ", t).ok()
                    })
                    .unwrap_or_else(jiff::Timestamp::now)
                    .to_string();

                let mut api_msg = ApiMessage {
                    id: format!("msg-{idx}"),
                    role: role.to_string(),
                    content: msg.content.clone(),
                    timestamp,
                    attachments: Vec::new(),
                    thinking: None,
                    is_streaming: Some(false),
                    is_thinking: Some(false),
                    tool_event: None,
                    bash_executions: Vec::new(),
                    stats: None,
                };

                if role == "assistant"
                    && (!snapshot.executions.is_empty()
                        || !snapshot.running_by_call_id.is_empty())
                {
                    let mut executions = snapshot.executions.clone();
                    executions.extend(snapshot.running_by_call_id.values().cloned());
                    api_msg.bash_executions = executions;
                }

                out.push(api_msg);
            }
            _ => continue,
        }
    }

    out
}

pub(super) fn map_skill(skill: crate::skill::SkillInfo) -> super::types::SkillInfoDto {
    super::types::SkillInfoDto {
        name: skill.name,
        description: skill.description,
        dir_name: skill.dir_name,
        source: skill.source.to_string(),
    }
}

// ── Attachment processing ────────────────────────────────────

pub(super) const INLINE_PASTED_TEXT_THRESHOLD: usize = 1200;

pub(super) async fn build_prompt_and_attachments(
    state: &UiServerState,
    req: &super::types::ChatRequest,
) -> SkyResult<(String, Vec<url::Url>)> {
    let mut prompt = req.content.clone();
    let mut attachments = Vec::new();
    let mut xml_items = Vec::new();

    for attachment in &req.attachments {
        if let Some(data_url) = attachment.upload_data_url.as_deref().or_else(|| {
            if attachment.attachment_type == "image" && attachment.url.starts_with("data:") {
                Some(attachment.url.as_str())
            } else {
                None
            }
        })
            && let Some((saved_path, relative_path)) =
                save_upload_data_url(state, &req.session_id, attachment, data_url).await?
            {
                xml_items.push(format!(
                    "<attachment name=\"{}\" type=\"{}\" path=\"{}\" />",
                    xml_escape(&attachment.name),
                    xml_escape(&attachment.attachment_type),
                    xml_escape(&relative_path),
                ));

                if attachment.attachment_type == "image" {
                    let url = url::Url::from_file_path(saved_path).map_err(|_| {
                        SkyError::msg("Failed to map image path to URL")
                            .set_status(StatusCode::INTERNAL_SERVER_ERROR)
                    })?;
                    attachments.push(url);
                }
                continue;
            }

        if let Some(text) = &attachment.text_content {
            if text.chars().count() <= INLINE_PASTED_TEXT_THRESHOLD {
                prompt.push_str("\n\n<pasted_text>");
                prompt.push_str(&xml_escape(text));
                prompt.push_str("</pasted_text>");
            } else {
                let (saved_path, relative_path) =
                    save_large_pasted_text(state, &req.session_id, text).await?;
                xml_items.push(format!(
                    "<attachment name=\"{}\" type=\"document\" path=\"{}\" />",
                    xml_escape(&attachment.name),
                    xml_escape(&relative_path),
                ));

                if attachment.attachment_type == "image" {
                    let url = url::Url::from_file_path(saved_path).map_err(|_| {
                        SkyError::msg("Failed to map image path to URL")
                            .set_status(StatusCode::INTERNAL_SERVER_ERROR)
                    })?;
                    attachments.push(url);
                }
            }
            continue;
        }

        xml_items.push(format!(
            "<attachment name=\"{}\" type=\"{}\" />",
            xml_escape(&attachment.name),
            xml_escape(&attachment.attachment_type),
        ));
    }

    if !xml_items.is_empty() {
        prompt.push_str("\n\n<attachments>\n");
        prompt.push_str(&xml_items.join("\n"));
        prompt.push_str("\n</attachments>");
    }

    Ok((prompt, attachments))
}

async fn save_upload_data_url(
    state: &UiServerState,
    session_id: &str,
    attachment: &ApiAttachment,
    data_url: &str,
) -> SkyResult<Option<(PathBuf, String)>> {
    let Some((header, data)) = data_url.split_once(',') else {
        return Ok(None);
    };

    if !header.starts_with("data:") {
        return Ok(None);
    }

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(bad_request_error)?;

    let ext = guess_extension(header, &attachment.name);
    let base_name = sanitize_upload_stem(&attachment.name);
    let (path, relative_path) =
        allocate_upload_path(state, session_id, &base_name, &ext).await?;

    if let Some(parent) = path.parent() {
        async_fs::create_dir_all(parent)
            .await
            .map_err(internal_error)?;
    }

    async_fs::write(&path, bytes)
        .await
        .map_err(internal_error)?;

    Ok(Some((path, relative_path)))
}

async fn save_large_pasted_text(
    state: &UiServerState,
    session_id: &str,
    text: &str,
) -> SkyResult<(PathBuf, String)> {
    let filename_base = generate_pasted_text_filename(state, text).await;
    let (path, relative_path) =
        allocate_upload_path(state, session_id, &filename_base, "txt").await?;

    if let Some(parent) = path.parent() {
        async_fs::create_dir_all(parent)
            .await
            .map_err(internal_error)?;
    }

    async_fs::write(&path, text.as_bytes())
        .await
        .map_err(internal_error)?;

    Ok((path, relative_path))
}

async fn generate_pasted_text_filename(state: &UiServerState, text: &str) -> String {
    let config = state.config.read().await;
    let snippet: String = text.chars().take(600).collect();

    let generated = generate_session_title(&config, &snippet, "").await;
    let base = generated
        .as_deref()
        .map(sanitize_upload_stem)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "pasted-text".to_string());

    if base.is_empty() {
        "pasted-text".to_string()
    } else {
        base
    }
}

async fn allocate_upload_path(
    state: &UiServerState,
    session_id: &str,
    base_name: &str,
    ext: &str,
) -> SkyResult<(PathBuf, String)> {
    let uploads_dir = state
        .may_dir
        .join("sessions")
        .join(session_id)
        .join("uploads");

    async_fs::create_dir_all(&uploads_dir)
        .await
        .map_err(internal_error)?;

    let clean_ext = ext.trim_start_matches('.');
    let mut file_name = format!("{}.{}", base_name, clean_ext);
    let mut path = uploads_dir.join(&file_name);

    if async_fs::metadata(&path).await.is_ok() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        file_name = format!("{}-{}.{}", base_name, ts, clean_ext);
        path = uploads_dir.join(&file_name);
    }

    Ok((path, format!("uploads/{}", file_name)))
}

fn guess_extension(data_url_header: &str, fallback_name: &str) -> String {
    // Extract MIME type from data URL header (e.g. "data:image/png;base64,")
    let mime_str = data_url_header
        .strip_prefix("data:")
        .and_then(|s| s.split(';').next())
        .unwrap_or("");

    if !mime_str.is_empty()
        && let Some(ext) = mime_guess::get_mime_extensions_str(mime_str)
            .and_then(|exts| exts.first())
        {
            return (*ext).to_string();
        }

    Path::new(fallback_name)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("bin")
        .to_string()
}

fn sanitize_upload_stem(raw: &str) -> String {
    let stem = Path::new(raw)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(raw);

    let mut out = String::new();
    for ch in stem.chars() {
        let safe = match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => '-',
            c => c,
        };
        out.push(safe);
    }

    let cleaned = out.trim_matches([' ', '.', '-']).to_string();
    if cleaned.is_empty() {
        "upload".to_string()
    } else {
        cleaned
    }
}
