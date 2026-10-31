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
    /// A turn was started without holding the session's run lease.
    #[error("no session run lease is held for this turn")]
    NoRunLease,
    /// The May daemon refused or could not be reached.
    #[error(transparent)]
    Daemon(#[from] crate::client::DaemonClientError),
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
    /// Listening for messages from other agents failed.
    #[error("failed to listen for messages from other agents: {0}")]
    AgentLink(#[from] crate::agent_link::AgentLinkError),
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

    /// Start the next queued turn for a session: take its run lease first,
    /// and only then the message, so a session held elsewhere never has its
    /// queue touched.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn begin_queued(&self, session_id: &SessionId) -> Result<QueuedRun, TuiRuntimeError> {
        let runtime = self.runtime_service.get(session_id.as_str()).await?;
        if !runtime.has_pending_messages() || runtime.interactions.is_active() {
            return Ok(QueuedRun::Nothing);
        }
        let Some(guard) = self.acquire_run(session_id).await? else {
            return Ok(QueuedRun::Busy);
        };
        // An error drops the guard, which settles the session.
        let next = self
            .runtime_service
            .take_next_for_run(session_id.as_str())
            .await?;
        let Some(message) = next else {
            guard
                .finish_idle(crate::session::RunOutcome::Completed)
                .await?;
            return Ok(QueuedRun::Nothing);
        };
        Ok(QueuedRun::Started(
            pending_message_from_runtime(message).await?,
            Box::new(guard),
        ))
    }

    /// Take the session's run lease, or `None` while another run holds it.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn acquire_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<crate::session::SessionRunGuard>, TuiRuntimeError> {
        match self
            .runtime_service
            .runs()
            .acquire(session_id.as_str())
            .await
        {
            Ok(guard) => Ok(Some(guard)),
            Err(crate::session::AcquireError::Busy(_)) => Ok(None),
            Err(crate::session::AcquireError::Storage(error)) => Err(error.into()),
        }
    }

    /// Persist an ask-user request and surface it in the app.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn register_ask_user_request(
        &self,
        session_id: &SessionId,
        questions: &[Question],
    ) -> Result<String, TuiRuntimeError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        self.runtime_service
            .register_ask_user_request(
                session_id.as_str(),
                &crate::session::PersistedAskUserRequest {
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
                },
            )
            .await?;
        Ok(request_id)
    }

    /// Commit the user's answers to the prompt a run is blocked on, clearing
    /// it by its id. The caller wakes the run only on
    /// [`ResponseCommit::Committed`].
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the answers do not fit the prompt or
    /// on storage failure; nothing is committed then.
    ///
    /// # Panics
    /// Panics when a question reported as answered has no answers in the
    /// history: the commit writes both in one transaction, so that is a
    /// corrupted database.
    pub async fn resolve_ask_user_response(
        &self,
        session_id: &SessionId,
        request_id: &str,
        answers: &[QuestionAnswer],
    ) -> Result<ResponseCommit<Vec<QuestionAnswer>>, TuiRuntimeError> {
        match self
            .runtime_service
            .answer_ask_user(request_id, answers)
            .await?
        {
            Some(crate::session::AskUserAnswer::Committed(_)) => {}
            Some(crate::session::AskUserAnswer::AlreadyAnswered { .. }) => {
                let committed = self
                    .runtime_service
                    .committed_ask_user_answers(request_id)
                    .await?
                    .expect("an answered question has its committed answers in the history");
                return Ok(ResponseCommit::Lost(committed));
            }
            None => return Ok(ResponseCommit::Withdrawn),
        }
        self.runtime_service
            .set_state(
                session_id.as_str(),
                crate::session::SessionRuntimeState::Running,
            )
            .await?;
        Ok(ResponseCommit::Committed)
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

    /// Commit the workspace decision, clearing the pending request by its
    /// id. The caller wakes the run only on [`ResponseCommit::Committed`].
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure; nothing is committed
    /// then.
    pub async fn resolve_workspace_response(
        &self,
        session_id: &SessionId,
        request_id: &str,
        approved: bool,
    ) -> Result<ResponseCommit<bool>, TuiRuntimeError> {
        if self
            .runtime_service
            .commit_workspace_response(request_id, approved)
            .await?
            .is_none()
        {
            let committed = self
                .runtime_service
                .committed_workspace_decision(request_id)
                .await?;
            return Ok(committed.map_or(ResponseCommit::Withdrawn, ResponseCommit::Lost));
        }
        self.runtime_service
            .set_state(
                session_id.as_str(),
                crate::session::SessionRuntimeState::Running,
            )
            .await?;
        Ok(ResponseCommit::Committed)
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

    /// The open questions of a session, oldest first.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn open_questions(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::session::PersistedAskUserRequest>, TuiRuntimeError> {
        Ok(self
            .runtime_service
            .get(session_id.as_str())
            .await?
            .open_questions)
    }

    /// Commit the answers to an open question into the session queue.
    /// Returns whether this call committed them: `false` when the question
    /// is no longer open (withdrawn, or answered elsewhere).
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the answers do not fit the question
    /// or on storage failure.
    pub async fn answer_open_question(
        &self,
        request_id: &str,
        answers: &[QuestionAnswer],
    ) -> Result<bool, TuiRuntimeError> {
        Ok(matches!(
            self.runtime_service
                .answer_ask_user(request_id, answers)
                .await?,
            Some(crate::session::AskUserAnswer::Committed(_))
        ))
    }

    /// Record the permission decision and clear the pending request.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn resolve_permission_response(
        &self,
        session_id: &SessionId,
        request_id: &str,
        approved: bool,
    ) -> Result<(), TuiRuntimeError> {
        // Commit first (the request cleared by its id, the decision in the
        // history), then wake the run.
        let Some(resolved) = self
            .permission_approver
            .resolve(request_id, approved)
            .await?
        else {
            tracing::warn!(request_id, "permission response for a request not pending");
            return Ok(());
        };
        self.runtime_service
            .set_state(
                session_id.as_str(),
                if resolved.woke {
                    crate::session::SessionRuntimeState::Running
                } else {
                    crate::session::SessionRuntimeState::Idle
                },
            )
            .await?;
        if !resolved.woke {
            tracing::warn!(
                request_id,
                "permission response resolved without a live waiter"
            );
        }
        Ok(())
    }
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
    workspace_broker: WorkspaceRequestBroker,
    ask_user_broker: AskUserBroker,
    tool_dispatchers: ToolDispatchers,
    location_provider: Arc<crate::location::LocationProvider>,
    local_runtime: LocalSessionRuntime,
    tool_distribution: Arc<aither_sandbox::ToolDistribution>,
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

        if self.daemon.is_some() {
            return Self::send_remote_turn(app, worker, msg);
        }

        let guard = app.run_guard.as_ref().ok_or(TuiRuntimeError::NoRunLease)?;
        let fence = guard.fence().clone();
        let lease = crate::agent::RunLease::of(guard);

        if let Some(hidden_context_xml) = msg.hidden_context_xml.as_ref() {
            app.session_manager
                .append_message_as_run(
                    &fence,
                    ChatMessage::hidden_context(hidden_context_xml.to_string()),
                )
                .await?;
        }

        let persisted_attachments = msg
            .attachments
            .iter()
            .map(persisted_attachment_from_tui)
            .collect::<Result<Vec<_>, _>>()?;
        let user_message_id = app
            .session_manager
            .append_message_as_run(
                &fence,
                ChatMessage::user_with_attachments(msg.content.clone(), persisted_attachments),
            )
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
            lease,
        ))
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
        assistant_response: &AssistantReply,
        outcome: crate::session::RunOutcome,
    ) {
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone());
        *agent_handle = None;
        app.finish_processing();

        if self.daemon.is_some() {
            // The daemon persisted the turn and names the session.
            if let Err(error) = app.session_manager.refresh_history().await {
                tracing::warn!(%error, "failed to reload the session history");
            }
            title_gen.take();
            if let Some(session_id) = session_id.as_ref()
                && let Err(error) = self.local_runtime.sync_app_queue(app, session_id).await
            {
                tracing::warn!(session_id = %session_id, error = %error, "failed to refresh the pending queue");
            }
            return;
        }

        if let Some(at) = assistant_response.at() {
            if let Some(fence) = app.run_guard.as_ref().map(|guard| guard.fence().clone()) {
                if let Err(error) = app
                    .session_manager
                    .append_message_as_run(
                        &fence,
                        ChatMessage::assistant(assistant_response.text(), at),
                    )
                    .await
                {
                    tracing::warn!(error = %error, "failed to persist assistant message");
                }
            } else {
                tracing::warn!("an assistant reply ended without a held session run");
            }
        }

        if let Err(e) = app.session_manager.save_current().await {
            tracing::warn!("Failed to save session history: {}", e);
        }
        if let Some(guard) = app.run_guard.take()
            && let Err(error) = guard.finish_idle(outcome).await
        {
            tracing::warn!(error = %error, "failed to release the session run");
        }
        if let Some(session_id) = session_id.as_ref()
            && let Err(error) = self.local_runtime.sync_app_queue(app, session_id).await
        {
            tracing::warn!(session_id = %session_id, error = %error, "failed to refresh local pending queue");
        }

        if let Some(ctx) = title_gen.take()
            && !assistant_response.text().is_empty()
        {
            Self::spawn_title_generation(
                app.config.clone(),
                app.session_manager.db().clone(),
                app.session_manager.current().map(|s| s.id.clone()),
                ctx.user_message,
                assistant_response.text().to_string(),
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
        let db = app.session_manager.db().clone();
        let cwd = app.cwd.display().to_string();
        let model_id = model.as_str().to_string();
        let app_events = app.event_tx.clone();
        let task = spawn(async move {
            let effort = reasoning_effort.map(|effort| match effort {
                ReasoningEffort::Low => "low",
                ReasoningEffort::Medium => "medium",
                _ => "high",
            });
            db.update_session(
                session_id.as_str(),
                None,
                Some(model_id.as_str()),
                effort.map(Some),
            )
            .await?;
            db.set_session_cwd(session_id.as_str(), &cwd).await?;
            if let Some(worktree) = crate::repo_search::worktree_root(std::path::Path::new(&cwd)) {
                let _ = db.touch_repo_usage(&worktree).await;
            }
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
        sandbox_dir: std::path::PathBuf,
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
            settings: crate::agent::AgentLiveSettings::of(&app.config),
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
            return self.local_runtime.sync_app_queue(app, &session_id).await;
        }
        self.local_runtime
            .enqueue_pending_message(app, &session_id, message)
            .await
    }

    /// Keep the app's current session discoverable by other main agents on
    /// this machine, and drop the listeners of sessions the app left. A message
    /// stored for the session wakes the event loop, which then dispatches it.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError::AgentLink`] when the socket cannot be bound.
    pub async fn listen_for_agent_messages(
        &self,
        app: &App,
        listeners: &crate::agent_link::AgentListeners,
        wake: crate::tui::event::EventSender,
    ) -> Result<(), TuiRuntimeError> {
        // The daemon makes every session discoverable itself.
        if self.daemon.is_some() {
            return Ok(());
        }
        let Some(session) = app.session_manager.current() else {
            return Ok(());
        };
        listeners.retain_only(session.id.as_str());
        listeners
            .ensure(
                &crate::session::MayHomePaths::new(app.session_manager.may_dir().to_path_buf())
                    .agents_dir(),
                crate::agent_link::AgentIdentity {
                    session_id: session.id.as_str().to_string(),
                    cwd: app.cwd.clone(),
                    sandbox_dir: session.sandbox_dir.clone(),
                },
                self.local_runtime.runtime_service.clone(),
                move || {
                    // The loop is parked on its next event; any event ends the park.
                    if let Err(error) = wake.try_send(crate::tui::event::AppEvent::Tick) {
                        tracing::warn!(%error, "failed to wake the event loop for an agent message");
                    }
                },
            )
            .await?;
        Ok(())
    }

    /// Start the next queued turn; the app holds the lease until
    /// [`Self::finish_agent_task`]. Returns the message to run, or why none
    /// started.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn begin_queued_run(&self, app: &mut App) -> Result<QueuedRunStart, TuiRuntimeError> {
        // The daemon dispatches the queue.
        if self.daemon.is_some() {
            return Ok(QueuedRunStart::Nothing);
        }
        let Some(session_id) = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
        else {
            return Ok(QueuedRunStart::Nothing);
        };
        match self.local_runtime.begin_queued(&session_id).await? {
            QueuedRun::Nothing => Ok(QueuedRunStart::Nothing),
            QueuedRun::Busy => Ok(QueuedRunStart::Busy),
            QueuedRun::Started(message, guard) => {
                app.run_guard = Some(*guard);
                self.local_runtime.sync_app_queue(app, &session_id).await?;
                Ok(QueuedRunStart::Started(message))
            }
        }
    }

    /// Take the run lease of the app's current session for the turn about to
    /// start; the app holds it until [`Self::finish_agent_task`]. Returns
    /// `false`, holding nothing, while another run (a web server or a second
    /// TUI process on the same database) holds the session.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError::NoActiveSession`] when no session is active, or [`TuiRuntimeError`] on storage failure.
    pub async fn begin_run(&self, app: &mut App) -> Result<bool, TuiRuntimeError> {
        // The daemon takes the run lease when the turn starts there.
        if self.daemon.is_some() {
            return Ok(true);
        }
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        let Some(guard) = self.local_runtime.acquire_run(&session_id).await? else {
            return Ok(false);
        };
        app.run_guard = Some(guard);
        Ok(true)
    }

    /// Persist an ask-user request and surface it in the app.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn register_ask_user_request(
        &self,
        session_id: &SessionId,
        questions: &[Question],
    ) -> Result<String, TuiRuntimeError> {
        self.local_runtime
            .register_ask_user_request(session_id, questions)
            .await
    }

    /// The open questions of the current session, oldest first.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn open_questions(
        &self,
        app: &App,
    ) -> Result<Vec<crate::session::PersistedAskUserRequest>, TuiRuntimeError> {
        let Some(session_id) = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
        else {
            return Ok(Vec::new());
        };
        self.local_runtime.open_questions(&session_id).await
    }

    /// Push-notify the user that a new open question arrived, through the
    /// same `notify` dispatch path the `notify` command uses. Best effort:
    /// a rejected or unroutable notification is logged, never fatal.
    pub async fn notify_open_question(&self, session_id: &SessionId, question: &str) {
        match self
            .tool_dispatchers
            .notify
            .request(crate::agent::NotifyArgs {
                title: "May has a question for you".to_string(),
                body: question.to_string(),
                session_id: Some(session_id.as_str().to_string()),
                callback_context_xml: None,
                callback_message: None,
            })
            .await
        {
            Ok(Ok(result)) => {
                tracing::debug!(sent = result.sent, "open-question notification delivered");
            }
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "open-question notification was rejected");
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
        self.local_runtime
            .answer_open_question(request_id, answers)
            .await
    }

    /// Commit the user's answers to the prompt a run is blocked on; the
    /// caller wakes the run afterwards, only if this call committed.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the answers do not fit the prompt or
    /// on storage failure.
    pub async fn resolve_ask_user_response(
        &self,
        session_id: &SessionId,
        request_id: &str,
        answers: &[QuestionAnswer],
    ) -> Result<ResponseCommit<Vec<QuestionAnswer>>, TuiRuntimeError> {
        self.local_runtime
            .resolve_ask_user_response(session_id, request_id, answers)
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

    /// Commit the workspace decision; the caller wakes the run afterwards,
    /// only if this call committed.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn resolve_workspace_response(
        &self,
        session_id: &SessionId,
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
        self.local_runtime
            .resolve_workspace_response(session_id, request_id, approved)
            .await
    }

    /// The workspace request the session's run waits on, if any.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn pending_workspace(
        &self,
        app: &App,
    ) -> Result<Option<crate::session::PersistedWorkspaceRequest>, TuiRuntimeError> {
        let Some(session_id) = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
        else {
            return Ok(None);
        };
        Ok(self
            .local_runtime
            .runtime_service
            .get(session_id.as_str())
            .await?
            .waiting_workspace)
    }

    /// The blocking question the session's run waits on, if any.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn pending_ask_user(
        &self,
        app: &App,
    ) -> Result<Option<crate::session::PersistedAskUserRequest>, TuiRuntimeError> {
        let Some(session_id) = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
        else {
            return Ok(None);
        };
        Ok(self
            .local_runtime
            .runtime_service
            .get(session_id.as_str())
            .await?
            .waiting_ask_user)
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
        app: &App,
        request_id: &str,
        approved: bool,
    ) -> Result<(), TuiRuntimeError> {
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
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
        self.local_runtime
            .resolve_permission_response(&session_id, request_id, approved)
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
                let block = crate::pasted_text::pasted_text_block(content, saved_path.as_deref());
                parts.push(format!("\n{block}"));
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
            subagents: None,
            background_jobs: aither_agent::BackgroundJobPolicy::Await,
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

    Ok(spawn_worker(
        DefaultExecutor,
        agent,
        db,
        deps.local_runtime.runtime_service.runs().clone(),
    ))
}

