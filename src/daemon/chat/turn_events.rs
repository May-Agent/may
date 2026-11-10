//! What one attempt of a turn does with each event its agent run emits.
//!
//! [`aither_agent::Run::drive`] hands every event to [`TurnEvents::handle`]
//! while the run keeps going, so a handler that waits on something the run
//! holds (the database, a client) never stalls it. The handler streams the
//! event to the clients, buffers the turn for persistence and tracks what
//! the turn did; a [`TurnStop`] ends the run between events.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Instant;

use aither_agent::{AgentEvent, TimedEvent};
use aither_core::LanguageModel as _;
use aither_core::llm::Usage;

use super::request_ledger::RequestLedger;
use super::turn_persister::TurnPersister;
use super::{
    ChatSink, PendingToolStream, SinkToolEvent, check_tasks_update, clear_terminal_runtime_state,
    closes_reasoning_phase, emit_speed, enforce_issue_ownership, is_takeover_active_for_session,
    log_first_agent_event, log_first_text_event, log_first_visible_tool_event, mark_run_running,
    normalize_tool_delta_for_sink, normalize_tool_start_for_sink, project_tool_summary_for_sink,
    record_request_log, record_tool_interval, segment_emitted_at, set_terminal_runtime_state,
};
use crate::command_reading::CommandReading;
use crate::daemon::state::DaemonState;
use crate::daemon::terminal_parsing::update_terminal_snapshot_from_tool_event;
use crate::daemon::tool_summary::ToolSummary;
use crate::daemon::types::{TerminalStateSnapshot, ToolEventRecord};
use crate::db::ChatDbError;
use crate::decode_meter::DecodeMeter;
use crate::session::{
    MessageTranslation, SessionId, SessionPaths, SessionRunGuard, TerminalRuntimeState,
};

