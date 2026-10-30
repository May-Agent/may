//! Each session's agent between turns.
//!
//! After a turn, the session's agent stays in the daemon as an idle
//! [`AgentWorker`]: it compacts the conversation when the provider's cache
//! is about to expire, and answers `/compact`, `/context` and `/btw` without
//! rebuilding. A new turn evicts it first, so an idle agent never acts on a
//! conversation a turn has moved past.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use skyzen::Result as SkyResult;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};

use crate::MayAgent;
use crate::agent::{AgentWorker, TuiEvent, spawn_worker};
use crate::ui_server::error::{
    bad_request_error, internal_error, internal_error_message, json_ok_response,
};
use crate::ui_server::state::UiServerState;

/// Idle agents of the sessions the daemon ran.
#[derive(Default)]
pub(crate) struct ParkedAgents {
    workers: Mutex<HashMap<String, Arc<AgentWorker>>>,
}

impl std::fmt::Debug for ParkedAgents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParkedAgents").finish_non_exhaustive()
    }
}

impl ParkedAgents {
    fn workers(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<AgentWorker>>> {
        self.workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Keeps `agent` as `session_id`'s idle agent.
    pub(crate) fn park(&self, state: &UiServerState, session_id: &str, agent: MayAgent) {
        let worker = spawn_worker(
            executor_core::DefaultExecutor,
            agent,
            state.db(),
            state.runtime_service.runs().clone(),
        );
        if let Some(previous) = self
            .workers()
            .insert(session_id.to_string(), Arc::new(worker))
        {
            previous.shutdown();
        }
    }

    /// Stops `session_id`'s idle agent, if any.
    pub(crate) fn evict(&self, session_id: &str) {
        if let Some(worker) = self.workers().remove(session_id) {
            worker.shutdown();
        }
    }

    fn get(&self, session_id: &str) -> Option<Arc<AgentWorker>> {
        self.workers().get(session_id).cloned()
    }
}

fn session_id_param(params: &Params) -> SkyResult<String> {
    Ok(params.get("id").map_err(bad_request_error)?.to_string())
}

/// The session's idle agent, built from its persisted context when the
/// daemon has none (it restarted, or the session never ran here).
async fn idle_agent(state: &Arc<UiServerState>, session_id: &str) -> SkyResult<Arc<AgentWorker>> {
    if let Some(worker) = state.parked_agents.get(session_id) {
        return Ok(worker);
    }
    let guard = state
        .runtime_service
        .runs()
        .acquire(session_id)
        .await
        .map_err(super::acquire_refusal)?;
    let request = super::ChatRequest {
        session_id: session_id.to_string(),
        content: String::new(),
        attachments: Vec::new(),
        hidden_context_xml: None,
        delivery_mode: crate::session::PendingUserMessageMode::default(),
    };
    let prepared = super::prepare_agent_for_request(state, &request, guard.fence(), false, true, 0)
        .await
        .map_err(internal_error);
    if let Err(error) = guard
        .finish_idle(crate::session::RunOutcome::Completed)
        .await
    {
        tracing::warn!(%session_id, %error, "failed to release the session after building its agent");
    }
    state.parked_agents.park(state, session_id, prepared?.agent);
    state
        .parked_agents
        .get(session_id)
        .ok_or_else(|| internal_error_message("the idle agent stopped at once"))
}

/// Waits for the worker's answer to one command.
async fn answer(handle: crate::agent::AgentHandle) -> Option<TuiEvent> {
    while let Some(event) = handle.recv().await {
        if !matches!(event, TuiEvent::Complete) {
            return Some(event);
        }
    }
    None
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompactRequest {
    /// What the summary should keep in focus.
    #[serde(default)]
    focus: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
/// Answer of `POST /api/sessions/{id}/compact`.
pub struct CompactResponse {
    /// Whether anything was compacted.
    pub compacted: bool,
    /// Messages folded into the summary.
    pub messages_compacted: usize,
    /// Messages kept verbatim.
    pub messages_remaining: usize,
    /// The summary.
    pub summary: Option<String>,
}

/// `POST /api/sessions/{id}/compact`.
pub(crate) async fn compact_session(
    params: Params,
    Json(req): Json<CompactRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<CompactResponse>> {
    let session_id = session_id_param(&params)?;
    let worker = idle_agent(&state, &session_id).await?;
    match answer(worker.compact(req.focus)).await {
        Some(TuiEvent::Compacted(Ok(result))) => Ok(Json(CompactResponse {
            compacted: result.is_some(),
            messages_compacted: result.as_ref().map_or(0, |r| r.messages_compacted),
            messages_remaining: result.as_ref().map_or(0, |r| r.messages_remaining),
            summary: result.map(|r| r.summary),
        })),
        Some(TuiEvent::Compacted(Err(error))) => Err(internal_error(error)),
        Some(TuiEvent::Error(message)) => {
            Err(skyzen::Error::msg(message).set_status(skyzen::StatusCode::CONFLICT))
        }
        _ => Ok(Json(CompactResponse {
            compacted: false,
            messages_compacted: 0,
            messages_remaining: 0,
            summary: None,
        })),
    }
}

/// `GET /api/sessions/{id}/context-report`: a [`crate::agent::ContextReport`].
pub(crate) async fn context_report(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<skyzen::Response> {
    let session_id = session_id_param(&params)?;
    let worker = idle_agent(&state, &session_id).await?;
    match answer(worker.context_report()).await {
        Some(TuiEvent::ContextReport(Ok(report))) => json_ok_response(&report),
        Some(TuiEvent::ContextReport(Err(error))) => Err(internal_error(error)),
        _ => Err(internal_error_message("the agent gave no context report")),
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
/// Answer of `POST /api/sessions/{id}/context-dump`.
pub struct ContextDumpResponse {
    /// Where the dump was written.
    pub path: String,
    /// Messages in the dump.
    pub message_count: usize,
}

/// `POST /api/sessions/{id}/context-dump`: writes the assembled context to
/// the session's `outputs/context-dump.md`.
pub(crate) async fn context_dump(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ContextDumpResponse>> {
    let session_id = session_id_param(&params)?;
    let worker = idle_agent(&state, &session_id).await?;
    let dump = match answer(worker.context_dump()).await {
        Some(TuiEvent::ContextDump(Ok(dump))) => dump,
        Some(TuiEvent::ContextDump(Err(error))) => return Err(internal_error(error)),
        _ => return Err(internal_error_message("the agent gave no context dump")),
    };
    let text = dump.render();
    let paths = crate::ui_server::helpers::load_session_paths(&state, &session_id).await?;
    let dir = paths.outputs_dir();
    async_fs::create_dir_all(&dir)
        .await
        .map_err(internal_error)?;
    let path = dir.join("context-dump.md");
    async_fs::write(&path, text).await.map_err(internal_error)?;
    Ok(Json(ContextDumpResponse {
        path: path.display().to_string(),
        message_count: dump.messages.len(),
    }))
}

/// One message of a `/btw` request.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BtwMessage {
    /// `system`, `user` or `assistant`.
    pub role: String,
    /// Message text.
    pub content: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
/// Body of `POST /api/sessions/{id}/btw`.
pub struct BtwRequest {
    /// The side question with the context it is asked in, oldest first.
    pub messages: Vec<BtwMessage>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
/// Answer of `POST /api/sessions/{id}/btw`.
pub struct BtwResponse {
    /// The answer.
    pub answer: String,
}

/// `POST /api/sessions/{id}/btw`: answers a side question with the
/// session's model, outside the conversation.
pub(crate) async fn btw(
    params: Params,
    Json(req): Json<BtwRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<BtwResponse>> {
    use aither_core::LanguageModel as _;
    use aither_core::llm::Message;
    use futures_lite::StreamExt as _;

    let session_id = session_id_param(&params)?;
    let worker = idle_agent(&state, &session_id).await?;
    let messages = req
        .messages
        .into_iter()
        .map(|message| match message.role.as_str() {
            "system" => Message::system(message.content),
            "assistant" => Message::assistant(message.content),
            _ => Message::user(message.content),
        })
        .collect::<Vec<_>>();
    let llm = worker.btw_llm().clone();
    let mut answer = String::new();
    let mut stream = std::pin::pin!(llm.respond(aither_core::llm::LLMRequest::new(messages)));
    while let Some(event) = stream.next().await {
        match event {
            Ok(aither_core::llm::Event::Text(text)) => answer.push_str(&text),
            Ok(_) => {}
            Err(error) => return Err(internal_error(error)),
        }
    }
    Ok(Json(BtwResponse { answer }))
}

/// `DELETE /api/sessions/{id}/agent`: drops the session's idle agent, for a
/// client that changed the conversation outside a turn (a rewind); the next
/// turn rebuilds it from history.
pub(crate) async fn release_agent(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<crate::ui_server::types::ApiHealth>> {
    let session_id = session_id_param(&params)?;
    state.parked_agents.evict(&session_id);
    Ok(Json(crate::ui_server::types::API_OK))
}
