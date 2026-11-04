//! Prompt suggestions: after a turn leaves the session idle, one aux run
//! against its history predicts the user's next message and broadcasts it
//! as a `session_suggestion` frame. The composer holds the text outside
//! the draft — `Tab` lifts it in for editing, a new turn supersedes it —
//! so a suggestion can never submit or persist on its own.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::daemon::state::DaemonState;

/// Longest a suggestion generation may take.
const GENERATION_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest accepted suggestion, in Unicode scalar values.
const SUGGESTION_MAX_CHARS: usize = 240;
const PROMPT: &str = include_str!("../../../templates/aux/suggestion.md");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedSuggestion {
    suggestion: Option<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SuggestionFrame<'a> {
    request_id: u64,
    session_id: &'a str,
    state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
}

/// Schedules a suggested next prompt for `session_id` after a completed
/// turn left it idle. No-op while `[suggestions] enabled` is off or the
/// session's prompt cache is cold; an aux task already in flight is
/// superseded.
pub(super) fn maybe_suggest(state: &Arc<DaemonState>, session_id: &str) {
    if !state.config.load().suggestions.enabled
        || super::aux::warm_cache(state, session_id).is_none()
    {
        return;
    }
    let task_state = Arc::clone(state);
    let task_session = session_id.to_string();
    state.aux_tasks.start(
        session_id,
        super::aux::AuxKind::Suggestion,
        |request_id| async move {
            let text = generate(&task_state, &task_session).await;
            // Only the task that still owns the session's slot publishes,
            // ordered against a cancelling turn by the slot's lock. The
            // setting is read live: a suggestion never appears under a
            // disabled setting.
            task_state.aux_tasks.commit(&task_session, request_id, || {
                let text = text
                    .as_deref()
                    .filter(|_| task_state.config.load().suggestions.enabled);
                match text {
                    Some(text) => {
                        publish(&task_state, &task_session, request_id, "ready", Some(text));
                    }
                    None => publish(&task_state, &task_session, request_id, "cleared", None),
                }
            });
        },
        &state.tokio_handle,
    );
}

/// Applies the live `[suggestions]` setting: once disabled, every
/// suggestion in flight is cancelled and its composer cleared.
pub(super) fn apply_settings(state: &Arc<DaemonState>) {
    if !state.config.load().suggestions.enabled {
        state
            .aux_tasks
            .cancel_kind(super::aux::AuxKind::Suggestion, |session_id| {
                publish(state, session_id, 0, "cleared", None);
            });
    }
}

/// Cancels `session_id`'s in-flight suggestion, if any, and broadcasts
/// `cleared`: every new turn supersedes whatever a composer may show.
pub(super) fn cancel(state: &Arc<DaemonState>, session_id: &str) {
    state.aux_tasks.cancel(session_id, || {
        publish(state, session_id, 0, "cleared", None);
    });
}

async fn generate(state: &Arc<DaemonState>, session_id: &str) -> Option<String> {
    let mut messages = match super::aux::context_messages(state, session_id).await {
        Ok(messages) => messages,
        Err(error) => {
            tracing::warn!(%session_id, %error, "suggestion: reading the agent context failed");
            return None;
        }
    };
    messages.push(aither_core::llm::Message::user(PROMPT));
    let response = match super::aux::run(state, session_id, messages, GENERATION_TIMEOUT).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%session_id, %error, "suggestion: the request failed");
            return None;
        }
    };
    super::aux::parse_strict::<GeneratedSuggestion>(&response, SUGGESTION_MAX_CHARS)?.suggestion
}

fn publish(
    state: &DaemonState,
    session_id: &str,
    request_id: u64,
    frame_state: &'static str,
    text: Option<&str>,
) {
    let frame = SuggestionFrame {
        request_id,
        session_id,
        state: frame_state,
        text,
    };
    let data: Arc<str> = serde_json::to_string(&frame)
        .expect("suggestion frame serializes")
        .into();
    state
        .turn_hub
        .publish(session_id, "session_suggestion", data);
}
