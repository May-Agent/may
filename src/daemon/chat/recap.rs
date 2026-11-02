//! `/recap`: one aux turn over the session's user-visible exchanges writes
//! a persisted catch-up card — a `recap` history row that never replays
//! into model context — and broadcasts it as a `session_recap` frame. A
//! request posted mid-turn queues behind the run guard and summarizes the
//! finished conversation instead.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use skyzen::Result as SkyResult;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};

use crate::daemon::state::DaemonState;
use crate::session::MessageRole;

/// Longest a recap generation may take.
const GENERATION_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest accepted model output, in Unicode scalar values.
const MAX_OUTPUT_CHARS: usize = 1_200;
/// Total user/assistant text the recap projection may hold, in Unicode
/// scalar values; older exchanges drop first.
const HISTORY_BUDGET: usize = 24_000;
const PROMPT: &str = include_str!("../../../templates/aux/recap.md");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecapOutput {
    summary: String,
    next_action: Option<String>,
}

/// The recap row's `content`, persisted as JSON.
///
/// Everything the card renders plus the turn count it covered, so idle
/// scheduling can tell whether the session has moved on since its last
/// recap. Clients replaying history parse the same shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecapRecord {
    /// The catch-up summary.
    pub summary: String,
    /// The suggested next step, when the recap named one.
    pub next_action: Option<String>,
    /// User messages the recap covered when it was written.
    pub turns: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecapFrame<'a> {
    request_id: u64,
    session_id: &'a str,
    state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_action: Option<&'a str>,
}

/// Response body of `POST /api/sessions/{id}/recap`.
#[derive(Debug, Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecapResponse {
    /// Whether a recap task was started.
    pub started: bool,
}

/// `POST /api/sessions/{id}/recap`: schedules a recap; the card arrives as
/// a `session_recap` frame once generation finishes.
pub async fn recap_session(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<RecapResponse>> {
    let session_id = super::hosts::session_id_param(&params)?;
    request_recap(&state, &session_id);
    Ok(Json(RecapResponse { started: true }))
}

/// Starts `session_id`'s recap on its aux slot, superseding whatever aux
/// work was in flight.
pub(super) fn request_recap(state: &Arc<DaemonState>, session_id: &str) {
    let task_state = Arc::clone(state);
    let task_session = session_id.to_string();
    state.aux_tasks.start(
        session_id,
        |request_id| async move {
            let record = generate(&task_state, &task_session).await;
            // A superseding request aborted this task before it could
            // publish; whatever survives still owns the session's slot.
            if task_state.aux_tasks.is_current(&task_session, request_id) {
                match record {
                    Some(record) => {
                        let persisted = persist(&task_state, &task_session, &record).await.is_ok();
                        if !persisted {
                            tracing::warn!("failed to persist the recap");
                        }
                        publish(
                            &task_state,
                            &task_session,
                            request_id,
                            if persisted { "ready" } else { "error" },
                            Some(&record.summary),
                            record.next_action.as_deref(),
                        );
                    }
                    None => publish(&task_state, &task_session, request_id, "error", None, None),
                }
                task_state.aux_tasks.clear(&task_session, request_id);
            }
        },
        &state.tokio_handle,
    );
}

/// Runs the recap's aux turn over user/assistant text only — tool calls
/// and hidden context are harness state, not conversation.
async fn generate(state: &Arc<DaemonState>, session_id: &str) -> Option<RecapRecord> {
    let history = crate::session::load_history_from_db(&state.db(), session_id)
        .await
        .ok()?;
    let mut budget = HISTORY_BUDGET;
    let mut messages = Vec::new();
    let mut turns = 0;
    for message in history.messages() {
        if message.content.chars().count() > budget {
            break;
        }
        let replayed = match message.role {
            MessageRole::User => aither_core::llm::Message::user(&message.content),
            MessageRole::Assistant => aither_core::llm::Message::assistant(&message.content),
            _ => continue,
        };
        turns += usize::from(message.role == MessageRole::User);
        budget -= message.content.chars().count();
        messages.push(replayed);
    }
    messages.push(aither_core::llm::Message::user(PROMPT));
    let response = super::aux::run(state, session_id, messages, GENERATION_TIMEOUT)
        .await
        .ok()?;
    let output = super::aux::parse_strict::<RecapOutput>(&response, MAX_OUTPUT_CHARS)?;
    if output.summary.trim().is_empty() {
        return None;
    }
    Some(RecapRecord {
        summary: output.summary.trim().to_string(),
        next_action: output
            .next_action
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string),
        turns,
    })
}

/// Writes the recap row; it indexes the session without replaying into
/// model context.
async fn persist(
    state: &Arc<DaemonState>,
    session_id: &str,
    record: &RecapRecord,
) -> Result<(), crate::db::ChatDbError> {
    let content = serde_json::to_string(record).expect("recap record serializes");
    state
        .db()
        .append_message(
            session_id,
            MessageRole::Recap,
            &content,
            None,
            crate::db::MessageMeta::default(),
        )
        .await
        .map(|_| ())
}

fn publish(
    state: &DaemonState,
    session_id: &str,
    request_id: u64,
    frame_state: &'static str,
    summary: Option<&str>,
    next_action: Option<&str>,
) {
    let frame = RecapFrame {
        request_id,
        session_id,
        state: frame_state,
        summary,
        next_action,
    };
    let data: Arc<str> = serde_json::to_string(&frame)
        .expect("recap frame serializes")
        .into();
    state.turn_hub.publish(session_id, "session_recap", data);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recap_record_round_trips_through_json() {
        let record = RecapRecord {
            summary: "landed the merge".to_string(),
            next_action: None,
            turns: 4,
        };
        let parsed: RecapRecord =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
        assert_eq!(parsed.summary, "landed the merge");
        assert_eq!(parsed.turns, 4);
        assert!(parsed.next_action.is_none());
    }

    #[test]
    fn recap_output_rejects_unexpected_fields() {
        assert!(
            super::super::aux::parse_strict::<RecapOutput>(
                r#"{"summary":"x","next_action":null,"extra":1}"#,
                MAX_OUTPUT_CHARS,
            )
            .is_none()
        );
    }
}