/// Why a turn's run stopped before it finished.
#[derive(Debug)]
pub(super) enum TurnStop {
    /// The run was cancelled, or its client went away; the reason is logged.
    Cancelled(&'static str),
    /// A browser takeover paused the session.
    TakeoverPaused,
    /// The agent reported an error event.
    Failed(aither_agent::AgentError),
    /// The turn's state could not be stored.
    Persist(ChatDbError),
}

/// What an attempt of a turn knows and has done, as its run's events arrive.
pub(super) struct TurnEvents<'t, 's, S> {
    pub(super) state: &'t Arc<DaemonState>,
    pub(super) guard: &'t SessionRunGuard,
    pub(super) sink: &'s mut S,
    pub(super) session_id: &'t SessionId,
    pub(super) assistant_id: &'t str,
    pub(super) session_paths: &'t SessionPaths,
    /// Where the agent's terminal commands run: the directory a call's
    /// reading places an edit's hunks in.
    pub(super) workdir: &'t std::path::Path,
    pub(super) started_at: Instant,
    /// Whether the request asked the provider for reasoning text, which arms
    /// the reasoning phase at every tool-result boundary.
    pub(super) reasoning_display: bool,
    /// `Some` while a model call is in its reasoning phase: the instant the
    /// phase began, measured as user-perceived wait.
    pub(super) reasoning_phase: Option<Instant>,
    /// The model's context window, sent to clients with each usage.
    pub(super) context_window: Option<u32>,
    /// The provider and model the turn's requests go to.
    pub(super) provider_id: &'t str,
    pub(super) model: &'t str,
    /// The prompt prefix of the request in flight, until its usage arrives
    /// and the two are measured against the route's cache.
    pub(super) request_prefix: Option<aither_agent::context::PromptPrefix>,
    /// The newest request's usage: the context the window holds now.
    pub(super) usage: Usage,
    pub(super) ledger: RequestLedger<'t>,
    pub(super) decode_meter: DecodeMeter,
    /// The agent's model, whose vocabulary counts the streamed text.
    pub(super) counting_model: crate::agent::ProviderLlm,
    pub(super) last_speed_emit: Option<Instant>,
    /// The current text segment, flushed on tool calls.
    pub(super) segment_text: String,
    /// Emission time of the newest text chunk in `segment_text`.
    pub(super) segment_at: Option<jiff::Timestamp>,
    /// The translation of `segment_text`, when the reply drifted from the
    /// communication language.
    pub(super) segment_translation: Option<MessageTranslation>,
    /// Emission time of the newest agent event.
    pub(super) last_event_at: Option<jiff::Timestamp>,
    /// The whole assistant text.
    pub(super) full_text: String,
    pub(super) reasoning_text: String,
    /// How much of `reasoning_text` belongs to model responses that finished:
    /// what follows is the response in flight, which a retry discards.
    pub(super) reasoning_floor: usize,
    pub(super) terminal_snapshot: TerminalStateSnapshot,
    pub(super) saw_tool_activity: bool,
    pub(super) tool_arguments_by_id: HashMap<String, String>,
    pub(super) tool_started_at: HashMap<String, jiff::Timestamp>,
    pub(super) pending_tool_streams: HashMap<String, PendingToolStream>,
    pub(super) last_todo_hash: Option<u64>,
    /// The tasks.md content the issue ownership rule accepted last.
    pub(super) tasks_accepted: Option<String>,
    /// The `tasks` edit of each running terminal call and the `tasks.md` it
    /// started from, until the call ends and its card is built.
    pub(super) tasks_edits: HashMap<String, (crate::tasks_md::edit::Op, String)>,
    pub(super) turn_persister: TurnPersister,
    pub(super) first_agent_event_seen: bool,
    pub(super) first_visible_tool_seen: bool,
    pub(super) first_text_seen: bool,
}

/// Reads a call that edits files again in the directory it runs in, so its
/// hunks carry absolute line numbers. The files are read before the call
/// runs, so they are still unedited.
async fn locate_reading(workdir: &std::path::Path, mut summary: ToolSummary) -> ToolSummary {
    if summary
        .reading
        .as_ref()
        .is_some_and(CommandReading::updates_files)
        && let Some(script) = summary.script.clone()
    {
        let workdir = workdir.to_path_buf();
        summary.reading =
            Some(blocking::unblock(move || CommandReading::of_in(&script, &workdir)).await);
    }
    summary
}

impl<S: ChatSink> TurnEvents<'_, '_, S> {
    /// Remembers the `tasks.md` a terminal call that edits it starts from. A
    /// script with several edits has no card: its changes cannot be told
    /// apart.
    async fn note_tasks_edit(&mut self, id: &str, arguments: &str) {
        let Some(script) = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|value| value.get("script")?.as_str().map(str::to_owned))
        else {
            return;
        };
        let reading = CommandReading::of(&script);
        let mut edits = reading.tasks_edits();
        let (Some(op), None) = (edits.next(), edits.next()) else {
            return;
        };
        let before = self
            .session_paths
            .tasks_content()
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        self.tasks_edits.insert(id.to_owned(), (op, before));
    }

    /// Builds, stores and publishes the card of a finished `tasks` call from
    /// the rows it changed.
    async fn finish_tasks_card(&mut self, id: &str) -> bool {
        let Some((op, before)) = self.tasks_edits.remove(id) else {
            return true;
        };
        let after = self
            .session_paths
            .tasks_content()
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let Some(card) = crate::command_reading::Card::tasks(&op, &before, &after) else {
            return true;
        };
        super::cards::store(&self.state.db(), self.session_id.as_str(), id, Some(&card)).await;
        self.sink.on_tool_card(id, &card).await
    }

    /// Feeds streamed model output to the decode meter: counted with the
    /// model's vocabulary where it has one, estimated from its bytes
    /// otherwise.
    fn note_decoded(&mut self, text: &str) {
        match self.counting_model.count_text(text) {
            Some(tokens) => self.decode_meter.note_counted(tokens),
            None => self.decode_meter.note_output(text.len()),
        }
    }

    /// Whether the turn did anything beyond thinking: it started a tool
    /// call or produced response text.
    pub(super) const fn took_action(&self) -> bool {
        self.saw_tool_activity
            || self.first_visible_tool_seen
            || self.first_text_seen
            || !self.terminal_snapshot.executions.is_empty()
            || !self.full_text.is_empty()
    }
}

/// How long `provider_id` keeps the conversation's prompt cache warm.
pub(super) fn provider_cache_lifetime(
    state: &Arc<DaemonState>,
    provider_id: &str,
) -> crate::cache_lifetime::CacheLifetime {
    state
        .config
        .load()
        .provider
        .get(provider_id)
        .map(|instance| instance.cache_lifetime)
        .unwrap_or_default()
}

