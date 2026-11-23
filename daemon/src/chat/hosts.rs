//! Each session's agent between turns.
//!
//! After a turn, the session's agent stays in the daemon as an idle
//! [`AgentWorker`]: it compacts the conversation when the provider's cache
//! is about to expire, answers `/compact`, `/context` and `/btw` without
//! rebuilding, and collects the background commands its turns detached.
//! The next turn continues with the same agent ([`ParkedAgents::take`]), so
//! the prompt cache stays warm and the commands keep running; it is rebuilt
//! only when something the agent was built from changed. A detached command
//! that finishes, or one blocked on stdin, starts a turn of its own
//! ([`super::start_background_turn`]).

pub use may_client::daemon::types::{
    BtwMessage, BtwRequest, BtwResponse, CompactRequest, CompactResponse, ContextDumpResponse,
};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use skyzen::Result as SkyResult;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};

use crate::MayAgent;
use crate::agent::ProviderLlm;
use crate::agent::{AgentWorker, TuiEvent, spawn_idle_worker};
use crate::error::{bad_request_error, internal_error, internal_error_message, json_ok_response};
use crate::state::DaemonState;
use may_protocol::model_slot::ModelSlot;

/// Tells every client of `session_id` how its compaction stands, on the
/// `session_compaction` frame kind.
pub fn publish_compaction(
    hub: &super::hub::TurnHub,
    session_id: &str,
    phase: crate::client::CompactionPhase,
) {
    let frame = crate::client::SessionCompactionFrame {
        session_id: session_id.to_string(),
        phase,
    };
    match serde_json::to_string(&frame) {
        Ok(json) => hub.publish(session_id, "session_compaction", json.into()),
        Err(error) => tracing::error!(%error, "failed to encode a compaction frame"),
    }
}

/// An idle session's parked agent worker.
type ParkedWorker = Arc<AgentWorker<ModelSlot<ProviderLlm>, crate::MayAgent>>;

/// Idle agents of the sessions the daemon ran.
#[derive(Default)]
pub struct ParkedAgents {
    workers: Mutex<HashMap<String, ParkedWorker>>,
}

impl std::fmt::Debug for ParkedAgents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParkedAgents").finish_non_exhaustive()
    }
}

impl ParkedAgents {
    fn workers(&self) -> std::sync::MutexGuard<'_, HashMap<String, ParkedWorker>> {
        self.workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Keeps `agent` as `session_id`'s idle agent.
    pub fn park(
        &self,
        state: &Arc<DaemonState>,
        session_id: &str,
        agent: MayAgent,
        released: crate::session::RunReleased,
    ) {
        // One pending wake is enough: the agent's context holds every event
        // a later wake would add, and the run it starts sees them all.
        let (wakes, wake_rx) = async_channel::bounded(1);
        let (compactions, compaction_rx) = async_channel::unbounded();
        let worker = spawn_idle_worker(
            executor_core::DefaultExecutor,
            agent,
            state.db(),
            state.runtime_service.runs().clone(),
            Some(crate::agent::BackgroundWakes { wakes, released }),
            Some(compactions),
        );
        // The compactions the worker runs on its own and on request reach
        // the session's clients; ends with the worker like the wake task.
        let hub = state.turn_hub.clone();
        let compaction_session = session_id.to_string();
        state.tokio_handle.spawn(async move {
            while let Ok(phase) = compaction_rx.recv().await {
                publish_compaction(&hub, &compaction_session, phase);
            }
        });
        let previous = self
            .workers()
            .insert(session_id.to_string(), Arc::new(worker));
        if let Some(previous) = previous {
            previous.shutdown();
        }
        // Ends with the worker, which drops the sending half.
        let wake_state = state.clone();
        let session_id = session_id.to_string();
        state.tokio_handle.spawn(async move {
            while let Ok(wake) = wake_rx.recv().await {
                // The session reads as resuming, and the wake's jobs as owed
                // a turn, through the whole turn, so a client never sees the
                // gap between the command finishing and the turn it starts;
                // the turn settles both when it releases the session.
                let resuming = wake_state
                    .session_background
                    .resume_pending(&session_id, wake.pending);
                super::start_background_turn(&wake_state, &session_id, resuming).await;
            }
        });
    }

