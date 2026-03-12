use std::sync::Arc;

use aither_sandbox::{BollardContainerExec, ContainerExecHandle, ContainerLaunchSpec};
use async_channel::Sender;
use executor_core::{DefaultExecutor, spawn};
use serde::Serialize;

use crate::agent::{AgentHandle, AgentWorker, spawn_worker};
use crate::ask_user::AskUserBroker;
use crate::container::ContainerRuntime;
use crate::container::lifecycle::ContainerManager;
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
                crate::location::LocationProvider::new()
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
        let environment = resolve_agent_environment_for_tui(
            profile_environment,
            profile_model.as_ref(),
            &may_dir,
            &sandbox_dir,
            &session_id,
        )
        .await?;
        let agent = MayAgent::from_config_with_model_and_permissions(
            &app.config,
            sandbox_dir,
            may_dir,
            app.cwd.clone(),
            session_id,
            self.workspace_broker.clone(),
            self.ask_user_broker.clone(),
            model,
            None,
            profile_persona,
            profile_memories,
            location,
            None,
            None,
            None,
            None,
            environment,
            0,
        )
        .await?;

        Ok(spawn_worker(DefaultExecutor, agent))
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
            let config = crate::config::MayConfig::load_from(&config_path)
                .map_err(|error| anyhow::anyhow!("failed to load config.toml for remote profile: {error}"))?;
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

    let container_exec = Arc::new(ContainerExecHandle::new(Arc::new(
        BollardContainerExec::new(Arc::new(container_manager.runtime().client.clone())),
    )));

    Ok(crate::agent::AgentEnvironment::Container {
        container_id,
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