#[cfg(test)]
mod tests {
    use aither_sandbox::TerminalMode;

    use super::{LocalSessionRuntime, QueuedRun, ResponseCommit};
    use crate::SessionId;
    use crate::ask_user::{AnswerValue, QuestionAnswer};
    use crate::db::ChatDb;
    use crate::session::{
        PendingUserMessage, PendingUserMessageMode, PersistedAskUserQuestion,
        PersistedAskUserRequest, PersistedWorkspaceRequest, PersistedWorkspaceRequestPayload,
        RunOutcome, SessionRuntimeService,
    };

    fn queued(content: &str) -> PendingUserMessage {
        PendingUserMessage {
            content: content.to_string(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            created_at: jiff::Timestamp::now().to_string(),
            delivery_mode: PendingUserMessageMode::Queue,
        }
    }

    async fn contents(service: &SessionRuntimeService, session_id: &str) -> Vec<String> {
        service
            .get(session_id)
            .await
            .expect("runtime")
            .queued_messages
            .into_iter()
            .map(|message| message.content)
            .collect()
    }

    /// While another holder runs the session, starting the next queued turn
    /// leaves the queue exactly as it was; once the holder finishes, the
    /// messages dispatch in their original order.
    #[tokio::test]
    async fn busy_session_keeps_queue_order_and_untouched_until_the_holder_finishes() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        let db = ChatDb::open(&path).await.expect("open");
        db.create_session("session", Some("Session"), Some("test-model"))
            .await
            .expect("create session");
        let local = LocalSessionRuntime::new(db);
        let other = SessionRuntimeService::new(
            ChatDb::open(&path).await.expect("open other handle"),
            crate::ui_event_bus::UiEventBus::new(),
        );
        let session_id = SessionId::from_string("session");

        for content in ["first", "second"] {
            local
                .runtime_service
                .enqueue("session", queued(content), PendingUserMessageMode::Queue)
                .await
                .expect("enqueue");
        }
        let before = local.runtime_service.get("session").await.expect("runtime");

        let holder = other.runs().acquire("session").await.expect("holder");
        for _ in 0..3 {
            let outcome = local.begin_queued(&session_id).await.expect("begin");
            assert!(matches!(outcome, QueuedRun::Busy), "got {outcome:?}");
        }
        let during = local.runtime_service.get("session").await.expect("runtime");
        assert_eq!(
            during.queued_messages, before.queued_messages,
            "the queue must not be rewritten while another run holds the session"
        );
        assert_eq!(
            contents(&local.runtime_service, "session").await,
            ["first", "second"]
        );

        holder
            .finish_idle(RunOutcome::Completed)
            .await
            .expect("holder ends");

        for expected in ["first", "second"] {
            let QueuedRun::Started(message, guard) =
                local.begin_queued(&session_id).await.expect("begin")
            else {
                panic!("expected {expected:?} to start");
            };
            assert_eq!(message.content, expected);
            (*guard)
                .finish_idle(RunOutcome::Completed)
                .await
                .expect("turn ends");
        }
        let QueuedRun::Nothing = local.begin_queued(&session_id).await.expect("begin") else {
            panic!("an empty queue must start nothing");
        };
        assert!(
            !local.runtime_service.runs().is_held("session"),
            "an empty queue must leave the run released"
        );
    }

