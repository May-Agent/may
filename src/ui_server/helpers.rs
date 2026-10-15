//! Generic utility helpers for the UI server.
//!
//! Domain-specific helpers live in their own modules:
//! - [`super::error`] — error response builders
//! - [`super::model_resolution`] — model lookup & persistence
//! - [`super::terminal_parsing`] — native terminal-tool event parsing
//! - [`super::message_mapping`] — DB message → API message conversion
//! - [`super::attachments`] — attachment processing & uploads

use std::path::PathBuf;

use serde::Serialize;
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use crate::session::{MayHomePaths, SessionPaths};

use super::error::{bad_request_error, internal_error};
use super::state::UiServerState;

pub(super) async fn send_event<T: Serialize + Sync>(
    sender: &skyzen::responder::sse::Sender,
    kind: &str,
    data: &T,
) -> bool {
    if let Ok(payload) = serde_json::to_string(data) {
        return sender
            .send(skyzen::responder::sse::Event::data(payload).event(kind))
            .await
            .is_ok();
    }
    false
}

pub(super) async fn load_session_paths(
    state: &UiServerState,
    session_id: &str,
) -> SkyResult<SessionPaths> {
    let session = state
        .db()
        .get_session(session_id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| {
            SkyError::msg(["session ", session_id, " not found"].concat())
                .set_status(StatusCode::NOT_FOUND)
        })?;
    let home_paths = MayHomePaths::new(state.may_dir.clone());
    let paths = session.profile_id.as_deref().map_or_else(
        || home_paths.session_paths(session_id),
        |profile_id| {
            home_paths
                .profile_home(profile_id)
                .session_paths(session_id)
        },
    );
    Ok(paths)
}

pub(super) fn parse_arg_value(args: &[String], key: &str) -> Option<String> {
    for (idx, arg) in args.iter().enumerate() {
        if let Some(value) = arg.strip_prefix(&format!("{key}=")) {
            return Some(value.to_string());
        }
        if arg == key
            && let Some(value) = args.get(idx + 1)
        {
            return Some(value.clone());
        }
    }
    None
}

/// Convert `Duration::as_millis()` (u128) to u64 with saturation.
pub(super) fn elapsed_ms_saturating(started_at: &std::time::Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

const PREVIEW_MAX_CHARS: usize = 100;

pub(super) fn truncate_preview_with_limit(content: &str, max_chars: usize) -> String {
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

pub(super) fn truncate_preview(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return "New conversation started".to_string();
    }

    truncate_preview_with_limit(trimmed, PREVIEW_MAX_CHARS)
}

pub(super) fn parse_safe_relative_path(raw: &str, label: &str) -> SkyResult<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(bad_request_error([label, " is required"].concat()));
    }

    let rel = PathBuf::from(trimmed);
    if rel.is_absolute() {
        return Err(bad_request_error([label, " must be relative"].concat()));
    }
    for comp in rel.components() {
        if matches!(comp, std::path::Component::ParentDir) {
            return Err(bad_request_error(
                [label, " path traversal is not allowed"].concat(),
            ));
        }
    }
    Ok(rel)
}

/// Backoff base duration for LLM retry attempts (multiplied by attempt number).
pub(super) const RETRY_BACKOFF_MS: u64 = 400;