/// Measure a request against its route's prompt cache; a miss is logged and
/// persisted with its cause.
async fn record_prompt_cache(state: &Arc<DaemonState>, request: crate::db::PromptCacheRequest<'_>) {
    let (session_id, provider_id, model) = (request.session_id, request.provider_id, request.model);
    match state.db().observe_prompt_cache(request).await {
        Ok(Some(miss)) => tracing::warn!(
            session_id,
            provider_id,
            model,
            cause = %miss.cause,
            missed_tokens = miss.missed_tokens,
            read_tokens = miss.read_tokens,
            gap_ms = u64::try_from(miss.gap.as_millis()).unwrap_or(u64::MAX),
            diverged = ?miss.diverged,
            compacted = miss.since.rewrites.compacted,
            reassembled = miss.since.rewrites.reassembled,
            agent_rebuilt = miss.since.agent_rebuilt,
            model_switched = miss.since.model_switched,
            "prompt cache miss"
        ),
        Ok(None) => {}
        Err(error) => tracing::warn!(error = %error, "failed to record the prompt cache"),
    }
}

/// The handler [`aither_agent::Run::drive`] calls with each event.
pub(super) async fn step<'a, 't, 's, S: ChatSink>(
    turn: &'a mut TurnEvents<'t, 's, S>,
    timed: TimedEvent,
) -> ControlFlow<TurnStop, &'a mut TurnEvents<'t, 's, S>> {
    match turn.handle(timed).await {
        ControlFlow::Continue(()) => ControlFlow::Continue(turn),
        ControlFlow::Break(stop) => ControlFlow::Break(stop),
    }
}

impl<S: ChatSink> TurnEvents<'_, '_, S> {
    /// Handles one event of the run.
    pub(super) async fn handle(&mut self, timed: TimedEvent) -> ControlFlow<TurnStop> {
        let TimedEvent { at, event } = timed;
        self.last_event_at = Some(at);
        // The first visible output of a call closes the pending reasoning
        // phase; its marker goes out before the event.
        if closes_reasoning_phase(&event)
            && let Some(started) = self.reasoning_phase.take()
            && !self
                .sink
                .on_reasoning_end(started.elapsed().as_secs_f64())
                .await
        {
            return ControlFlow::Break(TurnStop::Cancelled(
                "client disconnected during reasoning end",
            ));
        }
        self.sink.on_agent_event(Ok(&event), self.context_window);
        match self.event(at, event).await {
            Ok(ControlFlow::Continue(())) => {}
            Ok(ControlFlow::Break(stop)) => return ControlFlow::Break(stop),
            Err(error) => return ControlFlow::Break(TurnStop::Persist(error)),
        }
        if is_takeover_active_for_session(self.state, self.session_id).await {
            return ControlFlow::Break(TurnStop::TakeoverPaused);
        }
        ControlFlow::Continue(())
    }

    /// Ends the reasoning phase a run that failed left open, the way the
    /// failure's own event would have.
    pub(super) async fn close_reasoning_phase(&mut self) -> ControlFlow<TurnStop> {
        if let Some(started) = self.reasoning_phase.take()
            && !self
                .sink
                .on_reasoning_end(started.elapsed().as_secs_f64())
                .await
        {
            return ControlFlow::Break(TurnStop::Cancelled(
                "client disconnected during reasoning end",
            ));
        }
        ControlFlow::Continue(())
    }

    fn log_first_agent_event(&mut self, kind: &'static str) {
        log_first_agent_event(
            &mut self.first_agent_event_seen,
            &self.started_at,
            self.session_id.as_str(),
            self.assistant_id,
            kind,
        );
    }

    fn log_first_visible_tool_event(&mut self, id: &str, name: &str, origin: &'static str) {
        log_first_visible_tool_event(
            &mut self.first_visible_tool_seen,
            &self.started_at,
            self.session_id.as_str(),
            self.assistant_id,
            id,
            name,
            origin,
        );
    }

    /// Flushes the current text segment into the turn as an assistant
    /// message of its own.
    fn flush_segment(&mut self) {
        if !self.segment_text.trim().is_empty() {
            self.turn_persister.push_assistant(
                std::mem::take(&mut self.segment_text),
                segment_emitted_at(self.segment_at),
            );
            if let Some(translation) = self.segment_translation.take() {
                self.turn_persister.translate_last(translation);
            }
        }
    }