    /// Stops the background work of `session_id`'s idle agent, if it has
    /// one: its terminal jobs are killed and its subagent runs cancelled,
    /// and their completions wait for the user's next input instead of
    /// starting a turn. The agent stays parked, its prompt cache warm.
    pub async fn stop_background(&self, state: &Arc<DaemonState>, session_id: &str) {
        let Some(worker) = self.workers().remove(session_id) else {
            return;
        };
        let (reply, agent) = async_channel::bounded(1);
        worker.release(reply);
        let Ok(mut agent) = agent.recv().await else {
            return;
        };
        agent.interrupt();
        let killed = agent.kill_running_terminal_jobs().await;
        let cancelled = agent.subagent_runs().cancel_all();
        tracing::info!(%session_id, killed, cancelled, "stopped the idle agent's background work");
        self.park(
            state,
            session_id,
            agent,
            crate::session::RunReleased::already_released(),
        );
    }

    /// Stops `session_id`'s idle agent, if any.
    pub fn evict(&self, session_id: &str) {
        let worker = self.workers().remove(session_id);
        if let Some(worker) = worker {
            worker.shutdown();
        }
    }

    /// Takes `session_id`'s idle agent for a turn.
    pub async fn take(&self, session_id: &str) -> Option<MayAgent> {
        let worker = self.workers().remove(session_id)?;
        let (reply, agent) = async_channel::bounded(1);
        worker.release(reply);
        agent.recv().await.ok()
    }

    /// Whether `session_id` has an idle agent.
    pub fn contains(&self, session_id: &str) -> bool {
        self.workers().contains_key(session_id)
    }

    /// The prompt-cache entry of `session_id`'s idle agent, when it has one.
    pub fn cache_warmth(&self, session_id: &str) -> Option<aither_agent::CacheWarmth> {
        self.get(session_id)?.cache_warmth().cloned()
    }

    pub fn get(
        &self,
        session_id: &str,
    ) -> Option<Arc<AgentWorker<ModelSlot<ProviderLlm>, crate::MayAgent>>> {
        self.workers().get(session_id).cloned()
    }
}

pub fn session_id_param(params: &Params) -> SkyResult<String> {
    Ok(params.get("id").map_err(bad_request_error)?.to_string())
}

/// The session's idle agent, built from its persisted context when the
/// daemon has none (it restarted, or the session never ran here).
pub async fn idle_agent(
    state: &Arc<DaemonState>,
    session_id: &str,
) -> SkyResult<Arc<AgentWorker<ModelSlot<ProviderLlm>, crate::MayAgent>>> {
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
        pending_entry_id: None,
    };
    let prepared = super::prepare_agent_for_request(
        state,
        &request,
        guard.fence(),
        false,
        true,
        // No turn is resubmitting a prompt here: replay the full history.
        super::TurnAttempt {
            provider_attempt: 0,
            turn_boundary: None,
            retained_agent: None,
        },
        &super::TurnKind::Prompt,
    )
    .await
    .map_err(internal_error);
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Err(release_error) = guard.finish_idle(crate::session::RunOutcome::Failed).await
            {
                tracing::warn!(
                    %session_id,
                    error = %release_error,
                    "failed to release the session after a failed agent build"
                );
            }
            return Err(error);
        }
    };
    // Park the built agent while the run guard still holds the session, passing
    // `guard.released()` so no concurrent caller can observe the session idle
    // before `parked_agents` holds its worker.
    state
        .parked_agents
        .park(state, session_id, prepared.agent, guard.released());
    let worker = state.parked_agents.get(session_id);
    if let Err(error) = guard
        .finish_idle(crate::session::RunOutcome::Completed)
        .await
    {
        tracing::warn!(%session_id, %error, "failed to release the session after building its agent");
    }
    worker.ok_or_else(|| internal_error_message("the idle agent stopped at once"))
}

/// Waits for the worker's answer to one command.
async fn answer(
    handle: crate::agent::AgentHandle<ModelSlot<ProviderLlm>, crate::MayAgent>,
) -> Option<TuiEvent<ModelSlot<ProviderLlm>>> {
    while let Some(event) = handle.recv().await {
        if !matches!(event, TuiEvent::Complete) {
            return Some(event);
        }
    }
    None
}

