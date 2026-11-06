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

/// The user's predicted next message.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct GeneratedSuggestion {
    /// The message itself on one line, at most 240 characters. Null only
    /// when no grounded next move exists.
    suggestion: Option<String>,
}

impl super::aux::AuxOutput for GeneratedSuggestion {
    fn texts(&self) -> Vec<(&str, usize)> {
        self.suggestion
            .as_deref()
            .map(|text| (text, SUGGESTION_MAX_CHARS))
            .into_iter()
            .collect()
    }
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

/// Whether suggestions may be generated: the setting is on and the user is
/// awake, since nobody reads them while asleep.
pub(super) fn suggestions_allowed(state: &DaemonState) -> bool {
    state.config.load().suggestions.enabled && !state.sleep.is_asleep()
}

/// Schedules a suggested next prompt for `session_id` after a completed
/// turn left it idle. No-op while `[suggestions] enabled` is off or the
/// session's prompt cache is cold; an aux task already in flight is
/// superseded.
pub(super) fn maybe_suggest(state: &Arc<DaemonState>, session_id: &str) {
    if !suggestions_allowed(state) || super::aux::warm_cache(state, session_id).is_none() {
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
                let text = text.as_deref().filter(|_| suggestions_allowed(&task_state));
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
    if !suggestions_allowed(state) {
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
    match super::aux::generate::<GeneratedSuggestion>(
        state,
        session_id,
        messages,
        GENERATION_TIMEOUT,
    )
    .await
    {
        Ok(generated) => generated.suggestion,
        Err(error) => {
            tracing::warn!(%session_id, %error, "suggestion: the request failed");
            None
        }
    }
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

#[cfg(test)]
mod tests {
    use super::super::aux::{AuxError, generate_with, test_support::Replying};
    use super::*;

    async fn suggestion_from(reply: &'static str) -> Result<GeneratedSuggestion, AuxError> {
        let slot = crate::agent::ModelSlot::new(
            Replying(reply),
            aither_core::llm::model::Parameters::default(),
        );
        generate_with(
            &slot,
            vec![aither_core::llm::Message::user(PROMPT)],
            GENERATION_TIMEOUT,
        )
        .await
    }

    #[tokio::test]
    async fn a_fenced_reply_yields_a_suggestion() {
        let generated = suggestion_from("```json\n{\"suggestion\":\"run the tests\"}\n```")
            .await
            .expect("fenced JSON is recovered");
        assert_eq!(generated.suggestion.as_deref(), Some("run the tests"));
    }

    #[tokio::test]
    async fn an_over_limit_suggestion_is_rejected() {
        let reply: &'static str = Box::leak(
            format!(
                "{{\"suggestion\":\"{}\"}}",
                "x".repeat(SUGGESTION_MAX_CHARS + 1)
            )
            .into_boxed_str(),
        );
        assert!(matches!(
            suggestion_from(reply).await,
            Err(AuxError::Output(_))
        ));
    }
}
