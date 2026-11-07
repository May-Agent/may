//! `/recap`: one aux turn over the session's user-visible exchanges writes
//! a persisted catch-up card — a `recap` history row that never replays
//! into model context — and broadcasts it as a `session_recap` frame. A
//! request posted mid-turn is refused with a conflict: the running turn holds
//! the session's agent.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aither_agent::CacheWarmth;
use askama::Template;
use serde::{Deserialize, Serialize};
use skyzen::Result as SkyResult;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};

use super::aux::AuxKind;
use crate::daemon::state::DaemonState;
use crate::session::MessageRole;

/// Longest a recap generation may take.
const GENERATION_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest accepted summary, in Unicode scalar values.
const SUMMARY_MAX_CHARS: usize = 700;
/// Longest accepted next action, in Unicode scalar values.
const NEXT_ACTION_MAX_CHARS: usize = 200;
/// Total user/assistant text the recap projection may hold, in Unicode
/// scalar values; the newest exchanges are kept.
pub(super) const HISTORY_BUDGET: usize = 24_000;

/// A catch-up note for a user returning to the session.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RecapOutput {
    /// Plain text saying where the work stands, at most 700 characters.
    summary: String,
    /// The agreed next step, an unanswered question, or the obvious blocker
    /// resolution, at most 200 characters. Null when none exists.
    next_action: Option<String>,
}

impl super::aux::AuxOutput for RecapOutput {
    fn texts(&self) -> Vec<(&str, usize)> {
        let mut texts = vec![(self.summary.as_str(), SUMMARY_MAX_CHARS)];
        texts.extend(
            self.next_action
                .as_deref()
                .map(|text| (text, NEXT_ACTION_MAX_CHARS)),
        );
        texts
    }
}

impl RecapOutput {
    /// The record this output writes for a window covering `turns` user
    /// messages; `None` when the summary is blank.
    fn into_record(self, turns: usize) -> Option<RecapRecord> {
        if self.summary.trim().is_empty() {
            return None;
        }
        Some(RecapRecord {
            summary: self.summary.trim().to_string(),
            next_action: self
                .next_action
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string),
            turns,
        })
    }
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
    begin_recap(&state, &session_id).await.map(Json)
}

/// Starts a manual recap, or refuses it while a turn runs: the turn holds
/// the session's agent, so there is nothing to summarise with until it ends.
async fn begin_recap(state: &Arc<DaemonState>, session_id: &str) -> SkyResult<RecapResponse> {
    if state.runtime_service.runs().is_held(session_id) {
        return Err(skyzen::Error::msg(
            "a turn is running in this session; run /recap when it finishes",
        )
        .set_status(skyzen::StatusCode::CONFLICT));
    }
    if !has_user_messages(state, session_id).await {
        return Ok(RecapResponse { started: false });
    }
    request_recap(state, session_id, AuxKind::ManualRecap);
    Ok(RecapResponse { started: true })
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
/// work was in flight. An automatic recap is governed by `[recap] auto`,
/// read live.
pub(super) fn request_recap(state: &Arc<DaemonState>, session_id: &str, kind: AuxKind) {
    let task_state = Arc::clone(state);
    let task_session = session_id.to_string();
    state.aux_tasks.start(
        session_id,
        kind,
        |request_id| async move {
            let record = generate(&task_state, &task_session).await;
            // The commit is atomic with a cancelling turn. A recap card is
            // history, so once the task owns the slot, writing the card and
            // announcing it run as one unit that no later cancellation can
            // split.
            let allowed = kind != AuxKind::AutoRecap || task_state.config.load().recap.auto;
            task_state
                .aux_tasks
                .commit(&task_session, request_id, || match record {
                    // Disabled while generating: nothing is written or sent.
                    _ if !allowed => {}
                    Some(record) => {
                        let unit_state = Arc::clone(&task_state);
                        let unit_session = task_session.clone();
                        task_state.tokio_handle.spawn(async move {
                            let persisted =
                                persist(&unit_state, &unit_session, &record).await.is_ok();
                            if !persisted {
                                tracing::warn!("failed to persist the recap");
                            }
                            publish(
                                &unit_state,
                                &unit_session,
                                request_id,
                                if persisted { "ready" } else { "error" },
                                Some(&record.summary),
                                record.next_action.as_deref(),
                            );
                        });
                    }
                    None => publish(&task_state, &task_session, request_id, "error", None, None),
                });
        },
        &state.tokio_handle,
    );
}

/// The conversation a recap summarises.
pub(super) struct RecapWindow<'a> {
    /// The newest user and assistant messages that fit the budget, oldest
    /// first.
    pub(super) conversation: Vec<(&'a MessageRole, &'a str)>,
    /// Every user message of the session, in the window or not: what the
    /// recap covers once it is written.
    turns: usize,
}

/// Selects the recap input from `history` (oldest first): user and
/// assistant rows only, newest first until `budget` Unicode scalar values
/// are spent, restored to chronological order. Other rows never count
/// against the budget.
pub(super) fn recap_window<'a>(
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

/// The recap instruction, written in the session's communication language
/// when one is set.
#[derive(Template)]
#[template(path = "aux/recap.md", escape = "none")]
struct RecapPrompt {
    language: Option<aither_agent::Language>,
}