/// `POST /api/sessions/{id}/compact`.
pub async fn compact_inner(
    state: &Arc<DaemonState>,
    session_id: &str,
    req: CompactRequest,
) -> SkyResult<Json<CompactResponse>> {
    let worker = idle_agent(state, session_id).await?;
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
pub async fn context_report(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<skyzen::Response> {
    let session_id = session_id_param(&params)?;
    let worker = idle_agent(&state, &session_id).await?;
    match answer(worker.context_report()).await {
        Some(TuiEvent::ContextReport(Ok(report))) => json_ok_response(&report),
        Some(TuiEvent::ContextReport(Err(error))) => Err(internal_error(error)),
        _ => Err(internal_error_message("the agent gave no context report")),
    }
}

/// Answer of `GET /api/sessions/{id}/usage`: the session's provider usage
/// aggregated out of its request log, no agent needed.
pub async fn usage_report(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<skyzen::Response> {
    let session_id = session_id_param(&params)?;
    let rows = state
        .runtime_service
        .db()
        .request_logs_for_session(&session_id)
        .await
        .map_err(internal_error)?;
    let intervals = state
        .runtime_service
        .db()
        .session_intervals(&session_id)
        .await
        .map_err(internal_error)?;
    let misses = state
        .runtime_service
        .db()
        .cache_misses_for_session(&session_id)
        .await
        .map_err(internal_error)?;
    let created_at_ms = state
        .runtime_service
        .db()
        .get_session(&session_id)
        .await
        .map_err(internal_error)?
        .and_then(|session| {
            session
                .created_at
                .parse::<jiff::Timestamp>()
                .ok()
                .map(jiff::Timestamp::as_millisecond)
        });
    let now_ms = jiff::Timestamp::now().as_millisecond();
    json_ok_response(&crate::agent::assemble_usage_report(
        &session_id,
        &rows,
        &misses,
        &intervals,
        created_at_ms,
        now_ms,
    ))
}

/// `POST /api/sessions/{id}/context-dump`: writes the assembled context to
/// the session's `outputs/context-dump.md`.
pub async fn context_dump_inner(
    state: &Arc<DaemonState>,
    session_id: &str,
) -> SkyResult<Json<ContextDumpResponse>> {
    let worker = idle_agent(state, session_id).await?;
    let dump = match answer(worker.context_dump()).await {
        Some(TuiEvent::ContextDump(Ok(dump))) => dump,
        Some(TuiEvent::ContextDump(Err(error))) => return Err(internal_error(error)),
        _ => return Err(internal_error_message("the agent gave no context dump")),
    };
    let text = dump.render();
    let paths = crate::helpers::load_session_paths(state, session_id).await?;
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

/// `POST /api/sessions/{id}/btw`: answers a side question with the
/// session's model, outside the conversation.
pub async fn btw_inner(
    state: &Arc<DaemonState>,
    session_id: &str,
    req: BtwRequest,
) -> SkyResult<Json<BtwResponse>> {
    use aither_core::llm::Message;
    use futures_lite::StreamExt as _;

    // A live agent answers through a read-only fork of itself: it may run
    // commands in its own sandboxed space to gather what the answer needs.
    let question = req.messages.last().map(|message| message.content.clone());
    if let Some(question) = question
        && let Some(answer) = super::agent_messaging(state)
            .side_answer(session_id, &question)
            .await
            .map_err(internal_error)?
    {
        return Ok(Json(BtwResponse { answer }));
    }
    let messages = req
        .messages
        .into_iter()
        .map(|message| match message.role.as_str() {
            "system" => Message::system(message.content),
            "assistant" => Message::assistant(message.content),
            _ => Message::user(message.content),
        })
        .collect::<Vec<_>>();
    let worker = idle_agent(state, session_id).await?;
    let llm = worker.side_model().await.map_err(internal_error_message)?;
    let mut answer = String::new();
    let mut stream = std::pin::pin!(llm.respond(messages));
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
pub async fn release_agent(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<crate::types::ApiHealth>> {
    let session_id = session_id_param(&params)?;
    state.parked_agents.evict(&session_id);
    Ok(Json(crate::types::API_OK))
}
