use std::sync::Arc;

use aither_sandbox::{BollardContainerExec, ContainerExecHandle, ContainerLaunchSpec};
use async_channel::Sender;
use base64::Engine;
use executor_core::{DefaultExecutor, spawn};
use serde::Serialize;

use crate::agent::{AgentHandle, AgentWorker, spawn_worker};
use crate::ask_user::{AskUserBroker, Question, QuestionAnswer};
use crate::container::ContainerRuntime;
use crate::container::lifecycle::ContainerManager;
use crate::tui::queue::{LARGE_PASTE_THRESHOLD, PASTE_TRUNCATE_LIMIT};
use crate::tui::{App, Attachment, PendingMessage};
use crate::workspace_request::WorkspaceRequestBroker;
use crate::{ChatDb, MayAgent, MayConfig, SessionId, generate_session_title};

#[derive(Debug, Clone)]
pub struct LocalSessionRuntime {
    runtime_service: Arc<crate::session::SessionRuntimeService>,
    permission_approver: crate::agent::PermissionApprover,
}

impl LocalSessionRuntime {
    pub fn new(db: ChatDb) -> Self {
        let notify = Arc::new(tokio::sync::Notify::new());
        let runtime_service = Arc::new(crate::session::SessionRuntimeService::new(db, notify));
        let permission_approver =
            crate::agent::PermissionApprover::new().with_runtime_service(runtime_service.clone());
        Self {
            runtime_service,
            permission_approver,
        }
    }

    pub fn permission_approver_for(
        &self,
        session_id: &SessionId,
    ) -> crate::agent::PermissionApprover {
        self.permission_approver
            .for_session(session_id.as_str().to_string())
    }

