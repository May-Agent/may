//! `/recap`: one aux turn over the session's user-visible exchanges writes
//! a persisted catch-up card — a `recap` history row that never replays
//! into model context — and broadcasts it as a `session_recap` frame. A
//! request posted mid-turn queues behind the run guard and summarizes the
//! finished conversation instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aither_agent::CacheWarmth;
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
/// scalar values; the newest exchanges are kept.
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
    if !has_user_messages(&state, &session_id).await {
        return Ok(Json(RecapResponse { started: false }));
    }
    request_recap(&state, &session_id);
    Ok(Json(RecapResponse { started: true }))
}

/// Whether `session_id` holds a user message: an empty session has nothing
/// to recap.
async fn has_user_messages(state: &Arc<DaemonState>, session_id: &str) -> bool {
    crate::session::load_history_from_db(&state.db(), session_id)
        .await
        .is_ok_and(|history| {
            history
                .messages()
                .iter()
                .any(|message| message.role == MessageRole::User)
        })
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

/// The conversation a recap summarises.
struct RecapWindow<'a> {
    /// The newest user and assistant messages that fit the budget, oldest
    /// first.
    conversation: Vec<(&'a MessageRole, &'a str)>,
    /// Every user message of the session, in the window or not: what the
    /// recap covers once it is written.
    turns: usize,
}

/// Selects the recap input from `history` (oldest first): user and
/// assistant rows only, newest first until `budget` Unicode scalar values
/// are spent, restored to chronological order. Other rows never count
/// against the budget.
fn recap_window<'a>(
    history: impl DoubleEndedIterator<Item = (&'a MessageRole, &'a str)> + Clone,
    budget: usize,
) -> RecapWindow<'a> {
    let turns = history
        .clone()
        .filter(|(role, _)| **role == MessageRole::User)
        .count();
    let mut remaining = budget;
    let mut conversation = Vec::new();
    for (role, content) in history.rev() {
        if !matches!(role, MessageRole::User | MessageRole::Assistant) {
            continue;
        }
        let size = content.chars().count();
        if size > remaining {
            break;
        }
        remaining -= size;
        conversation.push((role, content));
    }
    conversation.reverse();
    RecapWindow {
        conversation,
        turns,
    }
}

/// Runs the recap's aux turn over user/assistant text only — tool calls
/// and hidden context are harness state, not conversation.
async fn generate(state: &Arc<DaemonState>, session_id: &str) -> Option<RecapRecord> {
    let history = crate::session::load_history_from_db(&state.db(), session_id)
        .await
        .ok()?;
    let window = recap_window(
        history
            .messages()
            .iter()
            .map(|message| (&message.role, message.content.as_str())),
        HISTORY_BUDGET,
    );
    if window.conversation.is_empty() {
        tracing::warn!(%session_id, "the newest message exceeds the recap budget on its own");
        return None;
    }
    let turns = window.turns;
    let mut messages: Vec<_> = window
        .conversation
        .iter()
        .map(|(role, content)| match role {
            MessageRole::User => aither_core::llm::Message::user(*content),
            _ => aither_core::llm::Message::assistant(*content),
        })
        .collect();
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

/// Per-session idle-recap timers, at most one per session.
#[derive(Default)]
pub struct RecapSchedules {
    schedules: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for RecapSchedules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecapSchedules").finish_non_exhaustive()
    }
}

impl RecapSchedules {
    fn schedules(&self) -> std::sync::MutexGuard<'_, HashMap<String, tokio::task::JoinHandle<()>>> {
        self.schedules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replaces `session_id`'s idle timer: after `idle_minutes` with no turn
    /// the task recaps the session if its history outgrew the last recap and
    /// its prompt cache is still warm.
    fn arm(&self, state: &Arc<DaemonState>, session_id: &str, warmth: CacheWarmth) {
        let idle = Duration::from_secs(state.config.load().recap.idle_minutes * 60);
        let task_state = Arc::clone(state);
        let task_session = session_id.to_string();
        let handle = state.tokio_handle.spawn(async move {
            if idle_with_warm_cache(&warmth, idle, aither_core::time::sleep).await
                && freshness(&task_state, &task_session).await
            {
                request_recap(&task_state, &task_session);
            }
            task_state.recap_schedules.schedules().remove(&task_session);
        });
        let previous = self.schedules().insert(session_id.to_string(), handle);
        if let Some(previous) = previous {
            previous.abort();
        }
    }

    /// Drops `session_id`'s idle timer, if any.
    fn unarm(&self, session_id: &str) {
        let handle = self.schedules().remove(session_id);
        if let Some(handle) = handle {
            handle.abort();
        }
    }
}

/// Waits out `idle` with `sleep`, then reports whether the cache entry is
/// still warm. Warmth is read from its own clock, which keeps counting while
/// the machine sleeps, so a timer that fires late after a lid close finds
/// the entry cold and reports `false`.
pub(super) async fn idle_with_warm_cache<S: Future<Output = ()>>(
    warmth: &CacheWarmth,
    idle: Duration,
    sleep: impl FnOnce(Duration) -> S,
) -> bool {
    sleep(idle).await;
    warmth.is_warm()
}