/// The aux request: the windowed conversation, then the recap instruction.
fn recap_messages(
    conversation: &[(&MessageRole, &str)],
    language: Option<aither_agent::Language>,
) -> Vec<aither_core::llm::Message> {
    let prompt = RecapPrompt { language }
        .render()
        .expect("recap prompt template must render");
    let mut messages: Vec<_> = conversation
        .iter()
        .map(|(role, content)| match role {
            MessageRole::User => aither_core::llm::Message::user(*content),
            _ => aither_core::llm::Message::assistant(*content),
        })
        .collect();
    messages.push(aither_core::llm::Message::user(prompt));
    messages
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
    let languages = super::aux::languages(state, session_id).await.ok()?;
    let messages = recap_messages(&window.conversation, languages.communication());
    super::aux::generate::<RecapOutput>(state, session_id, messages, GENERATION_TIMEOUT)
        .await
        .ok()?
        .into_record(turns)
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
                && task_state.config.load().recap.auto
                && freshness(&task_state, &task_session).await
            {
                request_recap(&task_state, &task_session, AuxKind::AutoRecap);
            }
            task_state.recap_schedules.schedules().remove(&task_session);
        });
        let previous = self.schedules().insert(session_id.to_string(), handle);
        if let Some(previous) = previous {
            previous.abort();
        }
    }

    /// Whether `session_id` has an idle timer pending.
    #[cfg(test)]
    pub(super) fn is_armed(&self, session_id: &str) -> bool {
        self.schedules().contains_key(session_id)
    }

    /// Drops every idle timer.
    fn unarm_all(&self) {
        let handles: Vec<_> = self.schedules().drain().map(|(_, handle)| handle).collect();
        for handle in handles {
            handle.abort();
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

/// Applies the live `[recap]` setting: once `auto` is off, every armed idle
/// timer and every automatic generation in flight is dropped.
pub(super) fn apply_settings(state: &Arc<DaemonState>) {
    if !state.config.load().recap.auto {
        state.recap_schedules.unarm_all();
        state.aux_tasks.cancel_kind(AuxKind::AutoRecap, |_| {});
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

    fn last_message(language: Option<aither_agent::Language>) -> String {
        let conversation = [
            (&MessageRole::User, "ship it"),
            (&MessageRole::Assistant, "shipped"),
        ];
        let messages = recap_messages(&conversation, language);
        assert_eq!(messages.len(), 3, "the conversation, then the instruction");
        messages[2].content().to_string()
    }

    #[test]
    fn the_recap_request_names_the_session_communication_language() {
        let chinese: aither_agent::Language = "Chinese".parse().unwrap();
        let prompt = last_message(Some(chinese));
        assert!(prompt.contains("Write in Chinese."), "{prompt}");
        assert!(!prompt.contains("language of the user's messages"));
    }

    #[test]
    fn an_unset_language_leaves_the_recap_in_the_users_language() {
        let prompt = last_message(None);
        assert!(prompt.contains("Write in the language of the user's messages."));
        assert!(!prompt.contains("{%"), "template tags are rendered");
    }

    #[test]
    fn the_recap_prompt_asks_for_plain_prose_without_asides() {
        let prompt = last_message(None);
        assert!(prompt.contains("State each open caveat once"));
        assert!(prompt.contains("full-width （）"));
        assert!(!prompt.contains("above all else"));
    }

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
            DaemonState::for_test(dir, crate::config::MayConfig::default())
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

    #[tokio::test]
    async fn a_recap_during_a_running_turn_is_refused_as_busy() {
        let dir = tempfile::tempdir().expect("dir");
        let state = state_with_session(dir.path()).await;
        append(&state, MessageRole::User, "question").await;

        let guard = state
            .runtime_service
            .runs()
            .acquire("session")
            .await
            .expect("acquire run");
        let refused = begin_recap(&state, "session")
            .await
            .expect_err("a running turn refuses the recap");
        assert_eq!(refused.status(), skyzen::StatusCode::CONFLICT);
        assert!(
            !state.aux_tasks.is_current("session", 0),
            "no generation started"
        );
        guard
            .finish_idle(crate::session::RunOutcome::Completed)
            .await
            .expect("release run");
    }

    async fn recap_from(reply: &'static str) -> Result<RecapOutput, super::super::aux::AuxError> {
        let slot = crate::agent::ModelSlot::new(
            super::super::aux::test_support::Replying(reply),
            aither_core::llm::model::Parameters::default(),
        );
        super::super::aux::generate_with::<_, RecapOutput>(
            &slot,
            recap_messages(&[(&MessageRole::User, "ship it")], None),
            GENERATION_TIMEOUT,
        )
        .await
    }

    #[tokio::test]
    async fn a_fenced_reply_yields_a_recap() {
        let reply = "```json\n{\"summary\":\"Merged.\",\"next_action\":null}\n```";
        let record = recap_from(reply)
            .await
            .expect("fenced JSON is recovered")
            .into_record(1)
            .expect("record");
        assert_eq!(record.summary, "Merged.");
        assert!(record.next_action.is_none());
    }

    #[tokio::test]
    async fn an_over_limit_summary_is_rejected() {
        let reply: &'static str = Box::leak(
            format!(
                "{{\"summary\":\"{}\",\"next_action\":null}}",
                "x".repeat(SUMMARY_MAX_CHARS + 1)
            )
            .into_boxed_str(),
        );
        let error = recap_from(reply).await.expect_err("over the limit");
        assert!(
            matches!(error, super::super::aux::AuxError::Output(_)),
            "{error}"
        );
    }

    #[test]
    fn the_recap_prompt_leaves_the_output_format_to_the_schema() {
        assert!(!last_message(None).contains("JSON"));
    }
}
