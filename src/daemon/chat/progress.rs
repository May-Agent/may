//! `/progress`: a side request asks the session's model for the work that
//! remains, grouped by what blocks it, with the counts of finished and total
//! items. Like `/btw`, it runs outside the conversation: nothing enters the
//! session's history or its next request.

use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use serde::{Deserialize, Serialize};
use skyzen::Result as SkyResult;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};

use crate::daemon::error::{internal_error, internal_error_message};
use crate::daemon::state::DaemonState;

/// Longest a progress generation may take.
const GENERATION_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest accepted item, in Unicode scalar values.
const ITEM_MAX_CHARS: usize = 300;

/// The remaining work of a session. Doubles as the schema the side model
/// fills in and as the body of the `/progress` response.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, skyzen::ToSchema,
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgressReport {
    /// How many items of the session's work are finished.
    pub done: u32,
    /// How many items the session's work has in all, finished or not. At
    /// least `done`.
    pub total: u32,
    /// Unfinished items whose work is complete and only waits to land.
    pub waiting_to_land: Vec<String>,
    /// Unfinished items blocked on something other than the user.
    pub waiting_on_other: Vec<String>,
    /// Unfinished items nobody has started.
    pub not_started: Vec<String>,
    /// Unfinished items blocked on an answer or action from the user.
    pub waiting_on_user: Vec<String>,
}

impl super::aux::AuxOutput for ProgressReport {
    fn texts(&self) -> Vec<(&str, usize)> {
        [
            &self.waiting_to_land,
            &self.waiting_on_other,
            &self.not_started,
            &self.waiting_on_user,
        ]
        .into_iter()
        .flatten()
        .map(|item| (item.as_str(), ITEM_MAX_CHARS))
        .collect()
    }
}

/// The progress instruction, written in the session's communication
/// language when one is set.
#[derive(Template)]
#[template(path = "aux/progress.md", escape = "none")]
struct ProgressPrompt {
    language: Option<aither_agent::Language>,
}

/// The side request: the messages the session's agent would send next, then
/// the instruction.
fn progress_messages(
    mut context: Vec<aither_core::llm::Message>,
    language: Option<aither_agent::Language>,
) -> Vec<aither_core::llm::Message> {
    let prompt = ProgressPrompt { language }
        .render()
        .expect("progress prompt template must render");
    context.push(aither_core::llm::Message::user(prompt));
    context
}

/// Rejects a report that finished more items than it counted.
fn validated(report: ProgressReport) -> Result<ProgressReport, super::aux::AuxError> {
    if report.done > report.total {
        return Err(super::aux::AuxError::Output(format!(
            "{} items done of {} in all",
            report.done, report.total
        )));
    }
    Ok(report)
}