    /// A message the user types while another run holds the session joins
    /// the queue behind the earlier ones.
    #[tokio::test]
    async fn message_typed_while_busy_is_queued_in_order() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        let db = ChatDb::open(&path).await.expect("open");
        db.create_session("session", Some("Session"), Some("test-model"))
            .await
            .expect("create session");
        let local = LocalSessionRuntime::new(db);
        let other = SessionRuntimeService::new(
            ChatDb::open(&path).await.expect("open other handle"),
            crate::ui_event_bus::UiEventBus::new(),
        );
        let session_id = SessionId::from_string("session");

        local
            .runtime_service
            .enqueue("session", queued("earlier"), PendingUserMessageMode::Queue)
            .await
            .expect("enqueue");
        let holder = other.runs().acquire("session").await.expect("holder");
        assert!(
            local
                .acquire_run(&session_id)
                .await
                .expect("acquire")
                .is_none()
        );
        local
            .runtime_service
            .enqueue("session", queued("typed"), PendingUserMessageMode::Queue)
            .await
            .expect("enqueue");
        assert_eq!(
            contents(&local.runtime_service, "session").await,
            ["earlier", "typed"]
        );
        holder
            .finish_idle(RunOutcome::Completed)
            .await
            .expect("holder ends");
    }

    async fn local_session() -> (tempfile::TempDir, LocalSessionRuntime, SessionId) {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let dir = tempfile::tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("open");
        db.create_session("session", Some("Session"), Some("test-model"))
            .await
            .expect("create session");
        (
            dir,
            LocalSessionRuntime::new(db),
            SessionId::from_string("session"),
        )
    }

    async fn bounded<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(20), future)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    fn workspace_request(id: &str) -> PersistedWorkspaceRequest {
        PersistedWorkspaceRequest {
            id: id.to_string(),
            request: PersistedWorkspaceRequestPayload {
                path: "/srv/data".to_string(),
                reason: "read the dataset".to_string(),
                session_id: Some("session".to_string()),
            },
        }
    }

    fn ssh_prompt(id: &str) -> PersistedAskUserRequest {
        PersistedAskUserRequest {
            id: id.to_string(),
            questions: vec![PersistedAskUserQuestion {
                section: String::new(),
                question: "Allow SSH?".to_string(),
                option: vec!["Allow".to_string(), "Deny".to_string()],
                multi_select: false,
            }],
        }
    }

    fn allow() -> Vec<QuestionAnswer> {
        vec![QuestionAnswer {
            section: String::new(),
            answer: AnswerValue::Selected(vec!["Allow".to_string()]),
        }]
    }

    async fn responses(local: &LocalSessionRuntime, role: &str) -> usize {
        local
            .runtime_service
            .db()
            .get_messages_by_role("session", role)
            .await
            .expect("history loads")
            .len()
    }

    /// Start a run waiting on a permission decision; returns the pending
    /// request id and the waiter, which reports what it saw on waking.
    async fn wait_for_permission(
        local: &LocalSessionRuntime,
        session_id: &SessionId,
    ) -> (
        String,
        tokio::task::JoinHandle<(bool, Option<String>, usize)>,
    ) {
        let permissions = local.permission_approver_for(session_id);
        let service = local.runtime_service.clone();
        let waiter = tokio::spawn(async move {
            let allowed = permissions
                .request_and_wait(TerminalMode::Unsafe, "make install".to_string())
                .await
                .expect("the waiter is woken with a decision");
            let runtime = service.get("session").await.expect("runtime");
            let responses = service
                .db()
                .get_messages_by_role("session", "permission_response")
                .await
                .expect("history loads")
                .len();
            (
                allowed,
                runtime.waiting_permission.map(|request| request.id),
                responses,
            )
        });
        let id = bounded("the permission request to show", async {
            loop {
                if let Some(request) = local
                    .runtime_service
                    .get("session")
                    .await
                    .expect("runtime")
                    .waiting_permission
                {
                    return request.id;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        (id, waiter)
    }

    /// The TUI commits a permission decision before it wakes the run, and
    /// clears the request it answered by its id: a stale decision leaves a
    /// newer request pending.
    #[tokio::test]
    async fn a_permission_response_commits_before_it_wakes_the_run_and_clears_by_id() {
        let (_dir, local, session_id) = local_session().await;
        let (older, first) = wait_for_permission(&local, &session_id).await;
        local
            .resolve_permission_response(&session_id, &older, true)
            .await
            .expect("the decision commits");
        let (approved, pending, recorded) = bounded("the first run to wake", first)
            .await
            .expect("the waiter finishes");
        assert!(approved);
        assert_eq!(pending, None, "the request was cleared before the run woke");
        assert_eq!(
            recorded, 1,
            "the decision was in the history before the run woke"
        );

        let (newer, second) = wait_for_permission(&local, &session_id).await;
        local
            .resolve_permission_response(&session_id, &older, true)
            .await
            .expect("a stale decision is not a failure");
        let still = local
            .runtime_service
            .get("session")
            .await
            .expect("runtime")
            .waiting_permission
            .map(|request| request.id);
        assert_eq!(
            still,
            Some(newer.clone()),
            "the newer request stays pending"
        );
        assert!(
            !second.is_finished(),
            "the stale decision woke the newer run"
        );
        local
            .resolve_permission_response(&session_id, &newer, false)
            .await
            .expect("the decision commits");
        let (approved, ..) = bounded("the second run to wake", second)
            .await
            .expect("the waiter finishes");
        assert!(!approved);
    }

    /// A permission decision whose commit fails wakes nobody and leaves the
    /// request pending.
    #[tokio::test]
    async fn a_failed_permission_commit_wakes_nobody() {
        let (_dir, local, session_id) = local_session().await;
        let (id, waiter) = wait_for_permission(&local, &session_id).await;
        local
            .runtime_service
            .db()
            .execute_for_test("CREATE TRIGGER fail_response_history BEFORE INSERT ON message_permission_responses BEGIN SELECT RAISE(ABORT, 'disk full'); END")
            .await
            .expect("break the history writes");
        assert!(
            local
                .resolve_permission_response(&session_id, &id, true)
                .await
                .is_err()
        );
        assert!(!waiter.is_finished(), "the failed decision woke the run");
        assert_eq!(
            local
                .runtime_service
                .get("session")
                .await
                .expect("runtime")
                .waiting_permission
                .map(|request| request.id),
            Some(id)
        );
        waiter.abort();
    }

    /// A workspace decision commits (the request cleared by its id, the
    /// decision in the history) when the call returns, which is before the
    /// caller wakes the run; a stale one leaves a newer request pending, and
    /// one whose commit fails returns the error so nothing is woken.
    #[tokio::test]
    async fn a_workspace_response_commits_by_id_before_the_caller_wakes_the_run() {
        let (_dir, local, session_id) = local_session().await;
        let service = &local.runtime_service;
        service
            .set_waiting_workspace("session", Some(workspace_request("older")))
            .await
            .expect("older request");
        assert!(matches!(
            local
                .resolve_workspace_response(&session_id, "older", true)
                .await
                .expect("the decision commits"),
            ResponseCommit::Committed
        ));
        let runtime = service.get("session").await.expect("runtime");
        assert!(runtime.waiting_workspace.is_none());
        assert_eq!(responses(&local, "workspace_response").await, 1);

        service
            .set_waiting_workspace("session", Some(workspace_request("newer")))
            .await
            .expect("newer request");
        assert!(
            matches!(
                local
                    .resolve_workspace_response(&session_id, "older", false)
                    .await
                    .expect("a stale decision is not a failure"),
                ResponseCommit::Lost(true)
            ),
            "a decision that lost the commit carries the committed decision, not its own"
        );
        assert!(
            matches!(
                local
                    .resolve_workspace_response(&session_id, "never-asked", true)
                    .await
                    .expect("a withdrawn request is not a failure"),
                ResponseCommit::Withdrawn
            ),
            "a request nobody committed has no decision to carry"
        );
        assert_eq!(
            service
                .get("session")
                .await
                .expect("runtime")
                .waiting_workspace
                .map(|request| request.id),
            Some("newer".to_string()),
            "the newer request stays pending"
        );
        assert_eq!(responses(&local, "workspace_response").await, 1);

        service
            .db()
            .execute_for_test("CREATE TRIGGER fail_response_history BEFORE INSERT ON message_workspace_responses BEGIN SELECT RAISE(ABORT, 'disk full'); END")
            .await
            .expect("break the history writes");
        assert!(
            local
                .resolve_workspace_response(&session_id, "newer", true)
                .await
                .is_err(),
            "a failed commit is returned, so the caller wakes nobody"
        );
        assert!(
            service
                .get("session")
                .await
                .expect("runtime")
                .waiting_workspace
                .is_some(),
            "nothing was committed"
        );
    }

    /// The same for the prompt a run blocks on.
    #[tokio::test]
    async fn an_ask_user_response_commits_by_id_before_the_caller_wakes_the_run() {
        let (_dir, local, session_id) = local_session().await;
        let service = &local.runtime_service;
        service
            .set_waiting_ask_user("session", Some(ssh_prompt("older")))
            .await
            .expect("older prompt");
        assert!(matches!(
            local
                .resolve_ask_user_response(&session_id, "older", &allow())
                .await
                .expect("the answer commits"),
            ResponseCommit::Committed
        ));
        assert!(
            service
                .get("session")
                .await
                .expect("runtime")
                .waiting_ask_user
                .is_none()
        );
        assert_eq!(responses(&local, "ask_user_response").await, 1);

        service
            .set_waiting_ask_user("session", Some(ssh_prompt("newer")))
            .await
            .expect("newer prompt");
        let deny = vec![QuestionAnswer {
            section: String::new(),
            answer: AnswerValue::Selected(vec!["Deny".to_string()]),
        }];
        let ResponseCommit::Lost(committed) = local
            .resolve_ask_user_response(&session_id, "older", &deny)
            .await
            .expect("a stale answer is not a failure")
        else {
            panic!("an answer that lost the commit must report the committed answers");
        };
        assert!(
            matches!(
                committed.as_slice(),
                [QuestionAnswer { answer: AnswerValue::Selected(values), .. }]
                    if values == &["Allow".to_string()]
            ),
            "the live run is answered with the committed answer, not this process's own: {committed:?}"
        );
        assert!(
            matches!(
                local
                    .resolve_ask_user_response(&session_id, "never-asked", &allow())
                    .await
                    .expect("a withdrawn prompt is not a failure"),
                ResponseCommit::Withdrawn
            ),
            "a prompt nobody committed has no answers to carry"
        );
        assert_eq!(
            service
                .get("session")
                .await
                .expect("runtime")
                .waiting_ask_user
                .map(|request| request.id),
            Some("newer".to_string()),
            "the newer prompt stays pending"
        );
        assert_eq!(responses(&local, "ask_user_response").await, 1);

        service
            .db()
            .execute_for_test("CREATE TRIGGER fail_response_history BEFORE INSERT ON message_ask_user_responses BEGIN SELECT RAISE(ABORT, 'disk full'); END")
            .await
            .expect("break the history writes");
        assert!(
            local
                .resolve_ask_user_response(&session_id, "newer", &allow())
                .await
                .is_err(),
            "a failed commit is returned, so the caller wakes nobody"
        );
    }

    /// Answering an open question that was withdrawn meanwhile commits
    /// nothing and says so, instead of dropping the answer silently.
    #[tokio::test]
    async fn answering_a_withdrawn_open_question_reports_it_is_gone() {
        let (_dir, local, _session_id) = local_session().await;
        let service = &local.runtime_service;
        service
            .open_question("session", &ssh_prompt("open-1"))
            .await
            .expect("the question opens");
        assert!(
            service
                .withdraw_question("session", "open-1")
                .await
                .expect("withdrawn")
        );
        let committed = local
            .answer_open_question("open-1", &allow())
            .await
            .expect("not a failure");
        assert!(!committed, "the answer to a withdrawn question is not sent");
    }
}
