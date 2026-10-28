use std::sync::Arc;

use aither_core::llm::{Attachment as LlmAttachment, model::ReasoningEffort};
use base64::Engine;
use executor_core::{DefaultExecutor, spawn};

use crate::agent::{AgentHandle, AgentWorker, spawn_worker};
use crate::ask_user::{AskUserBroker, Question, QuestionAnswer};
use crate::session::AttachmentKind;
use crate::tool_dispatch::ToolDispatchers;
use crate::tui::{App, Attachment, PendingMessage};
use crate::workspace_request::WorkspaceRequestBroker;
use crate::{ChatDb, ChatMessage, MayAgent, MayConfig, SessionId, generate_session_title};

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
    /// Failed to serialize paste XML.
    #[error("Failed to serialize paste XML: {0}")]
    SerializePasteXml(#[source] quick_xml::SeError),
    /// Invalid data URL.
    #[error("invalid data URL")]
    InvalidDataUrl,
    /// Failed to decode data URL.
    #[error("failed to decode data URL: {0}")]
    DecodeDataUrl(#[source] base64::DecodeError),
}

/// Session runtime facade used by the local TUI: queue state, pending interactions, and permission approval.
#[derive(Debug, Clone)]
pub struct LocalSessionRuntime {
    runtime_service: Arc<crate::session::SessionRuntimeService>,
    permission_approver: crate::agent::PermissionApprover,
    oneshot_restricted: bool,
}

impl LocalSessionRuntime {
    /// Create the runtime facade backed by `db`.
    #[must_use]
    pub fn new(db: ChatDb) -> Self {
        let runtime_service = Arc::new(crate::session::SessionRuntimeService::new(
            db,
            crate::ui_event_bus::UiEventBus::new(),
        ));
        let permission_approver =
            crate::agent::PermissionApprover::new().with_runtime_service(runtime_service.clone());
        Self {
            runtime_service,
            permission_approver,
            oneshot_restricted: false,
        }
    }

    /// Restrict runtime behaviour for one-shot (non-interactive) execution.
    pub const fn enable_oneshot_restricted_mode(&mut self) {
        self.oneshot_restricted = true;
    }

    /// Permission approver bound to `session_id`.
    #[must_use]
    pub fn permission_approver_for(
        &self,
        session_id: &SessionId,
    ) -> crate::agent::PermissionApprover {
        if self.oneshot_restricted {
            return self
                .permission_approver
                .for_session(session_id.as_str().to_string())
                .without_interactive_approval("restricted oneshot mode");
        }
        self.permission_approver
            .for_session(session_id.as_str().to_string())
    }

    async fn replace_app_queue(
        &self,
        app: &mut App,
        runtime: crate::session::SessionRuntimeSummary,
    ) -> Result<(), TuiRuntimeError> {
        let pending_immediate = match runtime.pending_immediate {
            Some(message) => Some(pending_message_from_runtime(message).await?),
            None => None,
        };
        let mut queued_messages = Vec::with_capacity(runtime.queued_messages.len());
        for message in runtime.queued_messages {
            queued_messages.push(pending_message_from_runtime(message).await?);
        }
        app.queue.replace(pending_immediate, queued_messages);
        Ok(())
    }

    /// Reconcile the app's queue view with persisted pending messages.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn sync_app_queue(
        &self,
        app: &mut App,
        session_id: &SessionId,
    ) -> Result<(), TuiRuntimeError> {
        let runtime = self.runtime_service.get(session_id.as_str()).await?;
        self.replace_app_queue(app, runtime).await
    }

    /// Drop every persisted pending message for a session and refresh the
    /// app's queue view. Rewind discards them with the rest of the future.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn clear_pending(
        &self,
        app: &mut App,
        session_id: &SessionId,
    ) -> Result<(), TuiRuntimeError> {
        let runtime = self
            .runtime_service
            .clear_pending_messages(session_id.as_str())
            .await?;
        self.replace_app_queue(app, runtime).await
    }

    /// Append a message to the pending queue and sync the app state.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn enqueue_pending_message(
        &self,
        app: &mut App,
        session_id: &SessionId,
        message: PendingMessage,
    ) -> Result<(), TuiRuntimeError> {
        let runtime = self
            .runtime_service
            .enqueue(
                session_id.as_str(),
                pending_message_to_runtime(&message)?,
                message.delivery_mode,
            )
            .await?;
        self.replace_app_queue(app, runtime).await
    }

    /// Take the next pending message eligible for dispatch.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn take_next_dispatchable(
        &self,
        app: &mut App,
        session_id: &SessionId,
    ) -> Result<Option<PendingMessage>, TuiRuntimeError> {
        let next = self
            .runtime_service
            .take_next_dispatchable(session_id.as_str())
            .await?;
        let next = match next {
            Some(message) => Some(pending_message_from_runtime(message).await?),
            None => None,
        };
        self.sync_app_queue(app, session_id).await?;
        Ok(next)
    }

    /// Mark a session as running.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn set_running(&self, session_id: &SessionId) -> Result<(), TuiRuntimeError> {
        self.runtime_service
            .set_state(
                session_id.as_str(),
                crate::session::SessionRuntimeState::Running,
            )
            .await?;
        Ok(())
    }

    /// Clear the running flag when `expected` still holds it.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn clear_running_if_needed(
        &self,
        session_id: &SessionId,
    ) -> Result<(), TuiRuntimeError> {
        self.runtime_service
            .clear_running_if_needed(session_id.as_str())
            .await?;
        Ok(())
    }

    /// Persist an ask-user request and surface it in the app.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn register_ask_user_request(
        &self,
        app: &mut App,
        session_id: &SessionId,
        questions: &[Question],
    ) -> Result<String, TuiRuntimeError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        self.runtime_service
            .set_waiting_ask_user(
                session_id.as_str(),
                Some(crate::session::PersistedAskUserRequest {
                    id: request_id.clone(),
                    questions: questions
                        .iter()
                        .map(|question| crate::session::PersistedAskUserQuestion {
                            section: question.section.clone(),
                            question: question.question.clone(),
                            option: question.option.clone(),
                            multi_select: question.multi_select,
                        })
                        .collect(),
                }),
            )
            .await?;
        app.session_manager
            .db()
            .append_ask_user_request_message(
                session_id.as_str(),
                request_id.as_str(),
                questions,
                None,
            )
            .await?;
        Ok(request_id)
    }

    /// Record the user's answers and clear the pending request.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn resolve_ask_user_response(
        &self,
        app: &mut App,
        session_id: &SessionId,
        request_id: &str,
        answers: &[QuestionAnswer],
    ) -> Result<(), TuiRuntimeError> {
        self.runtime_service
            .set_waiting_ask_user(session_id.as_str(), None)
            .await?;
        self.runtime_service
            .set_state(
                session_id.as_str(),
                crate::session::SessionRuntimeState::Running,
            )
            .await?;
        app.session_manager
            .db()
            .append_ask_user_response_message(session_id.as_str(), request_id, answers, None)
            .await?;
        Ok(())
    }

    /// Persist a workspace-access request and surface it in the app.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn register_workspace_request(
        &self,
        app: &mut App,
        session_id: &SessionId,
        request: &crate::workspace_request::RequestWorkspaceArgs,
    ) -> Result<String, TuiRuntimeError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        self.runtime_service
            .set_waiting_workspace(
                session_id.as_str(),
                Some(crate::session::PersistedWorkspaceRequest {
                    id: request_id.clone(),
                    request: crate::session::PersistedWorkspaceRequestPayload {
                        path: request.path.clone(),
                        reason: request.reason.clone(),
                        session_id: Some(session_id.as_str().to_string()),
                    },
                }),
            )
            .await?;
        app.session_manager
            .db()
            .append_workspace_request_message(
                session_id.as_str(),
                request_id.as_str(),
                request.path.as_str(),
                request.reason.as_str(),
                None,
            )
            .await?;
        Ok(request_id)
    }

    /// Record the workspace decision and clear the pending request.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn resolve_workspace_response(
        &self,
        app: &mut App,
        session_id: &SessionId,
        request_id: &str,
        approved: bool,
    ) -> Result<(), TuiRuntimeError> {
        self.runtime_service
            .set_waiting_workspace(session_id.as_str(), None)
            .await?;
        self.runtime_service
            .set_state(
                session_id.as_str(),
                crate::session::SessionRuntimeState::Running,
            )
            .await?;
        app.session_manager
            .db()
            .append_workspace_response_message(session_id.as_str(), request_id, approved, None)
            .await?;
        Ok(())
    }

    /// Fetch the permission request blocking the session, if any.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn waiting_permission(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<crate::session::PersistedPermissionRequest>, TuiRuntimeError> {
        Ok(self
            .runtime_service
            .get(session_id.as_str())
            .await?
            .waiting_permission)
    }

    /// Record the permission decision and clear the pending request.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn resolve_permission_response(
        &self,
        app: &mut App,
        session_id: &SessionId,
        request_id: &str,
        approved: bool,
    ) -> Result<(), TuiRuntimeError> {
        let responded = self.permission_approver.respond(request_id, approved);
        self.runtime_service
            .set_waiting_permission(session_id.as_str(), None)
            .await?;
        self.runtime_service
            .set_state(
                session_id.as_str(),
                if responded {
                    crate::session::SessionRuntimeState::Running
                } else {
                    crate::session::SessionRuntimeState::Idle
                },
            )
            .await?;
        app.session_manager
            .db()
            .append_permission_response_message(session_id.as_str(), request_id, approved, None)
            .await?;
        if !responded {
            tracing::warn!(
                request_id,
                "permission response resolved without a live waiter"
            );
        }
        Ok(())
    }
}