/// `POST /api/sessions/{id}/progress`: the remaining work of the session,
/// generated on the side.
pub async fn progress(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ProgressReport>> {
    let session_id = super::hosts::session_id_param(&params)?;
    report(&state, &session_id).await.map(Json)
}

/// Generates the remaining-work report of `session_id`.
async fn report(state: &Arc<DaemonState>, session_id: &str) -> SkyResult<ProgressReport> {
    let context = super::aux::context_messages(state, session_id)
        .await
        .map_err(internal_error)?;
    let languages = super::aux::languages(state, session_id)
        .await
        .map_err(internal_error)?;
    let messages = progress_messages(context, languages.communication());
    super::aux::generate::<ProgressReport>(state, session_id, messages, GENERATION_TIMEOUT)
        .await
        .and_then(validated)
        .map_err(internal_error_message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::idle_compaction_tests::{SESSION_MODEL, StubModel};

    const SESSION: &str = "session";
    const REPLY: &str = "PROGRESS-SENTINEL";

    fn progress_reply(body: &str) -> Option<String> {
        body.contains("Report the work that remains").then(|| {
            serde_json::json!({
                "done": 2, "total": 5,
                "waitingToLand": [REPLY],
                "waitingOnOther": ["等待 CI 完成"],
                "notStarted": ["ドキュメントを更新する"],
                "waitingOnUser": ["릴리스 승인"],
            })
            .to_string()
        })
    }

    #[test]
    fn the_instruction_names_the_session_language() {
        let chinese: aither_agent::Language = "Chinese".parse().unwrap();
        let context = vec![aither_core::llm::Message::user("继续")];
        let messages = progress_messages(context, Some(chinese));
        assert_eq!(messages.len(), 2, "the context, then the instruction");
        let text = messages[1].content().to_string();
        assert!(text.contains("Write in Chinese"), "{text}");
    }

    #[test]
    fn more_done_than_total_is_rejected() {
        let report = ProgressReport {
            done: 3,
            total: 2,
            waiting_to_land: vec![],
            waiting_on_other: vec![],
            not_started: vec![],
            waiting_on_user: vec![],
        };
        assert!(validated(report).is_err());
    }

    /// The side request carries the instruction and the output schema, the
    /// reply parses as a report, and neither the stored conversation nor the
    /// agent's next request holds any of it.
    #[tokio::test]
    async fn progress_is_a_side_request_that_leaves_the_conversation_untouched() {
        let dir = tempfile::tempdir().expect("dir");
        let stub = StubModel::spawn_replying(SESSION_MODEL, progress_reply).await;
        let (state, _clock, _session) =
            super::super::warm_aux_tests::state_with_idle_agent_on(dir.path(), true, &stub).await;
        state
            .db()
            .create_session(SESSION, Some("test"), Some("a::gpt-4o"))
            .await
            .expect("session");
        state
            .db()
            .append_message(
                SESSION,
                crate::session::MessageRole::User,
                "把剩下的工作做完",
                None,
                crate::db::MessageMeta::default(),
            )
            .await
            .expect("append");
        let stored_before = state.db().get_messages(SESSION).await.expect("rows").len();

        let report = report(&state, SESSION).await.expect("a report");
        assert_eq!((report.done, report.total), (2, 5));
        assert_eq!(report.waiting_to_land, [REPLY]);
        assert_eq!(report.waiting_on_user, ["릴리스 승인"]);

        let request = stub
            .requests
            .lock()
            .await
            .iter()
            .find(|body| body.contains("Report the work that remains"))
            .cloned()
            .expect("the side request reached the model");
        // The context is the agent's own, not the stored rows: it carries
        // the turn the agent ran ("Say hello."), never written to the
        // database here, and the stored row's text is absent.
        assert!(request.contains("Say hello."), "the agent's turn is in it");
        assert!(!request.contains("把剩下的工作做完"));
        let turn = stub
            .requests
            .lock()
            .await
            .iter()
            .find(|body| body.contains("Say hello.") && !body.contains("Report the work"))
            .cloned()
            .expect("the agent's own turn reached the model");
        let key = |body: &str| {
            serde_json::from_str::<serde_json::Value>(body).expect("a JSON request")
                ["prompt_cache_key"]
                .clone()
        };
        assert_eq!(key(&request), key(&turn), "the same prompt-cache entry");
        for field in [
            "done",
            "total",
            "waitingToLand",
            "waitingOnOther",
            "notStarted",
            "waitingOnUser",
        ] {
            assert!(request.contains(field), "the schema lacks {field}");
        }

        assert_eq!(
            state.db().get_messages(SESSION).await.expect("rows").len(),
            stored_before,
            "no row was stored"
        );
        let history = crate::session::load_history_from_db(&state.db(), SESSION)
            .await
            .expect("history");
        let replayed = format!("{:?}", history.to_aither_messages().expect("replay"));
        assert!(!replayed.contains(REPLY) && !replayed.contains("Report the work"));

        let worker = state.parked_agents.get(SESSION).expect("parked agent");
        let guard = state
            .runtime_service
            .runs()
            .acquire(SESSION)
            .await
            .expect("acquire run");
        let handle = worker.run(
            "Say hello again.".to_string(),
            None,
            Vec::new(),
            crate::agent::RunLease::of(&guard),
        );
        while let Some(event) = handle.recv().await {
            if matches!(event, crate::agent::TuiEvent::Complete) {
                break;
            }
            assert!(
                !matches!(event, crate::agent::TuiEvent::Error(_)),
                "{event:?}"
            );
        }
        let requests = stub.requests.lock().await;
        let next = requests
            .iter()
            .rev()
            .find(|body| body.contains("Say hello again."))
            .expect("the next turn reached the model");
        assert!(!next.contains(REPLY) && !next.contains("Report the work"));
        drop(requests);
        guard
            .finish_idle(crate::session::RunOutcome::Completed)
            .await
            .expect("release run");
    }
}