/// Arms the idle-recap timer after a completed turn left `session_id` idle.
/// No-op while `[recap] auto` is off or the session's prompt cache is cold.
pub(super) fn arm(state: &Arc<DaemonState>, session_id: &str) {
    if !state.config.load().recap.auto {
        return;
    }
    if let Some(warmth) = super::aux::warm_cache(state, session_id) {
        state.recap_schedules.arm(state, session_id, warmth);
    }
}

/// Drops `session_id`'s idle timer: a new message supersedes it.
pub(super) fn unarm(state: &Arc<DaemonState>, session_id: &str) {
    state.recap_schedules.unarm(session_id);
}

/// Whether `session_id` has user messages its latest recap never covered:
/// the point where an idle session owes the user a catch-up card.
async fn freshness(state: &Arc<DaemonState>, session_id: &str) -> bool {
    let Ok(history) = crate::session::load_history_from_db(&state.db(), session_id).await else {
        return false;
    };
    let mut user_turns = 0;
    let mut covered = 0;
    for message in history.messages() {
        match message.role {
            MessageRole::User => user_turns += 1,
            MessageRole::Recap => {
                covered = serde_json::from_str::<RecapRecord>(&message.content)
                    .map_or(user_turns, |record| record.turns);
            }
            _ => {}
        }
    }
    user_turns > covered
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

    fn window(history: &[(MessageRole, String)], budget: usize) -> (Vec<String>, usize) {
        let window = recap_window(
            history
                .iter()
                .map(|(role, content)| (role, content.as_str())),
            budget,
        );
        (
            window
                .conversation
                .iter()
                .map(|(_, content)| (*content).to_string())
                .collect(),
            window.turns,
        )
    }

    #[test]
    fn an_over_budget_history_keeps_the_newest_messages_in_order() {
        let history: Vec<_> = (0..6)
            .map(|i| {
                let role = if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                };
                (role, format!("m{i}-{}", "x".repeat(8)))
            })
            .collect();
        // Each message is 11 characters: three fit.
        let (conversation, turns) = window(&history, 33);
        assert_eq!(
            conversation,
            ["m3-xxxxxxxx", "m4-xxxxxxxx", "m5-xxxxxxxx"].map(str::to_string)
        );
        assert_eq!(turns, 3, "every user message counts, in the window or not");
    }

    #[test]
    fn a_large_early_tool_row_neither_counts_nor_ends_the_window() {
        let history = vec![
            (MessageRole::User, "first".to_string()),
            (MessageRole::Tool, "t".repeat(100_000)),
            (MessageRole::HiddenContext, "h".repeat(100_000)),
            (MessageRole::Assistant, "answer".to_string()),
            (MessageRole::User, "second".to_string()),
        ];
        let (conversation, turns) = window(&history, 24);
        assert_eq!(
            conversation,
            ["first", "answer", "second"].map(str::to_string)
        );
        assert_eq!(turns, 2);
    }

    async fn state_with_session(dir: &std::path::Path) -> Arc<DaemonState> {
        let state = Arc::new(
            DaemonState::for_test(dir, dir, crate::config::MayConfig::default())
                .await
                .expect("test daemon state"),
        );
        state
            .db()
            .create_session("session", Some("test"), Some("a::gpt-4o"))
            .await
            .expect("session");
        state
    }

    async fn append(state: &DaemonState, role: MessageRole, content: &str) {
        state
            .db()
            .append_message(
                "session",
                role,
                content,
                None,
                crate::db::MessageMeta::default(),
            )
            .await
            .expect("append");
    }

    #[tokio::test]
    async fn a_recap_covers_the_session_until_a_new_user_message_arrives() {
        let dir = tempfile::tempdir().expect("dir");
        let state = state_with_session(dir.path()).await;
        for i in 0..4 {
            append(&state, MessageRole::User, &format!("question {i}")).await;
            append(&state, MessageRole::Assistant, &"a".repeat(10)).await;
        }
        assert!(freshness(&state, "session").await);

        // A window of three messages, written the way `generate` writes it.
        let history = crate::session::load_history_from_db(&state.db(), "session")
            .await
            .expect("history");
        let window = recap_window(
            history
                .messages()
                .iter()
                .map(|message| (&message.role, message.content.as_str())),
            30,
        );
        assert!(window.conversation.len() < history.messages().len());
        let record = RecapRecord {
            summary: "so far".to_string(),
            next_action: None,
            turns: window.turns,
        };
        persist(&state, "session", &record).await.expect("persist");
        assert!(
            !freshness(&state, "session").await,
            "a repeated idle arm writes no second card"
        );

        append(&state, MessageRole::User, "one more").await;
        assert!(freshness(&state, "session").await);
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