    /// Forgets what the response in flight streamed so far: its text, its
    /// reasoning and the tool calls it began to stream. The replacement
    /// starts a reasoning phase of its own.
    fn discard_attempt(&mut self) {
        let kept = self.full_text.len().saturating_sub(self.segment_text.len());
        self.full_text.truncate(kept);
        self.segment_text.clear();
        self.segment_at = None;
        self.segment_translation = None;
        self.reasoning_text.truncate(self.reasoning_floor);
        self.pending_tool_streams.clear();
        self.reasoning_phase = self.reasoning_display.then(Instant::now);
        self.decode_meter.note_boundary();
    }

    async fn emit_speed(&mut self, stopped: &'static str) -> ControlFlow<TurnStop> {
        if emit_speed(self.sink, &self.decode_meter, &mut self.last_speed_emit).await {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(TurnStop::Cancelled(stopped))
        }
    }

    async fn event(
        &mut self,
        at: jiff::Timestamp,
        event: AgentEvent,
    ) -> Result<ControlFlow<TurnStop>, ChatDbError> {
        const TOOL_STREAM: &str = "client disconnected during tool stream";
        match event {
            AgentEvent::Reasoning(text) => {
                self.log_first_agent_event("reasoning");
                self.reasoning_text.push_str(&text);
                self.turn_persister.push_reasoning(&text);
                if !self.sink.on_reasoning(&text).await {
                    return Ok(ControlFlow::Break(TurnStop::Cancelled(
                        "client disconnected during reasoning stream",
                    )));
                }
                self.note_decoded(&text);
                Ok(self
                    .emit_speed("client disconnected during reasoning stream")
                    .await)
            }
            AgentEvent::Text(text) => {
                self.log_first_agent_event("text");
                log_first_text_event(
                    &mut self.first_text_seen,
                    &self.started_at,
                    self.session_id.as_str(),
                    self.assistant_id,
                );
                self.segment_text.push_str(&text);
                self.segment_at = Some(at);
                self.full_text.push_str(&text);
                if !self.sink.on_text(&text).await {
                    return Ok(ControlFlow::Break(TurnStop::Cancelled(
                        "client disconnected during text stream",
                    )));
                }
                self.note_decoded(&text);
                Ok(self
                    .emit_speed("client disconnected during text stream")
                    .await)
            }
            // The reply just streamed drifted from the communication
            // language: clients show this copy, the stored message keeps it
            // beside the original.
            AgentEvent::ReplyTranslated { text, language } => {
                if !self.sink.on_reply_translated(&text, &language).await {
                    return Ok(ControlFlow::Break(TurnStop::Cancelled(
                        "client disconnected during reply translation",
                    )));
                }
                self.segment_translation = Some(MessageTranslation { text, language });
                Ok(ControlFlow::Continue(()))
            }
            AgentEvent::PromptPrefix(prefix) => {
                self.request_prefix = Some(prefix);
                Ok(ControlFlow::Continue(()))
            }
            AgentEvent::Usage(usage) => {
                self.decode_meter.note_usage(usage.completion_tokens);
                record_request_log(self.state, self.ledger.completed(&usage, Instant::now())).await;
                if let Some(prefix) = self.request_prefix.take() {
                    record_prompt_cache(
                        self.state,
                        crate::db::PromptCacheRequest {
                            session_id: self.session_id.as_str(),
                            provider_id: self.provider_id,
                            model: self.model,
                            at: jiff::Timestamp::now(),
                            usage: &usage,
                            prefix,
                            lifetime: provider_cache_lifetime(self.state, self.provider_id),
                        },
                    )
                    .await;
                }
                self.usage = usage;
                Ok(ControlFlow::Continue(()))
            }
            AgentEvent::ToolCallDelta {
                id,
                name,
                arguments_fragment,
            } => {
                self.log_first_agent_event("tool_delta");
                tracing::trace!(
                    target: "may::chat::tool_delta",
                    id = %id,
                    name = %name,
                    fragment = %arguments_fragment,
                    "incoming ToolCallDelta from aither"
                );
                self.note_decoded(&arguments_fragment);
                if let ControlFlow::Break(stop) = self.emit_speed(TOOL_STREAM).await {
                    return Ok(ControlFlow::Break(stop));
                }
                let delivered = match normalize_tool_delta_for_sink(
                    &mut self.pending_tool_streams,
                    &id,
                    &name,
                    &arguments_fragment,
                ) {
                    SinkToolEvent::Start {
                        id,
                        name,
                        arguments,
                    } => {
                        self.log_first_visible_tool_event(&id, &name, "synthetic_start_from_delta");
                        let summary = project_tool_summary_for_sink(
                            &self.pending_tool_streams,
                            &id,
                            &name,
                            Some(arguments.as_str()),
                            false,
                        );
                        let summary = locate_reading(self.workdir, summary).await;
                        self.sink
                            .on_tool_start(&id, &name, &arguments, summary)
                            .await
                    }
                    SinkToolEvent::Delta {
                        id,
                        name,
                        arguments_fragment,
                    } => {
                        let summary = project_tool_summary_for_sink(
                            &self.pending_tool_streams,
                            &id,
                            &name,
                            None,
                            false,
                        );
                        self.sink
                            .on_tool_delta(&id, &name, &arguments_fragment, summary)
                            .await
                    }
                };
                Ok(if delivered {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(TurnStop::Cancelled(TOOL_STREAM))
                })
            }
            AgentEvent::ToolCallStart {
                id,
                name,
                arguments,
            } => {
                self.log_first_agent_event("tool_start");
                self.saw_tool_activity = true;
                // Args that never streamed as deltas still count as decoded
                // output.
                let streamed = self
                    .pending_tool_streams
                    .get(&id)
                    .map_or(0, |pending| pending.buffered_arguments.len());
                self.decode_meter
                    .note_output(arguments.len().saturating_sub(streamed));
                self.decode_meter.note_boundary();
                if matches!(
                    name.as_str(),
                    "terminal" | "terminal_input" | "terminal_kill" | "terminal_read"
                ) {
                    mark_run_running(self.state, self.guard.fence()).await?;
                }
                // Buffer accumulated text segment before the tool call.
                self.flush_segment();

                // The reading is stored with the call: a located edit's line
                // numbers come from files the call changes.
                let visible = match normalize_tool_start_for_sink(
                    &mut self.pending_tool_streams,
                    &id,
                    &name,
                    &arguments,
                ) {
                    Some(SinkToolEvent::Start {
                        id,
                        name,
                        arguments,
                    }) => {
                        let summary = project_tool_summary_for_sink(
                            &self.pending_tool_streams,
                            &id,
                            &name,
                            Some(arguments.as_str()),
                            false,
                        );
                        let summary = locate_reading(self.workdir, summary).await;
                        Some((id, name, arguments, summary))
                    }
                    _ => None,
                };
                let record = ToolEventRecord::Start {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    reading: visible
                        .as_ref()
                        .and_then(|(_, _, _, summary)| summary.reading.clone()),
                };
                let serialized = serde_json::to_string(&record)
                    .map_err(|e| ChatDbError::Storage(e.to_string()))?;
                self.turn_persister
                    .push_tool_event(id.clone(), serialized, at);
                update_terminal_snapshot_from_tool_event(&mut self.terminal_snapshot, &record);
                self.tool_arguments_by_id
                    .insert(id.clone(), arguments.clone());
                self.tool_started_at.insert(id.clone(), at);
                if name == "terminal" {
                    self.note_tasks_edit(&id, &arguments).await;
                }
                if let Some((id, name, arguments, summary)) = visible {
                    self.log_first_visible_tool_event(&id, &name, "canonical_start");
                    if !self
                        .sink
                        .on_tool_start(&id, &name, &arguments, summary)
                        .await
                    {
                        return Ok(ControlFlow::Break(TurnStop::Cancelled(TOOL_STREAM)));
                    }
                }
                Ok(ControlFlow::Continue(()))
            }
            AgentEvent::ToolCallEnd { id, name, result } => {
                self.log_first_agent_event("tool_end");
                self.saw_tool_activity = true;
                let record = ToolEventRecord::End {
                    id: id.clone(),
                    name: name.clone(),
                    result: result.clone(),
                };
                let serialized = serde_json::to_string(&record)
                    .map_err(|e| ChatDbError::Storage(e.to_string()))?;
                self.turn_persister
                    .push_tool_event(id.clone(), serialized, at);
                update_terminal_snapshot_from_tool_event(&mut self.terminal_snapshot, &record);
                if let Some(started) = self.tool_started_at.remove(&id) {
                    record_tool_interval(self.state, self.session_id.as_str(), started, at).await;
                }
                let arguments = self.tool_arguments_by_id.remove(&id);
                let summary = project_tool_summary_for_sink(
                    &self.pending_tool_streams,
                    &id,
                    &name,
                    arguments.as_deref(),
                    true,
                );
                self.pending_tool_streams.remove(&id);
                if !self
                    .sink
                    .on_tool_end(&id, &name, arguments.as_deref(), &result, summary)
                    .await
                {
                    return Ok(ControlFlow::Break(TurnStop::Cancelled(TOOL_STREAM)));
                }

                // A tasks.md change that takes an issue another agent owns is
                // reverted before anyone reads it.
                enforce_issue_ownership(self.state, self.session_id, &mut self.tasks_accepted)
                    .await;
                if !self.finish_tasks_card(&id).await {
                    return Ok(ControlFlow::Break(TurnStop::Cancelled(TOOL_STREAM)));
                }
                // Check if tasks.md changed after tool execution.
                if let Some(rows) =
                    check_tasks_update(self.session_paths, &mut self.last_todo_hash).await
                    && !self.sink.on_todo_update(rows).await
                {
                    return Ok(ControlFlow::Break(TurnStop::Cancelled(
                        "client disconnected during todo update",
                    )));
                }
                // A tool result is the boundary after which the next call may
                // reason again.
                self.reasoning_phase = self.reasoning_display.then(Instant::now);
                Ok(ControlFlow::Continue(()))
            }
            AgentEvent::BackgroundTaskStarted {
                task_id,
                output_preview,
                output_file,
            } => {
                self.saw_tool_activity = true;
                set_terminal_runtime_state(
                    self.state,
                    self.session_id.as_str(),
                    TerminalRuntimeState {
                        task_id: Some(task_id),
                        output_file: Some(output_file),
                        output_preview: Some(output_preview),
                        input_notice: None,
                    },
                )
                .await?;
                Ok(ControlFlow::Continue(()))
            }
            // Delivered tool output / a resumed run arms the next call's
            // reasoning phase.
            AgentEvent::BuiltInToolResult { .. } | AgentEvent::RunResumed { .. } => {
                self.reasoning_phase = self.reasoning_display.then(Instant::now);
                Ok(ControlFlow::Continue(()))
            }
            // A reply without tool calls that the run follows with another
            // reply is stored as a message of its own.
            AgentEvent::TurnComplete { has_tool_calls, .. } => {
                if !has_tool_calls {
                    self.flush_segment();
                }
                self.reasoning_floor = self.reasoning_text.len();
                Ok(ControlFlow::Continue(()))
            }
            // The response stream broke after it opened and the same request
            // is sent again: what streamed since it was sent is the broken
            // attempt's, which neither the conversation nor the stored turn
            // keeps. Clients drop what they rendered of it and show the wait.
            AgentEvent::ResponseRetry {
                error,
                retry,
                delay,
                ..
            } => {
                self.discard_attempt();
                let until = jiff::Timestamp::now()
                    .checked_add(delay)
                    .expect("a retry delay is far below the timestamp range");
                if !self.sink.on_retract().await
                    || !self
                        .sink
                        .on_hold(
                            crate::client::HoldReason::Retry { until, retry },
                            &error.to_string(),
                        )
                        .await
                {
                    return Ok(ControlFlow::Break(TurnStop::Cancelled(
                        "client disconnected during a response retry",
                    )));
                }
                Ok(ControlFlow::Continue(()))
            }
            AgentEvent::BackgroundTaskCompleted { .. } => {
                self.saw_tool_activity = true;
                self.reasoning_phase = self.reasoning_display.then(Instant::now);
                clear_terminal_runtime_state(self.state, self.session_id.as_str()).await?;
                mark_run_running(self.state, self.guard.fence()).await?;
                Ok(ControlFlow::Continue(()))
            }
            AgentEvent::Error(error) => Ok(ControlFlow::Break(TurnStop::Failed(error))),
            _ => Ok(ControlFlow::Continue(())),
        }
    }
}
