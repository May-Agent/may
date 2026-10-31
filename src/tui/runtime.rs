use aither_core::llm::model::ReasoningEffort;
use base64::Engine;
use executor_core::spawn;

use crate::agent::{AgentHandle, AgentWorker};
use crate::ask_user::QuestionAnswer;
use crate::session::AttachmentKind;
use crate::tui::{App, Attachment, PendingMessage};
use crate::{ChatMessage, MayConfig, SessionId};

/// Error type for TUI runtime operations.
#[derive(Debug, thiserror::Error)]
pub enum TuiRuntimeError {
    /// Database error.
    #[error("database error: {0}")]
    Database(#[from] crate::db::ChatDbError),
    /// Session error.
    #[error("session error: {0}")]
    Session(#[from] crate::session::SessionError),
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Agent environment resolution failed.
    #[error("agent environment resolution failed: {0}")]
    ResolveAgentEnvironment(#[from] crate::agent::ResolveAgentEnvironmentError),
    /// No active session.
    #[error("No active session")]
    NoActiveSession,
    /// A turn was started without holding the session's run lease.
    #[error("no session run lease is held for this turn")]
    NoRunLease,
    /// The May daemon refused or could not be reached.
    #[error(transparent)]
    Daemon(#[from] crate::client::DaemonClientError),
    /// The May daemon is not attached yet.
    #[error("the May daemon is not attached")]
    DaemonUnavailable,
    /// History replay failed.
    #[error("history replay failed: {0}")]
    HistoryReplay(#[from] crate::session::HistoryReplayError),
    /// Foreign transcript import failed.
    #[error("import failed: {0}")]
    Import(#[from] crate::session::import::ImportError),
    /// Invalid pending message timestamp.
    #[error("invalid pending message timestamp: {0}")]
    InvalidPendingMessageTimestamp(#[source] jiff::Error),
    /// Persisted image attachment is missing content.
    #[error("persisted image attachment is missing content")]
    PersistedImageMissingContent,
    /// Unsupported persisted attachment type in TUI runtime.
    #[error("unsupported persisted attachment type in TUI runtime: {0}")]
    UnsupportedPersistedAttachmentType(AttachmentKind),
    /// Failed to convert path into file URL.
    #[error("failed to convert path into file URL: {path}")]
    FileUrlConversion {
        /// Path that could not be converted.
        path: std::path::PathBuf,
    },
    /// Failed to parse attachment URL.
    #[error("failed to parse attachment URL {url}: {source}")]
    AttachmentUrlParse {
        /// URL that failed to parse.
        url: String,
        /// Underlying URL parse error.
        #[source]
        source: url::ParseError,
    },
    /// Attachment URL is not a local file URL.
    #[error("attachment URL is not a local file URL: {url}")]
    NonLocalAttachmentUrl {
        /// URL that is not a local `file://` URL.
        url: String,
    },
    /// An answer to an open question was refused.
    #[error("{0}")]
    Answer(#[from] crate::session::AnswerError),
    /// Selected model is empty.
    #[error("Selected model is empty")]
    EmptySelectedModel,
    /// No model configured.
    #[error("No model configured")]
    NoModelConfigured,
    /// Location fetch failed.
    #[error("Location fetch failed: {0}")]
    LocationFetch(#[source] crate::location::LocationError),
    /// Agent initialization failed.
    #[error("agent initialization failed: {0}")]
    MayAgent(#[from] crate::agent::MayAgentError),
    /// Agent operation failed.
    #[error("agent operation failed: {0}")]
    Agent(#[from] aither_agent::AgentError),
    /// Failed to convert attachment path to URL.
    #[error("Failed to convert attachment path to URL: {path}")]
    AttachmentPathToUrl {
        /// Path that could not be converted.
        path: std::path::PathBuf,
    },
    /// Unable to determine attachment MIME type.
    #[error("Unable to determine attachment MIME type: {path}")]
    UnknownAttachmentMediaType {
        /// Path whose MIME type could not be determined.
        path: std::path::PathBuf,
    },
    /// Invalid data URL.
    #[error("invalid data URL")]
    InvalidDataUrl,
    /// Failed to decode data URL.
    #[error("failed to decode data URL: {0}")]
    DecodeDataUrl(#[source] base64::DecodeError),
}

/// Whether the caller committed its response to a prompt a run is blocked on.
///
/// Only the committer wakes the run with its own response. When another
/// process sharing the database committed first, it finds no waiter
/// in its own registry and takes the resume path, so this process answers
/// the live run with what was committed (`R`), read back by request id, and
/// the user is told their own response was not used.
#[derive(Debug, Clone)]
#[must_use]
pub enum ResponseCommit<R> {
    /// This call committed the response; the caller wakes the run with it.
    Committed,
    /// Another response was committed first; the caller answers the run with
    /// it.
    Lost(R),
    /// The prompt was withdrawn and nothing was committed; the run fails.
    Withdrawn,
}

/// Answers as `POST /api/ask-user/respond` takes them.
fn answers_json(answers: &[QuestionAnswer]) -> serde_json::Value {
    use crate::ask_user::AnswerValue;
    serde_json::Value::Array(
        answers
            .iter()
            .filter_map(|answer| {
                let value = match &answer.answer {
                    AnswerValue::Selected(values) => {
                        serde_json::json!({ "kind": "selected", "values": values })
                    }
                    AnswerValue::Custom(text) => {
                        serde_json::json!({ "kind": "custom", "text": text })
                    }
                    AnswerValue::Skipped => return None,
                };
                Some(serde_json::json!({ "section": answer.section, "answer": value }))
            })
            .collect(),
    )
}

fn persisted_attachment_from_tui(
    attachment: &Attachment,
) -> Result<crate::session::PersistedAttachment, TuiRuntimeError> {
    let id = uuid::Uuid::new_v4().to_string();
    match attachment {
        Attachment::Image { path, data } => Ok(crate::session::PersistedAttachment {
            id,
            name: file_name_or_default(path, "image.png"),
            attachment_type: AttachmentKind::Image,
            url: file_url(path)?,
            upload_data_url: Some(image_data_url(path, data)),
            text_content: None,
        }),
        Attachment::FileReference { path } => Ok(crate::session::PersistedAttachment {
            id,
            name: file_name_or_default(path, "file"),
            attachment_type: AttachmentKind::File,
            url: file_url(path)?,
            upload_data_url: None,
            text_content: None,
        }),
        Attachment::PastedText {
            content,
            saved_path,
        } => Ok(crate::session::PersistedAttachment {
            id,
            name: saved_path.as_ref().map_or_else(
                || "pasted.txt".to_string(),
                |path| file_name_or_default(path, "pasted.txt"),
            ),
            attachment_type: AttachmentKind::Text,
            url: saved_path
                .as_ref()
                .map(|path| file_url(path))
                .transpose()?
                .unwrap_or_else(|| "may://inline-text".to_string()),
            upload_data_url: None,
            text_content: Some(content.clone()),
        }),
    }
}

pub(super) async fn tui_attachment_from_persisted(
    attachment: crate::session::PersistedAttachment,
) -> Result<Attachment, TuiRuntimeError> {
    match attachment.attachment_type {
        AttachmentKind::Image => {
            let path = path_from_attachment_url(&attachment.url)?;
            let data = if async_fs::metadata(&path).await.is_ok() {
                async_fs::read(&path).await?
            } else if let Some(data_url) = attachment.upload_data_url.as_deref() {
                decode_data_url(data_url)?
            } else {
                return Err(TuiRuntimeError::PersistedImageMissingContent);
            };
            Ok(Attachment::Image { path, data })
        }
        AttachmentKind::File => Ok(Attachment::FileReference {
            path: path_from_attachment_url(&attachment.url)?,
        }),
        AttachmentKind::Text => Ok(Attachment::PastedText {
            content: if let Some(content) = attachment.text_content {
                content
            } else {
                let path = path_from_attachment_url(&attachment.url)?;
                async_fs::read_to_string(path).await?
            },
            saved_path: if attachment.url.starts_with("file://") {
                Some(path_from_attachment_url(&attachment.url)?)
            } else {
                None
            },
        }),
        other => Err(TuiRuntimeError::UnsupportedPersistedAttachmentType(other)),
    }
}

fn file_url(path: &std::path::Path) -> Result<String, TuiRuntimeError> {
    url::Url::from_file_path(path)
        .map_err(|()| TuiRuntimeError::FileUrlConversion {
            path: path.to_path_buf(),
        })
        .map(|url| url.to_string())
}

fn path_from_attachment_url(url: &str) -> Result<std::path::PathBuf, TuiRuntimeError> {
    url::Url::parse(url)
        .map_err(|source| TuiRuntimeError::AttachmentUrlParse {
            url: url.to_string(),
            source,
        })?
        .to_file_path()
        .map_err(|()| TuiRuntimeError::NonLocalAttachmentUrl {
            url: url.to_string(),
        })
}

fn file_name_or_default(path: &std::path::Path, default_name: &str) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| default_name.to_string(), ToOwned::to_owned)
}

fn image_data_url(path: &std::path::Path, data: &[u8]) -> String {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    let mut url = String::from("data:");
    url.push_str(mime.as_ref());
    url.push_str(";base64,");
    url.push_str(encoded.as_str());
    url
}

fn decode_data_url(data_url: &str) -> Result<Vec<u8>, TuiRuntimeError> {
    let Some((_, encoded)) = data_url.split_once(',') else {
        return Err(TuiRuntimeError::InvalidDataUrl);
    };
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(TuiRuntimeError::DecodeDataUrl)
}

/// Result of starting the next queued turn.
#[derive(Debug)]
pub enum QueuedRun {
    /// No message is dispatchable; the session was left untouched.
    Nothing,
    /// Another run holds the session; the queue was left untouched.
    Busy,
    /// The lease is held and the message was taken for the turn.
    Started(PendingMessage, Box<crate::session::SessionRunGuard>),
}

/// Outcome of [`RuntimeDeps::begin_queued_run`].
#[derive(Debug)]
pub enum QueuedRunStart {
    /// No message is dispatchable.
    Nothing,
    /// Another run holds the session; the queue was left untouched.
    Busy,
    /// The app holds the lease and the message is to run.
    Started(PendingMessage),
}

/// Runtime dependencies shared by session and agent operations.
#[derive(Debug, Clone)]
pub struct RuntimeDeps {
    tool_environments: crate::agent::ToolEnvironmentCache,
    oneshot_model: Option<String>,
    oneshot_reasoning_effort: Option<ReasoningEffort>,
    /// The May daemon that runs this TUI's agents, when it is a client.
    daemon: Option<crate::client::DaemonClient>,
}

/// An agent worker under construction off the event loop.
///
/// Construction prepares the session's tool environments, which may ask the
/// user for SSH consent, so the event loop awaits [`Self::wait`] alongside its
/// ask-user queue instead of blocking on it. Dropping the build cancels it.
#[derive(Debug)]
pub struct PendingWorkerBuild {
    model: String,
    settings: crate::agent::AgentLiveSettings,
    task: std::pin::Pin<Box<executor_core::AnyExecutorTask<Result<AgentWorker, TuiRuntimeError>>>>,
}

impl PendingWorkerBuild {
    /// Model the worker is being built for.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Config values the worker is built from.
    #[must_use]
    pub const fn settings(&self) -> &crate::agent::AgentLiveSettings {
        &self.settings
    }

    /// Waits for the worker; cancellation-safe, so it can be raced in a select.
    ///
    /// # Errors
    /// Returns the agent construction failure.
    pub async fn wait(&mut self) -> Result<AgentWorker, TuiRuntimeError> {
        self.task.as_mut().await
    }
}

impl RuntimeDeps {
    /// Create the dependency bundle shared by every agent worker the runtime
    /// spawns.
    #[must_use]
    pub const fn new(tool_environments: crate::agent::ToolEnvironmentCache) -> Self {
        Self {
            tool_environments,
            oneshot_model: None,
            oneshot_reasoning_effort: None,
            daemon: None,
        }
    }

    /// Run every agent in the daemon behind `client` instead of in-process.
    #[must_use]
    pub fn with_daemon(mut self, client: crate::client::DaemonClient) -> Self {
        self.daemon = Some(client);
        self
    }

    /// The daemon this TUI is a client of, if any.
    #[must_use]
    pub const fn daemon(&self) -> Option<&crate::client::DaemonClient> {
        self.daemon.as_ref()
    }

    /// Forgets the prepared tool environment of a deleted session.
    pub fn evict_session(&self, session_id: &SessionId) {
        self.tool_environments.evict(session_id);
    }

    /// Return a copy that overrides the configured model and reasoning effort for
    /// the next spawned worker only.
    #[must_use]
    pub fn with_oneshot_overrides(
        mut self,
        model: Option<String>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Self {
        self.oneshot_model = model;
        self.oneshot_reasoning_effort = reasoning_effort;
        self
    }

    /// Creates and activates a new session and starts building its worker.
    ///
    /// The previous session's worker is dropped at once; messages sent before
    /// the new worker is ready are queued for it.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the session cannot be created or the worker build fails.
    pub async fn begin_new_session(
        &self,
        app: &mut App,
        worker: &mut Option<AgentWorker>,
    ) -> Result<PendingWorkerBuild, TuiRuntimeError> {
        let model = self
            .oneshot_model
            .clone()
            .or_else(|| app.config.last_model.as_ref().map(ToString::to_string));
        let (sandbox_dir, session_id, messages) = {
            let session = app
                .session_manager
                .create_session_with_model(model.clone())?;
            (
                session.sandbox_dir.clone(),
                session.id.clone(),
                session.history.messages().to_vec(),
            )
        };

        app.session_manager.ensure_persisted().await?;
        app.init_session(&session_id, &messages, model.as_deref())
            .await;
        *worker = None;

        let selected_model = SelectedModel::resolve(&app.config, model.as_deref())?;
        self.begin_worker_build(
            app,
            sandbox_dir,
            session_id,
            selected_model,
            self.oneshot_reasoning_effort,
        )
    }

    /// Activates a stored session and starts building its worker.
    ///
    /// The previous session's worker is dropped at once; messages sent before
    /// the new worker is ready are queued for it.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the session cannot be loaded or the worker build fails.
    pub async fn begin_load_session(
        &self,
        app: &mut App,
        worker: &mut Option<AgentWorker>,
        id: &SessionId,
    ) -> Result<PendingWorkerBuild, TuiRuntimeError> {
        let session = app.session_manager.load_session(id).await?;

        let sandbox_dir = session.sandbox_dir.clone();
        let session_id = session.id.clone();
        let session_model = session.model.clone();
        let messages: Vec<_> = session.history.messages().to_vec();

        app.init_session(&session_id, &messages, session_model.as_deref())
            .await;
        *worker = None;

        let selected_model = SelectedModel::resolve(&app.config, session_model.as_deref())?;
        self.begin_worker_build(app, sandbox_dir, session_id, selected_model, None)
    }

    /// Import a foreign agent transcript into a fresh session and start
    /// building its worker.
    ///
    /// The transcript is located and parsed off the event loop, then the new
    /// session is seeded with the converted messages and a provenance note.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the transcript cannot be found or
    /// parsed, or when session/worker setup fails.
    pub async fn begin_import_session(
        &self,
        app: &mut App,
        worker: &mut Option<AgentWorker>,
        source: crate::session::import::ForeignAgent,
        query: &str,
    ) -> Result<PendingWorkerBuild, TuiRuntimeError> {
        let query_owned = query.to_string();
        let mut imported =
            blocking::unblock(move || crate::session::import::import(source, &query_owned)).await?;

        let model = imported.model.clone();
        let (sandbox_dir, _session_id, imported_count) = {
            let session = app
                .session_manager
                .create_session_with_model(model.clone())?;
            (
                session.sandbox_dir.clone(),
                session.id.clone(),
                imported.messages.len(),
            )
        };
        app.session_manager.ensure_persisted().await?;
        app.session_manager
            .set_current_title(imported.title.clone())
            .await?;

        // Persist image payloads the parser decoded into the session dir,
        // then fix the attachment URLs to their real paths.
        for (rel, bytes) in &imported.files {
            let path = sandbox_dir.join(rel);
            if let Some(parent) = path.parent() {
                async_fs::create_dir_all(parent).await?;
            }
            async_fs::write(&path, bytes).await?;
        }
        let provenance = serde_json::json!({
            "kind": "imported_session",
            "source": source.label(),
            "source_session_id": imported.source_id,
            "source_path": imported.source_path.display().to_string(),
            "source_cwd": imported.cwd,
            "note": "This session continues an imported foreign-agent transcript. Prior tool calls appear as `terminal` calls; reasoning state was preserved verbatim.",
        });
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        let mut injected = Vec::with_capacity(imported.messages.len() + 1);
        injected.push(ChatMessage::hidden_context(provenance.to_string()));
        injected.append(&mut imported.messages);
        let _: i64 = self
            .daemon()
            .ok_or(TuiRuntimeError::DaemonUnavailable)?
            .post(
                &format!("/api/sessions/{}/history", session_id.as_str()),
                &injected,
            )
            .await?;
        for message in injected {
            app.session_manager.record_unpersisted(message);
        }

        let messages: Vec<_> = app
            .session_manager
            .current()
            .map(|session| session.history.messages().to_vec())
            .unwrap_or_default();
        app.init_session(&session_id, &messages, model.as_deref())
            .await;
        app.chat.push_notice(&format!(
            "Imported {imported_count} messages from {} session {}",
            source.label(),
            imported.source_id
        ));
        *worker = None;

        let selected_model = SelectedModel::resolve(&app.config, model.as_deref())?;
        self.begin_worker_build(app, sandbox_dir, session_id, selected_model, None)
    }

    /// Starts building a worker for the current session on `model`; the
    /// current worker keeps serving until the new one is installed.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the worker build cannot start.
    pub fn begin_switch_model(
        &self,
        app: &App,
        model: &str,
    ) -> Result<PendingWorkerBuild, TuiRuntimeError> {
        let session = app
            .session_manager
            .current()
            .ok_or(TuiRuntimeError::NoActiveSession)?;

        let sandbox_dir = session.sandbox_dir.clone();
        let session_id = session.id.clone();

        self.begin_worker_build(
            app,
            sandbox_dir,
            session_id,
            SelectedModel::new(model)?,
            None,
        )
    }

    /// Starts building a replacement worker for the current session from the
    /// saved config, on the session's model; the current worker keeps serving
    /// until the new one is installed.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the worker build cannot start.
    pub fn begin_reload_worker(&self, app: &App) -> Result<PendingWorkerBuild, TuiRuntimeError> {
        let session = app
            .session_manager
            .current()
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        let sandbox_dir = session.sandbox_dir.clone();
        let session_id = session.id.clone();
        let selected_model = SelectedModel::resolve(&app.config, session.model.as_deref())?;
        self.begin_worker_build(app, sandbox_dir, session_id, selected_model, None)
    }

    /// Installs a worker finished by `build` as the current session's worker.
    pub fn install_worker(
        app: &mut App,
        worker: &mut Option<AgentWorker>,
        build: &PendingWorkerBuild,
        new_worker: AgentWorker,
    ) {
        app.set_current_model(build.model());
        app.worker_settings = build.settings().clone();
        app.background = new_worker.background().cloned();
        app.btw_llm = new_worker.btw_llm().cloned();
        app.refresh_jobs();
        *worker = Some(new_worker);
    }

    /// Start an agent turn for the current session.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when no session is active or the worker fails to start.
    pub fn start_agent_task(
        &self,
        app: &mut App,
        worker: &AgentWorker,
        msg: PendingMessage,
        title_gen: &mut Option<TitleGenContext>,
    ) -> Result<AgentHandle, TuiRuntimeError> {
        let needs_title = app
            .session_manager
            .current()
            .is_some_and(|s| s.title.is_none());

        if needs_title {
            *title_gen = Some(TitleGenContext {
                user_message: msg.content.clone(),
            });
        }

        Self::send_remote_turn(app, worker, msg)
    }

    /// Hands `msg` to the daemon. The daemon writes the turn's rows; the
    /// in-memory history records the message now so the chat's history
    /// positions stay aligned until the turn's end reloads them.
    fn send_remote_turn(
        app: &mut App,
        worker: &AgentWorker,
        msg: PendingMessage,
    ) -> Result<AgentHandle, TuiRuntimeError> {
        let attachments = msg
            .attachments
            .iter()
            .map(persisted_attachment_from_tui)
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(hidden_context_xml) = msg.hidden_context_xml.as_ref() {
            app.session_manager
                .record_unpersisted(ChatMessage::hidden_context(hidden_context_xml.to_string()));
        }
        app.session_manager
            .record_unpersisted(ChatMessage::user_with_attachments(
                msg.content.clone(),
                attachments.clone(),
            ));
        app.start_processing(&msg);
        Ok(worker.send(crate::agent::RemoteTurn {
            content: msg.content,
            attachments,
            hidden_context_xml: msg.hidden_context_xml.as_ref().map(ToString::to_string),
        }))
    }

    /// Settle the app after a run finishes: persist the assistant message, save
    /// the session, finish the run (which settles the session's runtime state
    /// and releases its lease), and kick off title generation.
    pub async fn finish_agent_task(
        &self,
        app: &mut App,
        agent_handle: &mut Option<AgentHandle>,
        title_gen: &mut Option<TitleGenContext>,
        _assistant_response: &AssistantReply,
        _outcome: crate::session::RunOutcome,
    ) {
        let _session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone());
        *agent_handle = None;
        app.finish_processing();

        // The daemon persisted the turn and names the session.
        if let Err(error) = app.session_manager.refresh_history().await {
            tracing::warn!(%error, "failed to reload the session history");
        }
        title_gen.take();
    }

    /// Rewind the conversation to just before the user message at
    /// `history_index`, optionally restoring tracked files to that
    /// prompt's checkpoint, then start building a worker that replays the
    /// truncated history.
    ///
    /// The prompt's text and attachments are placed back in the input for
    /// editing; pending queued messages are dropped with the abandoned
    /// timeline; the current worker is dropped since its context still
    /// holds it.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when `history_index` does not identify
    /// a user prompt, the persisted history cannot be truncated, or the
    /// worker build cannot start.
    pub async fn rewind_conversation(
        &self,
        app: &mut App,
        worker: &mut Option<AgentWorker>,
        history_index: usize,
        restore_code: bool,
    ) -> Result<PendingWorkerBuild, TuiRuntimeError> {
        let Some((content, attachments, call_ids)) = app.chat.prompt_rewind_payload(history_index)
        else {
            return Err(crate::session::SessionError::NotFound(format!(
                "no prompt at history position {history_index}"
            ))
            .into());
        };
        let point = app.session_manager.rewind_to_prompt(history_index).await?;

        let session = app
            .session_manager
            .current()
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        let session_id = session.id.clone();
        let sandbox_dir = session.sandbox_dir.clone();
        let model = session.model.clone();

        if restore_code {
            app.request_prompt_rewind(point.user_message_id, call_ids);
        }

        app.chat.truncate_prompt_at(history_index);
        app.input = content;
        app.input_char_count = app.input.chars().count();
        app.cursor = app.input_char_count;
        app.attachments = attachments;
        app.chat.push_notice(&format!(
            "⏪ rewound to before this prompt{}",
            if restore_code {
                " (conversation + code)"
            } else {
                " (conversation)"
            }
        ));
        app.scroll = usize::MAX;

        *worker = None;
        let selected_model = SelectedModel::resolve(&app.config, model.as_deref())?;
        self.begin_worker_build(app, sandbox_dir, session_id, selected_model, None)
    }

    /// A worker for `session_id` whose agent runs in the daemon. The model,
    /// reasoning effort and working directory the daemon runs the session
    /// with are recorded on the session first.
    fn begin_remote_worker(
        app: &App,
        client: crate::client::DaemonClient,
        session_id: SessionId,
        model: SelectedModel,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> PendingWorkerBuild {
        let cwd = app.cwd.display().to_string();
        let model_id = model.as_str().to_string();
        let app_events = app.event_tx.clone();
        let task = spawn(async move {
            let effort = reasoning_effort.map(|effort| match effort {
                ReasoningEffort::Low => "low",
                ReasoningEffort::Medium => "medium",
                _ => "high",
            });
            let _: serde_json::Value = client
                .put(
                    &format!("/api/sessions/{}", session_id.as_str()),
                    &serde_json::json!({
                        "model": model_id,
                        "reasoningEffort": effort,
                        "cwd": cwd,
                    }),
                )
                .await
                .map_err(TuiRuntimeError::from)?;
            Ok(crate::tui::remote::spawn_remote_worker(
                client,
                session_id.as_str().to_string(),
                app_events,
            ))
        });
        PendingWorkerBuild {
            model: model.0,
            settings: crate::agent::AgentLiveSettings::of(&app.config),
            task: Box::pin(task),
        }
    }

    fn begin_worker_build(
        &self,
        app: &App,
        _sandbox_dir: std::path::PathBuf,
        session_id: SessionId,
        model: SelectedModel,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<PendingWorkerBuild, TuiRuntimeError> {
        if let Some(client) = self.daemon.clone() {
            return Ok(Self::begin_remote_worker(
                app,
                client,
                session_id,
                model,
                reasoning_effort,
            ));
        }
        Ok(Self::begin_remote_worker(
            app,
            self.daemon
                .clone()
                .ok_or(TuiRuntimeError::DaemonUnavailable)?,
            session_id,
            model,
            reasoning_effort,
        ))
    }

    /// Append a message to the pending queue and sync the app state.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn enqueue_pending_message(
        &self,
        app: &mut App,
        message: PendingMessage,
    ) -> Result<(), TuiRuntimeError> {
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        if let Some(client) = &self.daemon {
            // The daemon dispatches queued and steering messages itself;
            // the turns they start reach this TUI as foreign turns.
            let body = serde_json::json!({
                "sessionId": session_id.as_str(),
                "content": message.content,
                "attachments": message
                    .attachments
                    .iter()
                    .map(persisted_attachment_from_tui)
                    .collect::<Result<Vec<_>, _>>()?,
                "hiddenContextXml": message.hidden_context_xml.as_ref().map(ToString::to_string),
                "deliveryMode": message.delivery_mode,
            });
            let _: serde_json::Value = client.post("/api/chat/enqueue", &body).await?;
        }
        Ok(())
    }

    ///
    /// # Errors
    /// Always succeeds.
    pub const fn listen_for_agent_messages(&self) -> Result<(), TuiRuntimeError> {
        Ok(())
    }

    /// Start the next queued turn; the daemon dispatches the queue, so a
    /// client never has anything to start itself.
    ///
    /// # Errors
    /// Always succeeds.
    pub const fn begin_queued_run(
        &self,
        _app: &mut App,
    ) -> Result<QueuedRunStart, TuiRuntimeError> {
        Ok(QueuedRunStart::Nothing)
    }

    /// The daemon takes the run lease when the turn starts there.
    ///
    /// # Errors
    /// Always succeeds.
    pub const fn begin_run(&self, _app: &mut App) -> Result<bool, TuiRuntimeError> {
        Ok(true)
    }

    /// Push-notify the user that a new open question arrived, through the
    /// daemon's notification service. Best effort: a rejected or unroutable
    /// notification is logged, never fatal.
    pub async fn notify_open_question(&self, session_id: &SessionId, question: &str) {
        let Some(client) = self.daemon() else {
            return;
        };
        match client
            .post::<_, serde_json::Value>(
                "/api/notifications",
                &serde_json::json!({
                    "title": "May has a question for you",
                    "body": question,
                    "sessionId": session_id.as_str(),
                }),
            )
            .await
        {
            Ok(_) => {
                tracing::debug!("open-question notification delivered");
            }
            Err(error) => {
                tracing::warn!(error = %error, "failed to send open-question notification");
            }
        }
    }

    /// Commit the user's answers to an open question; the answer joins the
    /// session queue, which the event loop dispatches. Returns whether this
    /// call committed them: `false` when the question is no longer open.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the answers do not fit the question
    /// or on storage failure.
    pub async fn answer_open_question(
        &self,
        request_id: &str,
        answers: &[QuestionAnswer],
    ) -> Result<bool, TuiRuntimeError> {
        if let Some(client) = &self.daemon {
            return Ok(client
                .post::<_, serde_json::Value>(
                    "/api/ask-user/respond",
                    &serde_json::json!({ "id": request_id, "answers": answers_json(answers) }),
                )
                .await
                .is_ok());
        }
        Ok(false)
    }

    /// Commit the user's answers to the prompt a run is blocked on; the
    /// caller wakes the run afterwards, only if this call committed.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the answers do not fit the prompt or
    /// on storage failure.
    pub async fn resolve_ask_user_response(
        &self,
        _session_id: &SessionId,
        request_id: &str,
        answers: &[QuestionAnswer],
    ) -> Result<ResponseCommit<Vec<QuestionAnswer>>, TuiRuntimeError> {
        let Some(client) = &self.daemon else {
            return Ok(ResponseCommit::Lost(answers.to_vec()));
        };
        let committed = client
            .post::<_, serde_json::Value>(
                "/api/ask-user/respond",
                &serde_json::json!({ "id": request_id, "answers": answers_json(answers) }),
            )
            .await
            .is_ok();
        Ok(if committed {
            ResponseCommit::Committed
        } else {
            ResponseCommit::Lost(answers.to_vec())
        })
    }

    /// Commit the workspace decision; the caller wakes the run afterwards,
    /// only if this call committed.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn resolve_workspace_response(
        &self,
        _session_id: &SessionId,
        request_id: &str,
        approved: bool,
    ) -> Result<ResponseCommit<bool>, TuiRuntimeError> {
        if let Some(client) = &self.daemon {
            let _: serde_json::Value = client
                .post(
                    "/api/workspace/respond",
                    &serde_json::json!({ "id": request_id, "approved": approved }),
                )
                .await?;
            return Ok(ResponseCommit::Committed);
        }
        Ok(ResponseCommit::Lost(approved))
    }

    /// Record the permission decision and clear the pending request.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn resolve_permission_response(
        &self,
        app: &App,
        request_id: &str,
        approved: bool,
    ) -> Result<(), TuiRuntimeError> {
        app.session_manager
            .current()
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        if let Some(client) = &self.daemon {
            let _: serde_json::Value = client
                .post(
                    "/api/permissions/respond",
                    &serde_json::json!({ "id": request_id, "approved": approved }),
                )
                .await?;
            return Ok(());
        }
        Ok(())
    }
}

/// The assistant text a run streamed, with the emission time of its newest chunk.
#[derive(Debug, Default)]
pub struct AssistantReply {
    text: String,
    at: Option<jiff::Timestamp>,
}

impl AssistantReply {
    /// Append a text chunk the agent emitted at `at`.
    pub fn push(&mut self, chunk: &str, at: jiff::Timestamp) {
        if chunk.is_empty() {
            return;
        }
        self.text.push_str(chunk);
        self.at = Some(at);
    }

    /// Forget the accumulated reply.
    pub fn clear(&mut self) {
        self.text.clear();
        self.at = None;
    }

    /// The accumulated text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Emission time of the newest chunk; `None` while the reply is empty.
    #[must_use]
    pub const fn at(&self) -> Option<jiff::Timestamp> {
        self.at
    }
}

/// Context needed to generate a session title once a run finishes.
#[derive(Debug)]
pub struct TitleGenContext {
    /// The user message that opened the session.
    pub user_message: String,
}

#[derive(Debug, Clone)]
struct SelectedModel(String);

impl SelectedModel {
    fn new(model: &str) -> Result<Self, TuiRuntimeError> {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            return Err(TuiRuntimeError::EmptySelectedModel);
        }
        Ok(Self(trimmed.to_string()))
    }

    fn resolve(config: &MayConfig, preferred: Option<&str>) -> Result<Self, TuiRuntimeError> {
        if let Some(model) = preferred {
            return Self::new(model);
        }

        if let Some(model) = config.last_model.as_ref().map(ToString::to_string) {
            return Self::new(&model);
        }

        let model = config
            .default_model_for_tier(crate::config::ModelTier::Balanced)
            .ok_or(TuiRuntimeError::NoModelConfigured)?;

        Self::new(&model.to_string())
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}
