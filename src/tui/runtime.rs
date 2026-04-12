use std::sync::Arc;

use aither_core::llm::model::ReasoningEffort;
use async_channel::Sender;
use base64::Engine;
use executor_core::{DefaultExecutor, spawn};
use serde::Serialize;

use crate::agent::{AgentHandle, AgentWorker, spawn_worker};
use crate::ask_user::{AskUserBroker, Question, QuestionAnswer};
use crate::session::AttachmentKind;
use crate::tool_dispatch::ToolDispatchers;
use crate::tui::queue::{LARGE_PASTE_THRESHOLD, PASTE_TRUNCATE_LIMIT};
use crate::tui::{App, Attachment, PendingMessage};
use crate::workspace_request::WorkspaceRequestBroker;
use crate::{ChatDb, ChatMessage, MayAgent, MayConfig, SessionId, generate_session_title};

#[derive(Debug, thiserror::Error)]
pub enum TuiRuntimeError {
    #[error("database error: {0}")]
    Database(#[from] crate::db::ChatDbError),
    #[error("session error: {0}")]
    Session(#[from] crate::session::SessionError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("agent environment resolution failed: {0}")]
    ResolveAgentEnvironment(#[from] crate::agent::ResolveAgentEnvironmentError),
    #[error("No active session")]
    NoActiveSession,
    #[error("history replay failed: {0}")]
    HistoryReplay(#[from] crate::session::HistoryReplayError),
    #[error("invalid pending message timestamp: {0}")]
    InvalidPendingMessageTimestamp(#[source] jiff::Error),
    #[error("persisted image attachment is missing content")]
    PersistedImageMissingContent,
    #[error("unsupported persisted attachment type in TUI runtime: {0}")]
    UnsupportedPersistedAttachmentType(AttachmentKind),
    #[error("failed to convert path into file URL: {path}")]
    FileUrlConversion { path: std::path::PathBuf },
    #[error("failed to parse attachment URL {url}: {source}")]
    AttachmentUrlParse {
        url: String,
        #[source]
        source: url::ParseError,
    },
    #[error("attachment URL is not a local file URL: {url}")]
    NonLocalAttachmentUrl { url: String },
    #[error("Selected model is empty")]
    EmptySelectedModel,
    #[error("No model configured")]
    NoModelConfigured,
    #[error("Location fetch failed: {0}")]
    LocationFetch(#[source] crate::location::LocationError),
    #[error("agent initialization failed: {0}")]
    MayAgent(#[from] crate::agent::MayAgentError),
    #[error("agent operation failed: {0}")]
    Agent(#[from] aither_agent::AgentError),
    #[error("context snapshot history delta failed: {0}")]
    HistoryDelta(#[from] crate::session::HistoryDeltaError),
    #[error("Failed to convert attachment path to URL: {path}")]
    AttachmentPathToUrl { path: std::path::PathBuf },
    #[error("Failed to serialize paste XML: {0}")]
    SerializePasteXml(#[source] quick_xml::SeError),
    #[error("invalid data URL")]
    InvalidDataUrl,
    #[error("failed to decode data URL: {0}")]
    DecodeDataUrl(#[source] base64::DecodeError),
}

#[derive(Debug, Clone)]
pub struct LocalSessionRuntime {
    runtime_service: Arc<crate::session::SessionRuntimeService>,
    permission_approver: crate::agent::PermissionApprover,
    oneshot_restricted: bool,
}

impl LocalSessionRuntime {
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

    pub const fn enable_oneshot_restricted_mode(&mut self) {
        self.oneshot_restricted = true;
    }

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

    pub async fn sync_app_queue(
        &self,
        app: &mut App,
        session_id: &SessionId,
    ) -> Result<(), TuiRuntimeError> {
        let runtime = self.runtime_service.get(session_id.as_str()).await?;
        self.replace_app_queue(app, runtime).await
    }

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

    pub async fn set_running(&self, session_id: &SessionId) -> Result<(), TuiRuntimeError> {
        self.runtime_service
            .set_state(
                session_id.as_str(),
                crate::session::SessionRuntimeState::Running,
            )
            .await?;
        Ok(())
    }

    pub async fn clear_running_if_needed(
        &self,
        session_id: &SessionId,
    ) -> Result<(), TuiRuntimeError> {
        self.runtime_service
            .clear_running_if_needed(session_id.as_str())
            .await?;
        Ok(())
    }

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

async fn tui_attachment_from_persisted(
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
    oneshot_model: Option<String>,
    oneshot_reasoning_effort: Option<ReasoningEffort>,
}

impl RuntimeDeps {
    pub const fn new(
        local_runtime: LocalSessionRuntime,
        workspace_broker: WorkspaceRequestBroker,
        ask_user_broker: AskUserBroker,
        tool_dispatchers: ToolDispatchers,
        location_provider: Arc<crate::location::LocationProvider>,
    ) -> Self {
        Self {
            workspace_broker,
            ask_user_broker,
            tool_dispatchers,
            location_provider,
            local_runtime,
            oneshot_model: None,
            oneshot_reasoning_effort: None,
        }
    }

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

    pub async fn start_new_session(
        &self,
        app: &mut App,
        worker: &mut Option<AgentWorker>,
    ) -> Result<(), TuiRuntimeError> {
        let model = self
            .oneshot_model
            .clone()
            .or_else(|| app.config.last_model.clone());
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
        app.init_session(&session_id, &messages, model.as_deref());
        self.local_runtime.sync_app_queue(app, &session_id).await?;

        let selected_model = SelectedModel::resolve(&app.config, model.as_deref())?;
        let new_worker = self
            .build_worker_for_session(
                app,
                sandbox_dir,
                session_id,
                selected_model.as_str(),
                self.oneshot_reasoning_effort,
            )
            .await?;

        app.set_current_model(selected_model.as_str());
        *worker = Some(new_worker);
        Ok(())
    }

    pub async fn load_session(
        &self,
        app: &mut App,
        worker: &mut Option<AgentWorker>,
        id: &SessionId,
    ) -> Result<(), TuiRuntimeError> {
        let session = app.session_manager.load_session(id).await?;

        let sandbox_dir = session.sandbox_dir.clone();
        let session_id = session.id.clone();
        let session_model = session.model.clone();
        let messages: Vec<_> = session.history.messages().to_vec();

        app.init_session(&session_id, &messages, session_model.as_deref());
        self.local_runtime.sync_app_queue(app, &session_id).await?;

        let selected_model = SelectedModel::resolve(&app.config, session_model.as_deref())?;
        let new_worker = self
            .build_worker_for_session(app, sandbox_dir, session_id, selected_model.as_str(), None)
            .await?;

        app.set_current_model(selected_model.as_str());
        *worker = Some(new_worker);
        Ok(())
    }

    pub async fn switch_model(
        &self,
        app: &mut App,
        worker: &mut Option<AgentWorker>,
        model: &str,
    ) -> Result<(), TuiRuntimeError> {
        let session = app
            .session_manager
            .current()
            .ok_or(TuiRuntimeError::NoActiveSession)?;

        let sandbox_dir = session.sandbox_dir.clone();
        let session_id = session.id.clone();

        let selected_model = SelectedModel::new(model)?;
        let new_worker = self
            .build_worker_for_session(app, sandbox_dir, session_id, selected_model.as_str(), None)
            .await?;

        app.set_current_model(selected_model.as_str());
        *worker = Some(new_worker);
        Ok(())
    }

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

        app.session_manager
            .append_message(ChatMessage::user(&msg.content))
            .await?;

        let (prompt, image_urls) = prepare_message_content(&msg)?;
        app.start_processing(&msg);
        Ok(worker.run(
            prompt,
            msg.hidden_context_xml.as_ref().map(ToString::to_string),
            image_urls,
        ))
    }

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
            self.spawn_title_generation(
                app.config.clone(),
                app.session_manager.db().clone(),
                app.session_manager.current().map(|s| s.id.clone()),
                ctx.user_message,
                assistant_response.to_string(),
            );
        }
    }

    async fn build_worker_for_session(
        &self,
        app: &App,
        sandbox_dir: std::path::PathBuf,
        session_id: SessionId,
        model: &str,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<AgentWorker, TuiRuntimeError> {
        let db = app.session_manager.db().clone();
        let session_id_string = session_id.as_str().to_string();
        let history_messages = app
            .session_manager
            .current()
            .map(|session| session.history.to_aither_messages())
            .transpose()
            .map_err(|e| TuiRuntimeError::HistoryReplay(e))?
            .unwrap_or_default();
        let may_dir = app.session_manager.may_dir().to_path_buf();
        let profile_model = if let Some(profile_id) = app
            .session_manager
            .current()
            .and_then(|session| session.profile_id.clone())
        {
            app.session_manager.db().get_profile(&profile_id).await?
        } else {
            None
        };
        let profile_persona = profile_model
            .as_ref()
            .and_then(|profile| profile.persona.clone());
        let profile_memories = if let Some(ref profile) = profile_model {
            let memories =
                crate::memory::ProfileMemories::load(app.session_manager.db(), &profile.id).await?;
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
                self.location_provider
                    .get()
                    .await
                    .map_err(TuiRuntimeError::LocationFetch)?,
            )
        } else {
            None
        };
        let profile_preferences = match profile_model.as_ref() {
            Some(profile) => {
                app.session_manager
                    .db()
                    .load_profile_preferences(profile)
                    .await?
            }
            None => None,
        };
        let profile_environment = match (profile_model.clone(), profile_preferences.clone()) {
            (Some(profile), preferences) => Some(
                crate::profile::ProfileInfo::from_db_parts(profile, preferences)?
                    .environment
                    .into_kind(),
            ),
            (None, None) => None,
            (None, Some(_)) => panic!("profile preferences cannot exist without a profile model"),
        };
        let runtime = app
            .session_manager
            .db()
            .get_session_runtime(session_id.as_str())
            .await?
            .unwrap_or_default();
        let environment = resolve_agent_environment_for_tui(
            profile_environment,
            profile_model.as_ref(),
            profile_preferences.and_then(|preferences| preferences.container),
            &may_dir,
            &sandbox_dir,
            &session_id,
        )
        .await?;
        let permission_approver = self.local_runtime.permission_approver_for(&session_id);
        let mut agent = MayAgent::from_config_with_model_and_permissions(
            &app.config,
            sandbox_dir,
            may_dir,
            app.cwd.clone(),
            session_id,
            self.workspace_broker.clone(),
            self.ask_user_broker.clone(),
            model,
            reasoning_effort,
            profile_model.as_ref().map(|profile| profile.id.clone()),
            Some(permission_approver),
            profile_persona,
            profile_memories,
            runtime.plan_state,
            location,
            Some(self.tool_dispatchers.task.clone()),
            Some(self.tool_dispatchers.notify.clone()),
            self.tool_dispatchers.browser.clone(),
            Some(self.tool_dispatchers.register_skills.clone()),
            Some(self.tool_dispatchers.register_subagents.clone()),
            environment,
            0,
        )
        .await?;

        if let Some(snapshot) =
            crate::session::load_persisted_context_snapshot(&db, &session_id_string).await?
        {
            agent.restore_checkpoint(snapshot.checkpoint.clone())?;
            for message in crate::session::history_delta_since_snapshot(
                &history_messages,
                snapshot.history_message_count,
            )? {
                agent.push_message(message);
            }
        } else {
            for message in history_messages {
                agent.push_message(message);
            }
        }

        Ok(spawn_worker(DefaultExecutor, agent, db))
    }

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

    pub async fn mark_running(&self, app: &App) -> Result<(), TuiRuntimeError> {
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        self.local_runtime.set_running(&session_id).await
    }

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
        &self,
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
            if let Some(title) =
                generate_session_title(&config, &user_message, &assistant_response).await
                && let Err(e) = db
                    .update_session(session_id.as_str(), Some(&title), None)
                    .await
            {
                tracing::warn!("Failed to update session title in DB: {}", e);
            }
        })
        .detach();
    }
}

#[derive(Debug)]
pub struct TitleGenContext {
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

        if let Some(model) = config.last_model.as_deref() {
            return Self::new(model);
        }

        let model = config
            .default_model_for_tier(crate::config::ModelTier::Balanced)
            .ok_or(TuiRuntimeError::NoModelConfigured)?;

        Self::new(model)
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

fn prepare_message_content(
    msg: &PendingMessage,
) -> Result<(String, Vec<url::Url>), TuiRuntimeError> {
    let mut parts = vec![msg.content.clone()];
    let mut attachment_urls = Vec::new();

    for attachment in &msg.attachments {
        match attachment {
            Attachment::Image { path, .. } => {
                let url = url::Url::from_file_path(path).map_err(|()| {
                    TuiRuntimeError::AttachmentPathToUrl {
                        path: path.to_path_buf(),
                    }
                })?;
                attachment_urls.push(url);
            }
            Attachment::FileReference { path } => {
                parts.push(format!("\n[File reference: {}]", path.display()));
            }
            Attachment::PastedText {
                content,
                saved_path,
            } => {
                let char_count = content.chars().count();
                let xml = build_pasted_text_xml(content, char_count, saved_path.as_ref())?;
                parts.push(format!("\n{xml}"));
            }
        }
    }

    Ok((parts.join(""), attachment_urls))
}

async fn resolve_agent_environment_for_tui(
    profile_environment: Option<crate::profile::ProfileEnvironment>,
    profile_model: Option<&crate::db::entity::profiles::Model>,
    container_preferences: Option<crate::profile::ContainerProfilePreferences>,
    may_dir: &std::path::Path,
    sandbox_dir: &std::path::Path,
    session_id: &SessionId,
) -> Result<crate::agent::AgentEnvironment, TuiRuntimeError> {
    crate::agent::resolve_agent_environment_for_session(
        profile_environment,
        profile_model,
        container_preferences,
        may_dir,
        sandbox_dir,
        session_id,
    )
    .await
    .map_err(TuiRuntimeError::ResolveAgentEnvironment)
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct PastedText {
    #[serde(rename = "@chars")]
    chars: usize,
    #[serde(rename = "@truncated", skip_serializing_if = "Option::is_none")]
    truncated: Option<bool>,
    #[serde(rename = "@full_path", skip_serializing_if = "Option::is_none")]
    full_path: Option<String>,
    #[serde(rename = "$text")]
    content: String,
}

fn build_pasted_text_xml(
    content: &str,
    char_count: usize,
    saved_path: Option<&std::path::PathBuf>,
) -> Result<String, TuiRuntimeError> {
    let (text_content, is_truncated) = if char_count > LARGE_PASTE_THRESHOLD {
        let truncated: String = content.chars().take(PASTE_TRUNCATE_LIMIT).collect();
        (truncated, true)
    } else {
        (content.to_string(), false)
    };

    let xml = PastedText {
        chars: char_count,
        truncated: is_truncated.then_some(true),
        full_path: saved_path.map(|p| p.display().to_string()),
        content: text_content,
    };

    quick_xml::se::to_string(&xml).map_err(TuiRuntimeError::SerializePasteXml)
}

pub async fn send_image_fetch_event(
    event_tx: &Sender<crate::tui::AppEvent>,
    url: String,
    data: Option<Vec<u8>>,
) {
    let event = crate::tui::AppEvent::ImageFetchResult { url, data };
    if let Err(e) = event_tx.send(event).await {
        tracing::warn!("Failed to send image fetch event: {}", e);
    }
}