    pub async fn sync_app_queue(
        &self,
        app: &mut App,
        session_id: &SessionId,
    ) -> anyhow::Result<()> {
        let runtime = self
            .runtime_service
            .get(session_id.as_str())
            .await
            .map_err(anyhow::Error::from)?;
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

    pub async fn enqueue_pending_message(
        &self,
        app: &mut App,
        session_id: &SessionId,
        message: PendingMessage,
    ) -> anyhow::Result<()> {
        let runtime = self
            .runtime_service
            .enqueue(
                session_id.as_str(),
                pending_message_to_runtime(&message)?,
                message.delivery_mode,
            )
            .await
            .map_err(anyhow::Error::from)?;
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

    pub async fn take_next_dispatchable(
        &self,
        app: &mut App,
        session_id: &SessionId,
    ) -> anyhow::Result<Option<PendingMessage>> {
        let next = self
            .runtime_service
            .take_next_dispatchable(session_id.as_str())
            .await
            .map_err(anyhow::Error::from)?;
        let next = match next {
            Some(message) => Some(pending_message_from_runtime(message).await?),
            None => None,
        };
        self.sync_app_queue(app, session_id).await?;
        Ok(next)
    }

    pub async fn set_running(&self, session_id: &SessionId) -> anyhow::Result<()> {
        self.runtime_service
            .set_state(
                session_id.as_str(),
                crate::session::SessionRuntimeState::Running,
            )
            .await
            .map_err(anyhow::Error::from)
    }

    pub async fn clear_running_if_needed(&self, session_id: &SessionId) -> anyhow::Result<()> {
        self.runtime_service
            .clear_running_if_needed(session_id.as_str())
            .await
            .map_err(anyhow::Error::from)
    }

    pub async fn register_ask_user_request(
        &self,
        app: &mut App,
        session_id: &SessionId,
        questions: &[Question],
    ) -> anyhow::Result<String> {
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
            .await
            .map_err(anyhow::Error::from)?;
        let questions_json = serde_json::to_value(questions).map_err(anyhow::Error::from)?;
        app.session_manager
            .append_message(crate::session::ChatMessage::ask_user_request(
                &crate::session::history::AskUserRequestData {
                    id: request_id.clone(),
                    questions: questions_json,
                },
            ))
            .await
            .map_err(anyhow::Error::from)?;
        Ok(request_id)
    }

    pub async fn resolve_ask_user_response(
        &self,
        app: &mut App,
        session_id: &SessionId,
        request_id: &str,
        answers: &[QuestionAnswer],
    ) -> anyhow::Result<()> {
        self.runtime_service
            .set_waiting_ask_user(session_id.as_str(), None)
            .await
            .map_err(anyhow::Error::from)?;
        self.runtime_service
            .set_state(
                session_id.as_str(),
                crate::session::SessionRuntimeState::Running,
            )
            .await
            .map_err(anyhow::Error::from)?;
        let answers_json = serde_json::to_value(answers).map_err(anyhow::Error::from)?;
        app.session_manager
            .append_message(crate::session::ChatMessage::ask_user_response(
                &crate::session::history::AskUserResponseData {
                    id: request_id.to_string(),
                    answers: answers_json,
                },
            ))
            .await
            .map_err(anyhow::Error::from)?;
        Ok(())
    }

    pub async fn register_workspace_request(
        &self,
        app: &mut App,
        session_id: &SessionId,
        request: &crate::workspace_request::RequestWorkspaceArgs,
    ) -> anyhow::Result<String> {
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
            .await
            .map_err(anyhow::Error::from)?;
        app.session_manager
            .append_message(crate::session::ChatMessage::workspace_request(
                &crate::session::history::WorkspaceRequestData {
                    id: request_id.clone(),
                    path: request.path.clone(),
                    reason: request.reason.clone(),
                },
            ))
            .await
            .map_err(anyhow::Error::from)?;
        Ok(request_id)
    }

    pub async fn resolve_workspace_response(
        &self,
        app: &mut App,
        session_id: &SessionId,
        request_id: &str,
        approved: bool,
    ) -> anyhow::Result<()> {
        self.runtime_service
            .set_waiting_workspace(session_id.as_str(), None)
            .await
            .map_err(anyhow::Error::from)?;
        self.runtime_service
            .set_state(
                session_id.as_str(),
                crate::session::SessionRuntimeState::Running,
            )
            .await
            .map_err(anyhow::Error::from)?;
        app.session_manager
            .append_message(crate::session::ChatMessage::workspace_response(
                &crate::session::history::WorkspaceResponseData {
                    id: request_id.to_string(),
                    approved,
                },
            ))
            .await
            .map_err(anyhow::Error::from)?;
        Ok(())
    }

    pub async fn waiting_permission(
        &self,
        session_id: &SessionId,
    ) -> anyhow::Result<Option<crate::session::PersistedPermissionRequest>> {
        self.runtime_service
            .get(session_id.as_str())
            .await
            .map(|runtime| runtime.waiting_permission)
            .map_err(anyhow::Error::from)
    }

    pub async fn resolve_permission_response(
        &self,
        app: &mut App,
        session_id: &SessionId,
        request_id: &str,
        approved: bool,
    ) -> anyhow::Result<()> {
        let responded = self.permission_approver.respond(request_id, approved);
        self.runtime_service
            .set_waiting_permission(session_id.as_str(), None)
            .await
            .map_err(anyhow::Error::from)?;
        self.runtime_service
            .set_state(
                session_id.as_str(),
                if responded {
                    crate::session::SessionRuntimeState::Running
                } else {
                    crate::session::SessionRuntimeState::Idle
                },
            )
            .await
            .map_err(anyhow::Error::from)?;
        app.session_manager
            .append_message(crate::session::ChatMessage::permission_response(
                &crate::session::history::PermissionResponseData {
                    id: request_id.to_string(),
                    approved,
                },
            ))
            .await
            .map_err(anyhow::Error::from)?;
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
) -> anyhow::Result<crate::session::PendingUserMessage> {
    Ok(crate::session::PendingUserMessage {
        content: message.content.clone(),
        attachments: message
            .attachments
            .iter()
            .map(persisted_attachment_from_tui)
            .collect::<anyhow::Result<Vec<_>>>()?,
        hidden_context_xml: None,
        created_at: message.queued_at.to_string(),
        delivery_mode: message.delivery_mode,
    })
}

async fn pending_message_from_runtime(
    message: crate::session::PendingUserMessage,
) -> anyhow::Result<PendingMessage> {
    let mut attachments = Vec::with_capacity(message.attachments.len());
    for attachment in message.attachments {
        attachments.push(tui_attachment_from_persisted(attachment).await?);
    }
    Ok(PendingMessage {
        content: message.content,
        attachments,
        queued_at: message
            .created_at
            .parse()
            .map_err(|error| anyhow::anyhow!("invalid pending message timestamp: {error}"))?,
        delivery_mode: message.delivery_mode,
    })
}

fn persisted_attachment_from_tui(
    attachment: &Attachment,
) -> anyhow::Result<crate::session::PersistedAttachment> {
    let id = uuid::Uuid::new_v4().to_string();
    match attachment {
        Attachment::Image { path, data } => Ok(crate::session::PersistedAttachment {
            id,
            name: file_name_or_default(path, "image.png"),
            attachment_type: "image".to_string(),
            url: file_url(path)?,
            upload_data_url: Some(image_data_url(path, data)),
            text_content: None,
        }),
        Attachment::FileReference { path } => Ok(crate::session::PersistedAttachment {
            id,
            name: file_name_or_default(path, "file"),
            attachment_type: "file".to_string(),
            url: file_url(path)?,
            upload_data_url: None,
            text_content: None,
        }),
        Attachment::PastedText {
            content,
            saved_path,
        } => Ok(crate::session::PersistedAttachment {
            id,
            name: saved_path
                .as_ref()
                .map(|path| file_name_or_default(path, "pasted.txt"))
                .unwrap_or_else(|| "pasted.txt".to_string()),
            attachment_type: "text".to_string(),
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
) -> anyhow::Result<Attachment> {
    match attachment.attachment_type.as_str() {
        "image" => {
            let path = path_from_attachment_url(&attachment.url)?;
            let data = if async_fs::metadata(&path).await.is_ok() {
                async_fs::read(&path).await.map_err(anyhow::Error::from)?
            } else if let Some(data_url) = attachment.upload_data_url.as_deref() {
                decode_data_url(data_url)?
            } else {
                return Err(anyhow::anyhow!(
                    "persisted image attachment is missing content"
                ));
            };
            Ok(Attachment::Image { path, data })
        }
        "file" => Ok(Attachment::FileReference {
            path: path_from_attachment_url(&attachment.url)?,
        }),
        "text" => Ok(Attachment::PastedText {
            content: if let Some(content) = attachment.text_content {
                content
            } else {
                let path = path_from_attachment_url(&attachment.url)?;
                async_fs::read_to_string(path)
                    .await
                    .map_err(anyhow::Error::from)?
            },
            saved_path: if attachment.url.starts_with("file://") {
                Some(path_from_attachment_url(&attachment.url)?)
            } else {
                None
            },
        }),
        other => Err(anyhow::anyhow!(
            "unsupported persisted attachment type: {other}"
        )),
    }
}

fn file_url(path: &std::path::Path) -> anyhow::Result<String> {
    url::Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("failed to convert path into file URL"))
        .map(|url| url.to_string())
}

fn path_from_attachment_url(url: &str) -> anyhow::Result<std::path::PathBuf> {
    url::Url::parse(url)
        .map_err(anyhow::Error::from)?
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("attachment URL is not a local file URL"))
}

fn file_name_or_default(path: &std::path::Path, default_name: &str) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| default_name.to_string())
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

fn decode_data_url(data_url: &str) -> anyhow::Result<Vec<u8>> {
    let Some((_, encoded)) = data_url.split_once(",") else {
        return Err(anyhow::anyhow!("invalid data URL"));
    };
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(anyhow::Error::from)
}

/// Runtime dependencies shared by session and agent operations.
#[derive(Debug, Clone)]
pub struct RuntimeDeps {
    workspace_broker: WorkspaceRequestBroker,
    ask_user_broker: AskUserBroker,
    location_provider: Arc<crate::location::LocationProvider>,
    local_runtime: LocalSessionRuntime,
}

impl RuntimeDeps {
    pub fn new(
        local_runtime: LocalSessionRuntime,
        workspace_broker: WorkspaceRequestBroker,
        ask_user_broker: AskUserBroker,
        location_provider: Arc<crate::location::LocationProvider>,
    ) -> Self {
        Self {
            workspace_broker,
            ask_user_broker,
            location_provider,
            local_runtime,
        }
    }

    pub async fn start_new_session(
        &self,
        app: &mut App,
        worker: &mut Option<AgentWorker>,
    ) -> anyhow::Result<()> {
        let model = app.config.last_model.clone();
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
            .build_worker_for_session(app, sandbox_dir, session_id, selected_model.as_str())
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
    ) -> anyhow::Result<()> {
        let session = app.session_manager.load_session(id).await?;

        let sandbox_dir = session.sandbox_dir.clone();
        let session_id = session.id.clone();
        let session_model = session.model.clone();
        let messages: Vec<_> = session.history.messages().to_vec();

        app.init_session(&session_id, &messages, session_model.as_deref());
        self.local_runtime.sync_app_queue(app, &session_id).await?;

        let selected_model = SelectedModel::resolve(&app.config, session_model.as_deref())?;
        let new_worker = self
            .build_worker_for_session(app, sandbox_dir, session_id, selected_model.as_str())
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
    ) -> anyhow::Result<()> {
        let session = app
            .session_manager
            .current()
            .ok_or_else(|| anyhow::anyhow!("No active session"))?;

        let sandbox_dir = session.sandbox_dir.clone();
        let session_id = session.id.clone();

        let selected_model = SelectedModel::new(model)?;
        let new_worker = self
            .build_worker_for_session(app, sandbox_dir, session_id, selected_model.as_str())
            .await?;

        app.set_current_model(selected_model.as_str());
        *worker = Some(new_worker);
        Ok(())
    }

    pub fn start_agent_task(
        &self,
        app: &mut App,
        worker: &AgentWorker,
        msg: PendingMessage,
        title_gen: &mut Option<TitleGenContext>,
    ) -> anyhow::Result<AgentHandle> {
        let needs_title = app
            .session_manager
            .current()
            .map(|s| s.title.is_none())
            .unwrap_or(false);

        if needs_title {
            *title_gen = Some(TitleGenContext {
                user_message: msg.content.clone(),
            });
        }

        let (prompt, image_urls) = prepare_message_content(&msg)?;
        app.start_processing(&msg);
        Ok(worker.run(prompt, image_urls))
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
    ) -> anyhow::Result<AgentWorker> {
        let db = app.session_manager.db().clone();
        let session_id_string = session_id.as_str().to_string();
        let history_messages = app
            .session_manager
            .current()
            .map(|session| session.history.to_aither_messages())
            .unwrap_or_default();
        let may_dir = app.session_manager.may_dir().to_path_buf();
        let profile_model = if let Some(profile_id) = app
            .session_manager
            .current()
            .and_then(|session| session.profile_id.clone())
        {
            app.session_manager
                .db()
                .get_profile(&profile_id)
                .await
                .map_err(anyhow::Error::from)?
        } else {
            None
        };
        let profile_persona = profile_model
            .as_ref()
            .and_then(|profile| profile.persona.clone());
        let profile_memories = if let Some(ref profile) = profile_model {
            let memories =
                crate::memory::ProfileMemories::load(app.session_manager.db(), &profile.id)
                    .await
                    .map_err(anyhow::Error::from)?;
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
                    .map_err(|e| anyhow::anyhow!("Location fetch failed: {e}"))?,
            )
        } else {
            None
        };
        let profile_environment = profile_model
            .as_ref()
            .map(|profile| {
                crate::profile::ProfileInfo::from_db_model(profile.clone())
                    .map(|info| info.environment)
            })
            .transpose()
            .map_err(anyhow::Error::from)?;
        let runtime = app
            .session_manager
            .db()
            .get_session_runtime(session_id.as_str())
            .await
            .map_err(anyhow::Error::from)?
            .unwrap_or_default();
        let environment = resolve_agent_environment_for_tui(
            profile_environment,
            profile_model.as_ref(),
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
            profile_model.as_ref().map(|profile| profile.id.clone()),
            Some(permission_approver),
            profile_persona,
            profile_memories,
            runtime.plan_state,
            location,
            None,
            None,
            None,
            None,
            environment,
            0,
        )
        .await?;

        if let Some(snapshot) =
            crate::session::load_persisted_context_snapshot(&db, &session_id_string).await?
        {
            agent
                .restore_checkpoint(snapshot.checkpoint.clone())
                .map_err(anyhow::Error::from)?;
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
    ) -> anyhow::Result<()> {
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
            .ok_or_else(|| anyhow::anyhow!("No active session"))?;
        self.local_runtime
            .enqueue_pending_message(app, &session_id, message)
            .await
    }

    pub async fn take_next_dispatchable(
        &self,
        app: &mut App,
    ) -> anyhow::Result<Option<PendingMessage>> {
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

    pub async fn mark_running(&self, app: &App) -> anyhow::Result<()> {
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
            .ok_or_else(|| anyhow::anyhow!("No active session"))?;
        self.local_runtime.set_running(&session_id).await
    }

    pub async fn register_ask_user_request(
        &self,
        app: &mut App,
        session_id: &SessionId,
        questions: &[Question],
    ) -> anyhow::Result<String> {
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
    ) -> anyhow::Result<()> {
        self.local_runtime
            .resolve_ask_user_response(app, session_id, request_id, answers)
            .await
    }

    pub async fn register_workspace_request(
        &self,
        app: &mut App,
        session_id: &SessionId,
        request: &crate::workspace_request::RequestWorkspaceArgs,
    ) -> anyhow::Result<String> {
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
    ) -> anyhow::Result<()> {
        self.local_runtime
            .resolve_workspace_response(app, session_id, request_id, approved)
            .await
    }

    pub async fn pending_permission(
        &self,
        app: &App,
    ) -> anyhow::Result<Option<crate::session::PersistedPermissionRequest>> {
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
    ) -> anyhow::Result<()> {
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
            .ok_or_else(|| anyhow::anyhow!("No active session"))?;
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
    fn new(model: &str) -> anyhow::Result<Self> {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            anyhow::bail!("Selected model is empty");
        }
        Ok(Self(trimmed.to_string()))
    }

    fn resolve(config: &MayConfig, preferred: Option<&str>) -> anyhow::Result<Self> {
        if let Some(model) = preferred {
            return Self::new(model);
        }

        if let Some(model) = config.last_model.as_deref() {
            return Self::new(model);
        }

        let model = config
            .default_model_for_tier(crate::config::ModelTier::Balanced)
            .ok_or_else(|| anyhow::anyhow!("No model configured"))?;

        Self::new(model)
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

fn prepare_message_content(msg: &PendingMessage) -> anyhow::Result<(String, Vec<url::Url>)> {
    let mut parts = vec![msg.content.clone()];
    let mut attachment_urls = Vec::new();

    for attachment in &msg.attachments {
        match attachment {
            Attachment::Image { path, .. } => {
                let url = url::Url::from_file_path(path).map_err(|_| {
                    anyhow::anyhow!(
                        "Failed to convert attachment path to URL: {}",
                        path.display()
                    )
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
                parts.push(format!("\n{}", xml));
            }
        }
    }

    Ok((parts.join(""), attachment_urls))
}

async fn resolve_agent_environment_for_tui(
    profile_environment: Option<crate::profile::ProfileEnvironment>,
    profile_model: Option<&crate::db::entity::profiles::Model>,
    may_dir: &std::path::Path,
    sandbox_dir: &std::path::Path,
    session_id: &SessionId,
) -> anyhow::Result<crate::agent::AgentEnvironment> {
    match profile_environment {
        Some(crate::profile::ProfileEnvironment::Container { .. }) => {}
        Some(crate::profile::ProfileEnvironment::Remote { .. }) => {
            let profile =
                profile_model.ok_or_else(|| anyhow::anyhow!("remote profile model is missing"))?;
            let config_path = may_dir.join("config.toml");
            let config = crate::config::MayConfig::load_from(&config_path).map_err(|error| {
                anyhow::anyhow!("failed to load config.toml for remote profile: {error}")
            })?;
            let ssh_server =
                crate::remote_profile::resolve_remote_profile_server(profile, &config.ssh_servers)
                    .map_err(anyhow::Error::msg)?;
            return Ok(crate::agent::AgentEnvironment::Remote { ssh_server });
        }
        _ => return Ok(crate::agent::AgentEnvironment::Local),
    }

    let profile =
        profile_model.ok_or_else(|| anyhow::anyhow!("container profile model is missing"))?;

    let runtime = ContainerRuntime::detect().await.map_err(|e| {
        anyhow::anyhow!("No container runtime available for container profile: {e}")
    })?;
    let container_manager = ContainerManager::new(Arc::new(runtime), may_dir.to_path_buf());

    let default_image_tag = crate::container::profile::default_image_tag(may_dir, profile)
        .await
        .map_err(|e| anyhow::anyhow!("Invalid container profile image tag: {e}"))?;
    let image_spec =
        crate::container::profile::image_spec_for_profile(may_dir, profile, &default_image_tag)
            .map_err(|e| anyhow::anyhow!("Invalid container profile configuration: {e}"))?;
    let extra_mounts = crate::container::profile::mounts_for_profile(profile)
        .map_err(|e| anyhow::anyhow!("Invalid container mount configuration: {e}"))?;

    crate::container::image::ensure_image(container_manager.runtime(), &image_spec)
        .await
        .map_err(|e| anyhow::anyhow!("Container image build failed: {e}"))?;

    let container_name =
        crate::container::profile::container_name_for_profile_session(profile, session_id);
    let ipc_socket_path = sandbox_dir.join("leash-ipc.sock");
    let mut launch_spec = ContainerLaunchSpec::new(
        container_name,
        image_spec.tag.clone(),
        sandbox_dir.to_path_buf(),
    )
    .with_ipc_socket(ipc_socket_path);
    for mount in extra_mounts {
        launch_spec = launch_spec.with_mount(mount);
    }

    let container_id = container_manager
        .ensure_running(&launch_spec)
        .await
        .map_err(|e| anyhow::anyhow!("Container start failed: {e}"))?;

    let container_exec = ContainerExecHandle::new(Arc::new(BollardContainerExec::new(Arc::new(
        container_manager.runtime().client.clone(),
    ))));

    Ok(crate::agent::AgentEnvironment::Container {
        container_id,
        container_ipc_host: container_manager.runtime().host_gateway().to_string(),
        container_exec,
        browser_backend: None,
    })
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
) -> anyhow::Result<String> {
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

    quick_xml::se::to_string(&xml)
        .map_err(|e| anyhow::anyhow!("Failed to serialize paste XML: {}", e))
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
