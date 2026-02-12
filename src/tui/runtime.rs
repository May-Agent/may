use async_channel::Sender;
use executor_core::{DefaultExecutor, spawn};
use serde::Serialize;

use crate::agent::{AgentHandle, AgentWorker, spawn_worker};
use crate::ask_user::AskUserBroker;
use crate::tui::queue::{LARGE_PASTE_THRESHOLD, PASTE_TRUNCATE_LIMIT};
use crate::tui::{App, Attachment, PendingMessage};
use crate::workspace_request::WorkspaceRequestBroker;
use crate::{MayAgent, MayConfig, SessionId, generate_session_title};

/// Runtime dependencies shared by session and agent operations.
#[derive(Debug, Clone)]
pub struct RuntimeDeps {
    workspace_broker: WorkspaceRequestBroker,
    ask_user_broker: AskUserBroker,
}

impl RuntimeDeps {
    pub fn new(workspace_broker: WorkspaceRequestBroker, ask_user_broker: AskUserBroker) -> Self {
        Self {
            workspace_broker,
            ask_user_broker,
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
        *agent_handle = None;
        app.finish_processing();

        if let Err(e) = app.session_manager.save_current().await {
            tracing::warn!("Failed to save session history: {}", e);
        }

        if let Some(ctx) = title_gen.take() {
            if !assistant_response.is_empty() {
                self.spawn_title_generation(
                    app.config.clone(),
                    app.session_manager.may_dir().to_path_buf(),
                    app.session_manager.current().map(|s| s.id.clone()),
                    ctx.user_message,
                    assistant_response.to_string(),
                );
            }
        }
    }

    async fn build_worker_for_session(
        &self,
        app: &App,
        sandbox_dir: std::path::PathBuf,
        session_id: SessionId,
        model: &str,
    ) -> anyhow::Result<AgentWorker> {
        let may_dir = app.session_manager.may_dir().to_path_buf();
        let agent = MayAgent::from_config_with_model(
            &app.config,
            sandbox_dir,
            may_dir,
            app.cwd.clone(),
            session_id,
            self.workspace_broker.clone(),
            self.ask_user_broker.clone(),
            model,
        )
        .await?;

        Ok(spawn_worker(DefaultExecutor, agent))
    }

    fn spawn_title_generation(
        &self,
        config: MayConfig,
        may_dir: std::path::PathBuf,
        session_id: Option<SessionId>,
        user_message: String,
        assistant_response: String,
    ) {
        let Some(session_id) = session_id else {
            tracing::warn!("Session missing during title generation");
            return;
        };

        spawn(async move {
            use crate::session::SessionStore;

            if let Some(title) =
                generate_session_title(&config, &user_message, &assistant_response).await
            {
                let store = SessionStore::new(may_dir.join("sessions"));
                match store.read_history(session_id.as_str()).await {
                    Ok(mut history) => {
                        history.metadata.title = Some(title);
                        if let Err(e) = store.write_history(session_id.as_str(), &history).await {
                            tracing::warn!("Failed to write session title: {}", e);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to read history for title update: {}", e);
                    }
                }
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