fn pending_message_to_runtime(
    message: &PendingMessage,
) -> Result<crate::session::PendingUserMessage, TuiRuntimeError> {
    Ok(crate::session::PendingUserMessage {
        content: message.content.clone(),
        attachments: message
            .attachments
            .iter()
            .map(persisted_attachment_from_tui)
            .collect::<Result<Vec<_>, _>>()?,
        hidden_context_xml: message.hidden_context_xml.clone(),
        created_at: message.queued_at.to_string(),
        delivery_mode: message.delivery_mode,
    })
}

async fn pending_message_from_runtime(
    message: crate::session::PendingUserMessage,
) -> Result<PendingMessage, TuiRuntimeError> {
    let mut attachments = Vec::with_capacity(message.attachments.len());
    for attachment in message.attachments {
        attachments.push(tui_attachment_from_persisted(attachment).await?);
    }
    Ok(PendingMessage {
        content: message.content,
        hidden_context_xml: message.hidden_context_xml,
        attachments,
        queued_at: message
            .created_at
            .parse()
            .map_err(TuiRuntimeError::InvalidPendingMessageTimestamp)?,
        delivery_mode: message.delivery_mode,
        display: None,
    })
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

/// Runtime dependencies shared by session and agent operations.
#[derive(Debug, Clone)]
pub struct RuntimeDeps {
    workspace_broker: WorkspaceRequestBroker,
    ask_user_broker: AskUserBroker,
    tool_dispatchers: ToolDispatchers,
    location_provider: Arc<crate::location::LocationProvider>,
    local_runtime: LocalSessionRuntime,
    tool_distribution: Arc<aither_sandbox::ToolDistribution>,
    tool_environments: crate::agent::ToolEnvironmentCache,
    oneshot_model: Option<String>,
    oneshot_reasoning_effort: Option<ReasoningEffort>,
}

/// An agent worker under construction off the event loop.
///
/// Construction prepares the session's tool environments, which may ask the
/// user for SSH consent, so the event loop awaits [`Self::wait`] alongside its
/// ask-user queue instead of blocking on it. Dropping the build cancels it.
#[derive(Debug)]
pub struct PendingWorkerBuild {
    model: String,
    task: std::pin::Pin<Box<executor_core::AnyExecutorTask<Result<AgentWorker, TuiRuntimeError>>>>,
}

impl PendingWorkerBuild {
    /// Model the worker is being built for.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Waits for the worker; cancellation-safe, so it can be raced in a select.
    ///
    /// # Errors
    /// Returns the agent construction failure.
    pub async fn wait(&mut self) -> Result<AgentWorker, TuiRuntimeError> {
        self.task.as_mut().await
    }
}

/// Everything a worker build needs, owned so it runs off the event loop.
struct WorkerBuildInputs {
    deps: RuntimeDeps,
    config: MayConfig,
    db: ChatDb,
    history_messages: Vec<aither_core::llm::Message>,
    may_dir: std::path::PathBuf,
    cwd: std::path::PathBuf,
    profile_id: Option<String>,
    sandbox_dir: std::path::PathBuf,
    session_id: SessionId,
    model: String,
    reasoning_effort: Option<ReasoningEffort>,
}

impl RuntimeDeps {
    /// Create the dependency bundle shared by every agent worker the runtime
    /// spawns.
    pub const fn new(
        local_runtime: LocalSessionRuntime,
        workspace_broker: WorkspaceRequestBroker,
        ask_user_broker: AskUserBroker,
        tool_dispatchers: ToolDispatchers,
        location_provider: Arc<crate::location::LocationProvider>,
        tool_distribution: Arc<aither_sandbox::ToolDistribution>,
        tool_environments: crate::agent::ToolEnvironmentCache,
    ) -> Self {
        Self {
            workspace_broker,
            ask_user_broker,
            tool_dispatchers,
            location_provider,
            local_runtime,
            tool_distribution,
            tool_environments,
            oneshot_model: None,
            oneshot_reasoning_effort: None,
        }
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
        self.local_runtime.sync_app_queue(app, &session_id).await?;

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
        self.local_runtime.sync_app_queue(app, &session_id).await?;

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
        let (sandbox_dir, session_id, imported_count) = {
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
        app.session_manager
            .append_message(ChatMessage::hidden_context(provenance.to_string()))
            .await?;
        for message in imported.messages.drain(..) {
            app.session_manager.append_message(message).await?;
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
        self.local_runtime.sync_app_queue(app, &session_id).await?;

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

    /// Installs a worker finished by `build` as the current session's worker.
    pub fn install_worker(
        app: &mut App,
        worker: &mut Option<AgentWorker>,
        build: &PendingWorkerBuild,
        new_worker: AgentWorker,
    ) {
        app.set_current_model(build.model());
        app.job_registry = Some(new_worker.job_registry().clone());
        app.subagent_runs = Some(new_worker.subagent_runs().clone());
        app.btw_llm = Some(new_worker.btw_llm().clone());
        app.subagent_tracker = new_worker.subagent_tracker().clone();
        *worker = Some(new_worker);
    }

    /// Start an agent turn for the current session.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when no session is active or the worker fails to start.
    pub async fn start_agent_task(
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

        if let Some(hidden_context_xml) = msg.hidden_context_xml.as_ref() {
            app.session_manager
                .append_message(ChatMessage::hidden_context(hidden_context_xml.to_string()))
                .await?;
        }

        let persisted_attachments = msg
            .attachments
            .iter()
            .map(persisted_attachment_from_tui)
            .collect::<Result<Vec<_>, _>>()?;
        let user_message_id = app
            .session_manager
            .append_message(ChatMessage::user_with_attachments(
                msg.content.clone(),
                persisted_attachments,
            ))
            .await?;
        // Checkpoint the code at this prompt boundary so a later
        // conversation-and-code rewind can restore it.
        app.request_prompt_mark(user_message_id);

        let (prompt, image_urls) = prepare_message_content(&msg)?;
        app.start_processing(&msg);
        Ok(worker.run(
            prompt,
            msg.hidden_context_xml.as_ref().map(ToString::to_string),
            image_urls,
        ))
    }

    /// Settle the app after a run finishes: persist the assistant message, save
    /// the session, clear runtime state, and kick off title generation.
    pub async fn finish_agent_task(
        &self,
        app: &mut App,
        agent_handle: &mut Option<AgentHandle>,
        title_gen: &mut Option<TitleGenContext>,
        assistant_response: &str,
    ) {
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone());
        *agent_handle = None;
        app.finish_processing();

        if !assistant_response.is_empty()
            && let Err(error) = app
                .session_manager
                .append_message(ChatMessage::assistant(assistant_response))
                .await
        {
            tracing::warn!(error = %error, "failed to persist assistant message");
        }

        if let Err(e) = app.session_manager.save_current().await {
            tracing::warn!("Failed to save session history: {}", e);
        }
        if let Some(session_id) = session_id.as_ref() {
            if let Err(error) = self.local_runtime.clear_running_if_needed(session_id).await {
                tracing::warn!(session_id = %session_id, error = %error, "failed to clear local runtime state");
            }
            if let Err(error) = self.local_runtime.sync_app_queue(app, session_id).await {
                tracing::warn!(session_id = %session_id, error = %error, "failed to refresh local pending queue");
            }
        }

        if let Some(ctx) = title_gen.take()
            && !assistant_response.is_empty()
        {
            Self::spawn_title_generation(
                app.config.clone(),
                app.session_manager.db().clone(),
                app.session_manager.current().map(|s| s.id.clone()),
                ctx.user_message,
                assistant_response.to_string(),
            );
        }
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

        self.local_runtime.clear_pending(app, &session_id).await?;

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

    fn begin_worker_build(
        &self,
        app: &App,
        sandbox_dir: std::path::PathBuf,
        session_id: SessionId,
        model: SelectedModel,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<PendingWorkerBuild, TuiRuntimeError> {
        let current = app.session_manager.current();
        let history_messages = current
            .map(|session| session.history.to_aither_messages())
            .transpose()
            .map_err(TuiRuntimeError::HistoryReplay)?
            .unwrap_or_default();
        let inputs = WorkerBuildInputs {
            deps: self.clone(),
            config: app.config.clone(),
            db: app.session_manager.db().clone(),
            history_messages,
            may_dir: app.session_manager.may_dir().to_path_buf(),
            cwd: app.cwd.clone(),
            profile_id: current.and_then(|session| session.profile_id.clone()),
            sandbox_dir,
            session_id,
            model: model.as_str().to_string(),
            reasoning_effort,
        };
        Ok(PendingWorkerBuild {
            model: model.0,
            task: Box::pin(spawn(build_worker_for_session(inputs))),
        })
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
        self.local_runtime
            .enqueue_pending_message(app, &session_id, message)
            .await
    }

    /// Take the next pending message eligible for dispatch.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn take_next_dispatchable(
        &self,
        app: &mut App,
    ) -> Result<Option<PendingMessage>, TuiRuntimeError> {
        let Some(session_id) = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
        else {
            return Ok(None);
        };
        self.local_runtime
            .take_next_dispatchable(app, &session_id)
            .await
    }

    /// Mark the app's current session as running.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError::NoActiveSession`] when no session is active, or [`TuiRuntimeError`] on storage failure.
    pub async fn mark_running(&self, app: &App) -> Result<(), TuiRuntimeError> {
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        self.local_runtime.set_running(&session_id).await
    }

    /// Persist an ask-user request and surface it in the app.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn register_ask_user_request(
        &self,
        app: &mut App,
        session_id: &SessionId,
        questions: &[Question],
    ) -> Result<String, TuiRuntimeError> {
        self.local_runtime
            .register_ask_user_request(app, session_id, questions)
            .await
    }

    /// Record the user's answers and clear the pending request.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn resolve_ask_user_response(
        &self,
        app: &mut App,
        session_id: &SessionId,
        request_id: &str,
        answers: &[QuestionAnswer],
    ) -> Result<(), TuiRuntimeError> {
        self.local_runtime
            .resolve_ask_user_response(app, session_id, request_id, answers)
            .await
    }

    /// Persist a workspace-access request and surface it in the app.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn register_workspace_request(
        &self,
        app: &mut App,
        session_id: &SessionId,
        request: &crate::workspace_request::RequestWorkspaceArgs,
    ) -> Result<String, TuiRuntimeError> {
        self.local_runtime
            .register_workspace_request(app, session_id, request)
            .await
    }

    /// Record the workspace decision and clear the pending request.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn resolve_workspace_response(
        &self,
        app: &mut App,
        session_id: &SessionId,
        request_id: &str,
        approved: bool,
    ) -> Result<(), TuiRuntimeError> {
        self.local_runtime
            .resolve_workspace_response(app, session_id, request_id, approved)
            .await
    }

    /// Fetch the permission request blocking the session, if any.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn pending_permission(
        &self,
        app: &App,
    ) -> Result<Option<crate::session::PersistedPermissionRequest>, TuiRuntimeError> {
        let Some(session_id) = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
        else {
            return Ok(None);
        };
        self.local_runtime.waiting_permission(&session_id).await
    }

    /// Record the permission decision and clear the pending request.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn resolve_permission_response(
        &self,
        app: &mut App,
        request_id: &str,
        approved: bool,
    ) -> Result<(), TuiRuntimeError> {
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        self.local_runtime
            .resolve_permission_response(app, &session_id, request_id, approved)
            .await
    }

    fn spawn_title_generation(
        config: MayConfig,
        db: crate::db::ChatDb,
        session_id: Option<SessionId>,
        user_message: String,
        assistant_response: String,
    ) {
        let Some(session_id) = session_id else {
            tracing::warn!("Session missing during title generation");
            return;
        };

        spawn(async move {
            if let Some(title) = Box::pin(generate_session_title(
                &config,
                &user_message,
                &assistant_response,
            ))
            .await
                && let Err(e) = db
                    .update_session(session_id.as_str(), Some(&title), None, None)
                    .await
            {
                tracing::warn!("Failed to update session title in DB: {}", e);
            }
        })
        .detach();
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

fn prepare_message_content(
    msg: &PendingMessage,
) -> Result<(String, Vec<LlmAttachment>), TuiRuntimeError> {
    let mut parts = vec![msg.content.clone()];
    let mut attachment_urls = Vec::new();

    for attachment in &msg.attachments {
        match attachment {
            Attachment::Image { path, .. } => {
                let url = url::Url::from_file_path(path)
                    .map_err(|()| TuiRuntimeError::AttachmentPathToUrl { path: path.clone() })?;
                let media_type = mime_guess::from_path(path).first().ok_or_else(|| {
                    TuiRuntimeError::UnknownAttachmentMediaType { path: path.clone() }
                })?;
                attachment_urls.push(LlmAttachment::new(url, media_type));
            }
            Attachment::FileReference { path } => {
                parts.push(format!("\n[File reference: {}]", path.display()));
            }
            Attachment::PastedText {
                content,
                saved_path,
            } => {
                let xml = crate::xml::pasted_text_xml(content, saved_path.as_deref())
                    .map_err(TuiRuntimeError::SerializePasteXml)?;
                parts.push(format!("\n{xml}"));
            }
        }
    }

    Ok((parts.join(""), attachment_urls))
}

/// Builds a session's agent worker from owned inputs, off the event loop.
async fn build_worker_for_session(
    inputs: WorkerBuildInputs,
) -> Result<AgentWorker, TuiRuntimeError> {
    let WorkerBuildInputs {
        deps,
        config,
        db,
        history_messages,
        may_dir,
        cwd,
        profile_id,
        sandbox_dir,
        session_id,
        model,
        reasoning_effort,
    } = inputs;
    let session_id_string = session_id.as_str().to_string();
    let profile_model = match profile_id {
        Some(profile_id) => db.get_profile(&profile_id).await?,
        None => None,
    };
    let profile_persona = profile_model
        .as_ref()
        .and_then(|profile| profile.persona.clone());
    let profile_memories = if let Some(ref profile) = profile_model {
        let memories = crate::memory::ProfileMemories::load(&db, &profile.id).await?;
        if memories.is_empty() {
            None
        } else {
            Some(memories)
        }
    } else {
        None
    };
    let location = if profile_model
        .as_ref()
        .is_some_and(|profile| profile.location_aware)
    {
        Some(
            deps.location_provider
                .get()
                .await
                .map_err(TuiRuntimeError::LocationFetch)?,
        )
    } else {
        None
    };
    let profile_preferences = match profile_model.as_ref() {
        Some(profile) => db.load_profile_preferences(profile).await?,
        None => None,
    };
    let profile_environment = match profile_model.as_ref() {
        Some(profile) => Some(
            db.profile_info_from_model(profile.clone())
                .await?
                .environment
                .into_kind(),
        ),
        None => None,
    };
    let runtime = db
        .get_session_runtime(session_id.as_str())
        .await?
        .unwrap_or_default();
    let environment = Box::pin(crate::agent::resolve_agent_environment_for_session(
        profile_environment,
        profile_model.as_ref(),
        profile_preferences.and_then(|preferences| preferences.container),
        &may_dir,
        &sandbox_dir,
        &session_id,
        &deps.tool_distribution,
    ))
    .await
    .map_err(TuiRuntimeError::ResolveAgentEnvironment)?;
    let permission_approver = deps.local_runtime.permission_approver_for(&session_id);
    let mut agent = Box::pin(MayAgent::from_config_with_model_and_permissions(
        &config,
        crate::agent::MayAgentSessionInput {
            sandbox_dir,
            may_dir,
            cwd,
            session_id,
            workspace_request_broker: deps.workspace_broker.clone(),
            ask_user_broker: deps.ask_user_broker.clone(),
            db: db.clone(),
            model: &model,
            reasoning_effort,
            profile_id: profile_model.as_ref().map(|profile| profile.id.clone()),
            permission_approver: Some(permission_approver),
            profile_persona,
            profile_memories,
            plan_state: runtime.plan_state,
            location,
            task_broker: Some(deps.tool_dispatchers.task.clone()),
            notify_broker: Some(deps.tool_dispatchers.notify.clone()),
            browser_broker: deps.tool_dispatchers.browser.clone(),
            register_skills_broker: Some(deps.tool_dispatchers.register_skills.clone()),
            register_subagents_broker: Some(deps.tool_dispatchers.register_subagents.clone()),
            environment,
            tool_distribution: deps.tool_distribution.clone(),
            tool_environments: deps.tool_environments.clone(),
            provider_attempt: 0,
        },
    ))
    .await?;

    if let Some(snapshot) =
        crate::session::load_persisted_context_snapshot(&db, &session_id_string).await?
    {
        let db_messages = db.get_messages(&session_id_string).await?;
        let db_ids = crate::session::llm_row_ids(&db_messages);
        if let Some(delta) = crate::session::history_delta_since_snapshot(
            &history_messages,
            &db_ids,
            snapshot.last_message_db_id,
        ) {
            agent.restore_checkpoint(snapshot.checkpoint.clone())?;
            for message in delta {
                agent.push_message(message);
            }
        } else {
            tracing::warn!(
                session_id = %session_id_string,
                "context snapshot anchor is stale, falling back to full history replay"
            );
            let _ = db.delete_session_context_snapshot(&session_id_string).await;
            for message in history_messages {
                agent.push_message(message);
            }
        }
    } else {
        for message in history_messages {
            agent.push_message(message);
        }
    }

    Ok(spawn_worker(DefaultExecutor, agent, db))
}
