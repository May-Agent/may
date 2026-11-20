use std::sync::Arc;
use std::time::Duration;

use notify::{
    Config as NotifyConfig, Event as NotifyEvent, RecommendedWatcher, RecursiveMode, Watcher,
};
use skyzen::Result as SkyResult;
use skyzen::extract::Query;
use skyzen::responder::Sse;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use tokio::time::MissedTickBehavior;

use crate::config::{MayConfig, NativeToolsConfig, ProviderInstance, SshServerConfig};
use crate::copilot::{clear_copilot_credentials, has_copilot_token, save_copilot_token};
use crate::mcp::MayMcpServerConfig;
use crate::model_selector::ModelSelector;
use crate::session::MayHomePaths;
use crate::skill::SkillManager;
use crate::ui_event_bus::UiEventTopics;

use super::error::{bad_request_error, internal_error, internal_error_message};
use super::helpers::send_event;
use super::message_mapping::map_skill;
use super::model_resolution::persist_last_model;
use super::state::DaemonState;
use super::types::{
    ApiHealth, CopilotAuthEvent, CopilotAuthStatus, CopilotStartResponse, CopilotStatusResponse,
    JobPoolRequest, LastModelRequest, McpCreateRequest, MemoryPayload, MemorySaveRequest,
    SkillInfoDto, SkillInstallRequest, SseHeartbeat, SshServerRequest, UiAppearanceConfig,
    UiBrowserConfig, UiConfigResponse, UiFullConfig, UiModelsConfig, UiNativeToolsConfig,
    UiPreferencesConfig, UiProviderConfig, UiRecapConfig, UiRemoteAccessConfig, UiSandboxConfig,
    UiSessionsConfig, UiStreamConfig, UiSuggestionsConfig, UpdateChannelRequest,
};
use crate::computer::proto::{AgentRequest, AgentResponse};

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct SkillScopeQuery {
    profile_id: String,
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct CopilotAuthStreamQuery {
    device_code: String,
    interval: u64,
}

fn scoped_skill_manager(state: &DaemonState, profile_id: &str) -> SkillManager {
    let home_paths = MayHomePaths::new(state.may_dir.clone());
    SkillManager::from_profile_home(&home_paths.profile_home(profile_id))
}

fn parse_selector_list(values: &[String]) -> SkyResult<Vec<ModelSelector>> {
    values
        .iter()
        .map(|value| value.parse::<ModelSelector>().map_err(bad_request_error))
        .collect()
}

pub(super) async fn list_skills_for_profile(
    state: &DaemonState,
    profile_id: &str,
) -> Result<Vec<SkillInfoDto>, skyzen::Error> {
    let skills = scoped_skill_manager(state, profile_id)
        .list_skills()
        .await
        .map_err(internal_error)?;
    Ok(skills.into_iter().map(map_skill).collect::<Vec<_>>())
}

pub(super) async fn get_ui_config(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<UiConfigResponse>> {
    let config = state.config_snapshot();
    Ok(Json(build_ui_config_response(&config)))
}

pub(super) fn build_ui_config_response(config: &MayConfig) -> UiConfigResponse {
    UiConfigResponse {
        ssh_servers: config.ssh_servers.clone(),
        acp_agents: super::acp_agents::connected_dto(config),
        max_iterations: config.max_iterations,
        ui_font: config.appearance.font.clone(),
        ui_theme: config.appearance.theme.clone(),
        timezone: config.time.user_timezone().to_string(),
        full_config: build_ui_full_config(config),
    }
}

pub(super) fn build_ui_full_config(config: &MayConfig) -> UiFullConfig {
    let mut providers = config
        .provider
        .iter()
        .map(|(id, instance)| UiProviderConfig {
            id: id.clone(),
            provider_type: instance.resolve_type(id),
            has_api_key: instance.api_key.is_some(),
            api_key: None,
            base_url: instance.base_url.clone(),
            display_name: instance.display_name.clone(),
            enabled: instance.enabled,
            native_tools: UiNativeToolsConfig::from(&instance.native_tools),
            cache_lifetime: instance.cache_lifetime,
            api: instance.api,
        })
        .collect::<Vec<_>>();
    providers.sort_by(|a, b| a.id.cmp(&b.id));

    UiFullConfig {
        providers,
        models: UiModelsConfig {
            advanced: config
                .models
                .advanced
                .iter()
                .map(ToString::to_string)
                .collect(),
            balanced: config
                .models
                .balanced
                .iter()
                .map(ToString::to_string)
                .collect(),
            fast: config.models.fast.iter().map(ToString::to_string).collect(),
            image_generation: config
                .models
                .image_generation
                .iter()
                .map(ToString::to_string)
                .collect(),
            compaction_model: config
                .models
                .compaction_model
                .as_ref()
                .map(ToString::to_string),
        },
        preferences: UiPreferencesConfig {
            warn_on_model_change: config.preferences.warn_on_model_change,
            show_token_usage: config.preferences.show_token_usage,
            show_decode_speed: config.preferences.show_decode_speed,
            network_policy_mode: config.preferences.network_policy.mode,
            network_policy_hosts: config.preferences.network_policy.hosts.clone(),
        },
        appearance: UiAppearanceConfig {
            font: config.appearance.font.clone(),
            theme: config.appearance.theme.clone(),
        },
        last_model: config.last_model.as_ref().map(ToString::to_string),
        max_iterations: config.max_iterations,
        browser: UiBrowserConfig {
            enabled: config.browser.enabled,
            backend: config.browser.backend,
            port: config.browser.port,
            remote_cdp_endpoint: config.browser.remote_cdp_endpoint.clone(),
            screencast_jpeg_quality: config.browser.screencast_jpeg_quality,
            encoder_initial_bitrate_kbps: config.browser.encoder_initial_bitrate_kbps,
            screencast_max_width: config.browser.screencast_max_width,
            screencast_max_height: config.browser.screencast_max_height,
            every_nth_frame: config.browser.every_nth_frame,
            stealth_enabled: config.browser.stealth_enabled,
            system_profile: config.browser.system_profile.as_ref().map(|sp| {
                crate::daemon::types::UiSystemProfileConfig {
                    path: sp.path.display().to_string(),
                    copy_on_first_use: sp.copy_on_first_use,
                }
            }),
        },
        remote_access: UiRemoteAccessConfig::from(&config.remote_access),
        sandbox: UiSandboxConfig {
            command_priority: config.sandbox.command_priority,
            job_pool_size: config
                .sandbox
                .job_pool_size
                .map(std::num::NonZeroUsize::get),
        },
        stream: UiStreamConfig {
            codec: config.stream.codec,
        },
        suggestions: Some(UiSuggestionsConfig {
            enabled: config.suggestions.enabled,
        }),
        recap: Some(UiRecapConfig {
            auto: config.recap.auto,
            idle_minutes: config.recap.idle_minutes,
        }),
        sessions: Some(UiSessionsConfig {
            recent_hours: config.sessions.recent_hours,
        }),
    }
}

pub(super) async fn save_full_config(
    Json(req): Json<UiFullConfig>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let tokio_handle = state.tokio_handle.clone();
    let response = tokio_handle
        .spawn({
            let state = state.clone();
            async move { run_save_full_config(state, req).await }
        })
        .await
        .map_err(|_| {
            internal_error_message("config save task panicked before producing a response")
        })??;
    Ok(Json(response))
}

/// Build the provider map for a full-config save.
///
/// Shared by the REST handler and tests so a save applies exactly what the
/// web UI submits, including provider-native tool toggles.
fn provider_map_from_ui(
    stored: &std::collections::HashMap<String, ProviderInstance>,
    providers: &[UiProviderConfig],
) -> std::collections::HashMap<String, ProviderInstance> {
    providers
        .iter()
        .map(|p| {
            let mut instance = ProviderInstance::new(p.provider_type);
            instance.api_key = match &p.api_key {
                Some(crate::daemon::types::SecretUpdate::Replace(new_key)) => {
                    (!new_key.trim().is_empty()).then(|| new_key.clone())
                }
                Some(crate::daemon::types::SecretUpdate::Remove) => None,
                None | Some(crate::daemon::types::SecretUpdate::Keep) => stored
                    .get(&p.id)
                    .and_then(|instance| instance.api_key.clone()),
            };
            instance.base_url = p.base_url.clone().filter(|v| !v.trim().is_empty());
            instance.display_name = p.display_name.clone().filter(|v| !v.trim().is_empty());
            instance.enabled = p.enabled;
            instance.native_tools = NativeToolsConfig::from(&p.native_tools);
            instance.cache_lifetime = p.cache_lifetime;
            instance.api = p.api;
            (p.id.clone(), instance)
        })
        .collect()
}

/// A stream codec this host cannot serve is the request's problem; every
/// other browser failure is the daemon's.
fn browser_stream_error(error: crate::container::browser::BrowserError) -> skyzen::Error {
    match error {
        crate::container::browser::BrowserError::CodecSelection(error) => bad_request_error(error),
        error => internal_error(error),
    }
}

async fn run_save_full_config(state: Arc<DaemonState>, req: UiFullConfig) -> SkyResult<ApiHealth> {
    let previous_browser_manager = state.browser_manager();
    let previous_config = state.config_snapshot();
    let codec = req.stream.codec;
    let job_pool_size = parse_job_pool_size(req.sandbox.job_pool_size)?;
    let advanced = parse_selector_list(&req.models.advanced)?;
    let balanced = parse_selector_list(&req.models.balanced)?;
    let fast = parse_selector_list(&req.models.fast)?;
    let image_generation = parse_selector_list(&req.models.image_generation)?;
    let compaction_model = req
        .models
        .compaction_model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.parse::<ModelSelector>().map_err(bad_request_error))
        .transpose()?;
    let last_model = req
        .last_model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.parse::<ModelSelector>().map_err(bad_request_error))
        .transpose()?;

    let mut updated = (*previous_config).clone();
    updated.provider = provider_map_from_ui(&updated.provider, &req.providers);
    updated.models.advanced = advanced;
    updated.models.balanced = balanced;
    updated.models.fast = fast;
    updated.models.image_generation = image_generation;
    updated.models.compaction_model = compaction_model;
    updated.preferences.warn_on_model_change = req.preferences.warn_on_model_change;
    updated.preferences.show_token_usage = req.preferences.show_token_usage;
    updated.preferences.show_decode_speed = req.preferences.show_decode_speed;
    updated.preferences.network_policy.mode = req.preferences.network_policy_mode;
    updated.preferences.network_policy.hosts = req
        .preferences
        .network_policy_hosts
        .iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect();
    updated.appearance.font.clone_from(&req.appearance.font);
    updated.appearance.theme = req.appearance.theme;
    updated.last_model = last_model;
    updated.max_iterations = req.max_iterations;
    updated.sandbox.command_priority = req.sandbox.command_priority;
    updated.sandbox.job_pool_size = job_pool_size;
    updated.stream.codec = codec;
    updated.browser.enabled = req.browser.enabled;
    updated.browser.backend = req.browser.backend;
    updated.browser.port = req.browser.port;
    updated.browser.remote_cdp_endpoint = req
        .browser
        .remote_cdp_endpoint
        .clone()
        .filter(|value| !value.trim().is_empty());
    updated.browser.screencast_jpeg_quality = req.browser.screencast_jpeg_quality;
    updated.browser.encoder_initial_bitrate_kbps = req.browser.encoder_initial_bitrate_kbps;
    updated.browser.screencast_max_width = req.browser.screencast_max_width;
    updated.browser.screencast_max_height = req.browser.screencast_max_height;
    updated.browser.every_nth_frame = req.browser.every_nth_frame;
    updated.browser.stealth_enabled = req.browser.stealth_enabled;
    updated.browser.system_profile =
        req.browser
            .system_profile
            .as_ref()
            .map(|sp| crate::config::SystemProfileConfig {
                path: std::path::PathBuf::from(&sp.path),
                copy_on_first_use: sp.copy_on_first_use,
            });
    updated.remote_access.enabled = req.remote_access.enabled;
    updated
        .remote_access
        .tailscale_binary
        .clone_from(&req.remote_access.tailscale_binary);
    updated.remote_access.https_port = req.remote_access.https_port;
    updated.remote_access.allow_local_environment = req.remote_access.allow_local_environment;
    updated.remote_access.allowed_logins = normalize_logins(&req.remote_access.allowed_logins);
    updated.remote_access.funnel = req.remote_access.funnel;
    updated.remote_access.funnel_port = req.remote_access.funnel_port;
    updated.remote_access.webhook_max_body_bytes = req.remote_access.webhook_max_body_bytes;
    if let Some(suggestions) = &req.suggestions {
        updated.suggestions.enabled = suggestions.enabled;
    }
    if let Some(recap) = &req.recap {
        updated.recap.auto = recap.auto;
        updated.recap.idle_minutes = recap.idle_minutes;
    }
    if let Some(sessions) = &req.sessions {
        updated.sessions.recent_hours = sessions.recent_hours;
    }

    updated.validate().map_err(bad_request_error)?;
    updated
        .preferences
        .network_policy
        .compile()
        .map_err(bad_request_error)?;

    // Each stream selects the codec for its own format before it changes
    // anything, so a codec this host cannot serve leaves both as they were.
    let codec_changed = codec != previous_config.stream.codec;
    let next_browser_manager = if updated.browser.enabled {
        let next_browser_config = updated.browser.clone();
        match &previous_browser_manager {
            // Nothing but possibly the codec changed: the running browser
            // keeps its pages and only its stream moves to the new codec.
            Some(manager) if next_browser_config == previous_config.browser => {
                if codec_changed {
                    manager
                        .set_codec(codec)
                        .await
                        .map_err(browser_stream_error)?;
                }
                Some(manager.clone())
            }
            _ => Some(Arc::new(
                crate::container::browser::BrowserManager::new(
                    next_browser_config,
                    state.may_dir.clone(),
                    crate::container::browser::BrowserLifecyclePublisher::new(
                        state.browser_lifecycle_tx.clone(),
                    ),
                    codec,
                )
                .map_err(browser_stream_error)?,
            )),
        }
    } else {
        None
    };
    if codec_changed && state.computer.is_running() {
        match state
            .computer
            .request(AgentRequest::SetStreamCodec { codec })
            .await
        {
            Ok(AgentResponse::Ok) => {}
            Ok(AgentResponse::Error { error }) | Err(error) => {
                return Err(internal_error_message(error));
            }
            Ok(_) => {
                return Err(internal_error_message(
                    "unexpected reply from computer-agent",
                ));
            }
        }
    }

    state.save_config(&updated).await.map_err(internal_error)?;
    state.replace_config(updated.clone());
    state.live_limits.apply(&updated);
    state.job_server.resize(updated.sandbox.job_pool_size());
    if let Some(previous_browser_manager) = previous_browser_manager
        && !next_browser_manager
            .as_ref()
            .is_some_and(|next| Arc::ptr_eq(next, &previous_browser_manager))
    {
        previous_browser_manager
            .stop()
            .await
            .map_err(internal_error)?;
    }
    state.set_browser_manager(next_browser_manager);
    state
        .task_runtime
        .set_network_policy(&updated.preferences.network_policy)
        .map_err(internal_error)?;
    super::remote_access::sync_remote_access_from_state(&state)
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::CONFIG | UiEventTopics::BROWSER_STATUS);
    Ok(ApiHealth { ok: true })
}

fn parse_job_pool_size(size: Option<usize>) -> SkyResult<Option<std::num::NonZeroUsize>> {
    size.map(|size| {
        std::num::NonZeroUsize::new(size)
            .ok_or_else(|| bad_request_error("the jobserver pool needs at least one token"))
    })
    .transpose()
}

/// Sets the machine-wide jobserver pool size and resizes the running pool.
pub(super) async fn set_job_pool(
    Json(req): Json<JobPoolRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let size = parse_job_pool_size(req.size)?;
    let updated = state
        .save_and_update_config(|config| config.sandbox.job_pool_size = size)
        .await
        .map_err(internal_error)?;
    state.job_server.resize(updated.sandbox.job_pool_size());
    state.notify_ui_events(UiEventTopics::CONFIG);
    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn set_update_channel(
    Json(req): Json<UpdateChannelRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let _ = state
        .save_and_update_config(|config| config.update.channel = Some(req.channel))
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::CONFIG);
    Ok(Json(ApiHealth { ok: true }))
}

/// Logins trimmed, with blanks and repeats dropped, in their given order.
fn normalize_logins(logins: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    logins
        .iter()
        .map(|login| login.trim())
        .filter(|login| !login.is_empty() && seen.insert(*login))
        .map(str::to_string)
        .collect()
}

pub(super) async fn set_last_model(
    Json(req): Json<LastModelRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let model = req.model.trim();
    if model.is_empty() {
        return Err(bad_request_error("model is required"));
    }

    persist_last_model(&state, model)
        .await
        .map_err(|error| match error {
            super::model_resolution::PersistLastModelError::InvalidSelector(err) => {
                bad_request_error(err)
            }
            super::model_resolution::PersistLastModelError::Save(err) => internal_error(err),
        })?;
    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn get_memory(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<MemoryPayload>> {
    let memory_file = MayHomePaths::new(state.may_dir.clone()).memory_file();
    let content = async_fs::read_to_string(&memory_file)
        .await
        .unwrap_or_default();
    Ok(Json(MemoryPayload { content }))
}

pub(super) async fn save_memory(
    Json(payload): Json<MemorySaveRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    if !payload.explicit_user_request {
        return Err(bad_request_error(
            "memory updates require explicit user request confirmation",
        ));
    }
    let memory_file = MayHomePaths::new(state.may_dir.clone()).memory_file();
    async_fs::write(&memory_file, &payload.content)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

// ── SSH ────────────────────────────────────────

pub(super) async fn add_ssh_server(
    Json(req): Json<SshServerRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let server = SshServerConfig::probe(req.name, req.target)
        .await
        .map_err(bad_request_error)?;
    let _ = state
        .save_and_update_config(|config| config.upsert_ssh_server(server))
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::CONFIG);
    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn remove_ssh_server(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let name = params.get("name").map_err(bad_request_error)?;
    let _ = state
        .save_and_update_config(|config| {
            let _ = config.remove_ssh_server(name);
        })
        .await
        .map_err(internal_error)?;
    state
        .db()
        .remove_profile_attachments_for_server(name)
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::CONFIG);
    Ok(Json(ApiHealth { ok: true }))
}

// ── MCP ────────────────────────────────────────

pub(super) async fn list_mcp_servers(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<crate::mcp::McpServerInfo>>> {
    Ok(Json(state.mcp().server_info().await))
}

pub(super) async fn add_mcp_server(
    Json(req): Json<McpCreateRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let (name, config) = match req {
        McpCreateRequest::Process {
            name,
            command,
            args,
            env,
        } => {
            let trimmed_name = name.trim();
            if trimmed_name.is_empty() {
                return Err(bad_request_error("mcp server name is required"));
            }
            let trimmed_command = command.trim();
            if trimmed_command.is_empty() {
                return Err(bad_request_error("mcp process command is required"));
            }

            let mut normalized_args = Vec::with_capacity(args.len());
            for arg in args {
                let trimmed = arg.trim();
                if trimmed.is_empty() {
                    return Err(bad_request_error("mcp process args must not be empty"));
                }
                normalized_args.push(trimmed.to_string());
            }

            let mut normalized_env = std::collections::HashMap::with_capacity(env.len());
            for (key, value) in env {
                let trimmed_key = key.trim();
                if trimmed_key.is_empty() {
                    return Err(bad_request_error(
                        "mcp environment variable key is required",
                    ));
                }
                normalized_env.insert(trimmed_key.to_string(), value);
            }

            (
                trimmed_name.to_string(),
                MayMcpServerConfig {
                    command: Some(trimmed_command.to_string()),
                    args: normalized_args,
                    url: None,
                    env: normalized_env,
                },
            )
        }
        McpCreateRequest::Url { name, url } => {
            let trimmed_name = name.trim();
            if trimmed_name.is_empty() {
                return Err(bad_request_error("mcp server name is required"));
            }
            let trimmed_url = url.trim();
            if trimmed_url.is_empty() {
                return Err(bad_request_error("mcp server url is required"));
            }
            let parsed_url = url::Url::parse(trimmed_url)
                .map_err(|error| bad_request_error(error.to_string()))?;
            (
                trimmed_name.to_string(),
                MayMcpServerConfig {
                    command: None,
                    args: Vec::new(),
                    url: Some(parsed_url.to_string()),
                    env: std::collections::HashMap::new(),
                },
            )
        }
    };
    state
        .mcp()
        .add_server(name, config)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn remove_mcp_server(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let name = params.get("name").map_err(bad_request_error)?;
    state
        .mcp()
        .remove_server(name.to_string())
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn reconnect_mcp_server(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let name = params.get("name").map_err(bad_request_error)?;
    state
        .mcp()
        .reconnect(name.to_string())
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

// ── Skills ────────────────────────────────────────

pub(super) async fn list_skills(
    Query(query): Query<SkillScopeQuery>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<SkillInfoDto>>> {
    Ok(Json(
        list_skills_for_profile(state.as_ref(), query.profile_id.as_str()).await?,
    ))
}

pub(super) async fn install_skill(
    Query(query): Query<SkillScopeQuery>,
    Json(req): Json<SkillInstallRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<SkillInfoDto>>> {
    let repo = SkillManager::parse_skill_repo(&req.repo, &state.config_snapshot())
        .map_err(bad_request_error)?;
    let auth = match state.forge_auth.git_https_auth(repo.host().as_str()).await {
        Ok(auth) => Some(auth),
        // An unauthenticated host still installs public repositories.
        Err(crate::forge::ForgeAuthError::Unauthenticated { .. }) => None,
        Err(error) => return Err(internal_error(error)),
    };
    let installed = scoped_skill_manager(&state, query.profile_id.as_str())
        .install_from_forge(&repo, auth)
        .await
        .map_err(internal_error)?;
    Ok(Json(installed.into_iter().map(map_skill).collect()))
}

pub(super) async fn delete_skill(
    params: Params,
    Query(query): Query<SkillScopeQuery>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let dir = params.get("dir").map_err(bad_request_error)?;
    scoped_skill_manager(&state, query.profile_id.as_str())
        .delete_skill(dir)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

// ── Copilot ────────────────────────────────────────

pub(super) async fn copilot_status() -> SkyResult<Json<CopilotStatusResponse>> {
    Ok(Json(CopilotStatusResponse {
        authenticated: has_copilot_token(),
    }))
}

pub(super) async fn copilot_start() -> SkyResult<Json<CopilotStartResponse>> {
    let device = aither_copilot::auth::request_device_code()
        .await
        .map_err(internal_error)?;
    Ok(Json(CopilotStartResponse {
        device_code: device.device_code,
        user_code: device.user_code,
        verification_uri: device.verification_uri,
        interval: device.interval,
    }))
}

pub(super) async fn copilot_stream(
    Query(query): Query<CopilotAuthStreamQuery>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Sse> {
    let device_code = query.device_code.trim().to_string();
    if device_code.is_empty() {
        return Err(bad_request_error("deviceCode is required"));
    }
    if query.interval == 0 {
        return Err(bad_request_error("interval must be greater than 0"));
    }

    let poll_interval = Duration::from_secs(query.interval);
    let heartbeat_interval = Duration::from_secs(15);
    let (sender, sse) = Sse::channel();

    state.tokio_handle.spawn(async move {
        let mut poll_tick = tokio::time::interval(poll_interval);
        poll_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut heartbeat = tokio::time::interval(heartbeat_interval);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let _ = poll_tick.tick().await;
        let _ = heartbeat.tick().await;

        if !send_event(
            &sender,
            "copilot_auth",
            &CopilotAuthEvent {
                status: CopilotAuthStatus::Pending,
                message: None,
            },
        )
        .await
        {
            return;
        }

        loop {
            tokio::select! {
                _ = poll_tick.tick() => {
                    let event = poll_copilot_auth_event(device_code.as_str()).await;
                    let is_terminal = !matches!(event.status, CopilotAuthStatus::Pending);
                    if !send_event(&sender, "copilot_auth", &event).await {
                        break;
                    }
                    if is_terminal {
                        break;
                    }
                }
                _ = heartbeat.tick() => {
                    if !send_event(&sender, "heartbeat", &SseHeartbeat { ok: true }).await {
                        break;
                    }
                }
            }
        }
    });

    Ok(sse)
}

async fn poll_copilot_auth_event(device_code: &str) -> CopilotAuthEvent {
    match aither_copilot::auth::try_get_token(device_code).await {
        Ok(token) => match save_copilot_token(&token).await {
            Ok(()) => CopilotAuthEvent {
                status: CopilotAuthStatus::Success,
                message: None,
            },
            Err(error) => CopilotAuthEvent {
                status: CopilotAuthStatus::Error,
                message: Some(error.to_string()),
            },
        },
        Err(aither_copilot::CopilotError::AuthorizationPending) => CopilotAuthEvent {
            status: CopilotAuthStatus::Pending,
            message: None,
        },
        Err(aither_copilot::CopilotError::DeviceCodeExpired) => CopilotAuthEvent {
            status: CopilotAuthStatus::Expired,
            message: Some("Device code expired. Please restart authentication.".to_string()),
        },
        Err(aither_copilot::CopilotError::AccessDenied) => CopilotAuthEvent {
            status: CopilotAuthStatus::Denied,
            message: Some("Authorization denied by user.".to_string()),
        },
        Err(err) => CopilotAuthEvent {
            status: CopilotAuthStatus::Error,
            message: Some(err.to_string()),
        },
    }
}

pub(super) async fn copilot_logout() -> SkyResult<Json<ApiHealth>> {
    clear_copilot_credentials().await.map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

const CONFIG_WATCH_DEBOUNCE: Duration = Duration::from_millis(60);

fn configs_equal(left: &MayConfig, right: &MayConfig) -> bool {
    match (serde_json::to_value(left), serde_json::to_value(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Reloads `<may_dir>/config.toml` from disk into `state` when valid and
/// different from the active snapshot, broadcasting `UiEventTopics::CONFIG`.
/// Invalid edits are logged and ignored so the daemon keeps serving.
pub(super) async fn reload_config_from_disk(state: &Arc<DaemonState>) {
    let config_path = state.may_dir.join("config.toml");
    let loaded = match MayConfig::load_from(&config_path).await {
        Ok(loaded) => loaded,
        Err(error) => {
            tracing::warn!(
                path = %config_path.display(),
                %error,
                "ignoring invalid config.toml edit; keeping previous configuration"
            );
            return;
        }
    };
    if let Err(error) = loaded.preferences.network_policy.compile() {
        tracing::warn!(
            path = %config_path.display(),
            %error,
            "ignoring config.toml edit with invalid network policy; keeping previous configuration"
        );
        return;
    }

    let previous_config = state.config_snapshot();
    if configs_equal(&previous_config, &loaded) {
        return;
    }

    let previous_browser_manager = state.browser_manager();
    let codec = loaded.stream.codec;
    let codec_changed = codec != previous_config.stream.codec;
    let mut browser_updated = false;
    let next_browser_manager = if loaded.browser.enabled {
        match &previous_browser_manager {
            Some(manager) if loaded.browser == previous_config.browser => {
                if codec_changed
                    && let Err(error) = manager.set_codec(codec).await
                {
                    tracing::warn!(%error, "failed to update browser stream codec after config.toml reload");
                }
                Some(manager.clone())
            }
            _ => match crate::container::browser::BrowserManager::new(
                loaded.browser.clone(),
                state.may_dir.clone(),
                crate::container::browser::BrowserLifecyclePublisher::new(
                    state.browser_lifecycle_tx.clone(),
                ),
                codec,
            ) {
                Ok(manager) => {
                    browser_updated = true;
                    Some(Arc::new(manager))
                }
                Err(error) => {
                    tracing::warn!(%error, "failed to initialize browser manager after config.toml reload");
                    previous_browser_manager.clone()
                }
            },
        }
    } else {
        browser_updated = previous_browser_manager.is_some();
        None
    };

    if codec_changed
        && state.computer.is_running()
        && let Err(error) = state
            .computer
            .request(AgentRequest::SetStreamCodec { codec })
            .await
    {
        tracing::warn!(%error, "failed to update computer stream codec after config.toml reload");
    }

    state.replace_config(loaded.clone());
    state.live_limits.apply(&loaded);
    state.job_server.resize(loaded.sandbox.job_pool_size());
    if let Some(previous_browser_manager) = previous_browser_manager
        && !next_browser_manager
            .as_ref()
            .is_some_and(|next| Arc::ptr_eq(next, &previous_browser_manager))
        && let Err(error) = previous_browser_manager.stop().await
    {
        tracing::warn!(%error, "failed to stop previous browser manager after config.toml reload");
    }
    state.set_browser_manager(next_browser_manager);
    if let Err(error) = state
        .task_runtime
        .set_network_policy(&loaded.preferences.network_policy)
    {
        tracing::warn!(%error, "failed to apply reloaded network policy");
    }
    if let Err(error) = super::remote_access::sync_remote_access_from_state(state).await {
        tracing::warn!(%error, "failed to sync remote access after config.toml reload");
    }
    let mut topics = UiEventTopics::CONFIG;
    if browser_updated {
        topics |= UiEventTopics::BROWSER_STATUS;
    }
    state.notify_ui_events(topics);
}

/// Watches `<may_dir>/config.toml` for external edits (including atomic editor
/// renames and file creation/removal) and reloads configuration live without
/// restarting the daemon.
pub(super) fn spawn_config_file_watcher(state: &Arc<DaemonState>) {
    if let Err(error) = crate::session::ensure_private_dir_sync(&state.may_dir) {
        tracing::warn!(
            path = %state.may_dir.display(),
            %error,
            "config watcher could not ensure may_dir"
        );
        return;
    }
    let (watch_tx, watch_rx) = async_channel::unbounded::<notify::Result<NotifyEvent>>();
    let mut watcher = match RecommendedWatcher::new(
        move |event| {
            let _ = watch_tx.send_blocking(event);
        },
        NotifyConfig::default().with_poll_interval(Duration::from_millis(200)),
    ) {
        Ok(watcher) => watcher,
        Err(error) => {
            tracing::warn!(%error, "config.toml watcher unavailable");
            return;
        }
    };
    if let Err(error) = watcher.watch(&state.may_dir, RecursiveMode::NonRecursive) {
        tracing::warn!(
            path = %state.may_dir.display(),
            %error,
            "config.toml watcher failed to watch may_dir"
        );
        return;
    }

    let weak_state = Arc::downgrade(state);
    let shutdown = state.daemon_shutdown.clone();
    state.tokio_handle.spawn(async move {
        let _watcher = watcher;
        let mut dirty = false;
        enum WatchStep {
            Shutdown,
            Event(Option<notify::Result<NotifyEvent>>),
            QuietElapsed,
        }
        loop {
            let step = if dirty {
                futures_lite::future::or(
                    async {
                        shutdown.notified().await;
                        WatchStep::Shutdown
                    },
                    futures_lite::future::or(
                        async { WatchStep::Event(watch_rx.recv().await.ok()) },
                        async {
                            async_io::Timer::after(CONFIG_WATCH_DEBOUNCE).await;
                            WatchStep::QuietElapsed
                        },
                    ),
                )
                .await
            } else {
                futures_lite::future::or(
                    async {
                        shutdown.notified().await;
                        WatchStep::Shutdown
                    },
                    async { WatchStep::Event(watch_rx.recv().await.ok()) },
                )
                .await
            };

            match step {
                WatchStep::Shutdown | WatchStep::Event(None) => break,
                WatchStep::Event(Some(Ok(event))) => {
                    let touches_config = event.paths.iter().any(|path| {
                        path.file_name().is_some_and(|name| name == "config.toml")
                    });
                    if touches_config {
                        dirty = true;
                    }
                }
                WatchStep::Event(Some(Err(error))) => {
                    tracing::warn!(%error, "config.toml watcher error");
                }
                WatchStep::QuietElapsed => {
                    dirty = false;
                    let Some(state) = weak_state.upgrade() else {
                        break;
                    };
                    crate::panic_boundary::round("config.toml watcher", async {
                        reload_config_from_disk(&state).await;
                    })
                    .await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{build_ui_full_config, provider_map_from_ui};
    use crate::cache_lifetime::CacheLifetime;
    use crate::config::{CommandPriority, MayConfig, ProviderInstance, ProviderType};
    use std::time::Duration;

    #[test]
    fn full_config_save_roundtrip_preserves_cache_lifetime_and_compaction_model() {
        let mut config = MayConfig::default();
        for (id, lifetime) in [
            ("default", CacheLifetime::default()),
            ("timed", CacheLifetime::Limited(Duration::from_mins(30))),
            ("forever", CacheLifetime::Permanent),
        ] {
            let mut instance = ProviderInstance::new(ProviderType::Custom)
                .with_base_url("http://127.0.0.1:8787/v1");
            instance.cache_lifetime = lifetime;
            config.upsert_provider(id, instance);
        }
        config.models.compaction_model = Some("default::small".parse().expect("selector"));

        // The web UI posts the GET response back as the save body.
        let ui = serde_json::to_value(build_ui_full_config(&config)).expect("ui config serializes");
        let providers = ui["providers"].as_array().expect("providers");
        let wire = |id: &str| {
            providers
                .iter()
                .find(|provider| provider["id"] == id)
                .map(|provider| provider["cacheLifetime"].clone())
                .expect("provider on the wire")
        };
        assert_eq!(wire("default"), "1h");
        assert_eq!(wire("timed"), "30m");
        assert_eq!(wire("forever"), "permanent");
        assert_eq!(ui["models"]["compactionModel"], "default::small");

        let saved: super::UiFullConfig =
            serde_json::from_value(ui).expect("ui config deserializes");
        let providers = provider_map_from_ui(&config.provider, &saved.providers);
        for (id, instance) in &config.provider {
            assert_eq!(
                providers[id].cache_lifetime, instance.cache_lifetime,
                "{id}"
            );
        }
    }

    #[test]
    fn full_config_carries_the_aux_settings_and_tolerates_their_absence() {
        let mut config = MayConfig::default();
        config.suggestions.enabled = false;
        config.recap.auto = false;
        config.recap.idle_minutes = 7;
        let mut ui = serde_json::to_value(build_ui_full_config(&config)).expect("serializes");
        assert_eq!(ui["suggestions"]["enabled"], false);
        assert_eq!(ui["recap"]["auto"], false);
        assert_eq!(ui["recap"]["idleMinutes"], 7);

        // A client that predates the sections leaves them as they are.
        let object = ui.as_object_mut().expect("object");
        object.remove("suggestions");
        object.remove("recap");
        let saved: super::UiFullConfig = serde_json::from_value(ui).expect("deserializes");
        assert!(saved.suggestions.is_none() && saved.recap.is_none());
    }

    #[test]
    fn full_config_without_cache_lifetime_saves_the_default() {
        let ui = serde_json::to_value(build_ui_full_config(&{
            let mut config = MayConfig::default();
            config.upsert_provider("p", ProviderInstance::new(ProviderType::OpenAI));
            config
        }))
        .expect("ui config serializes");
        let mut ui = ui;
        ui["providers"][0]
            .as_object_mut()
            .expect("provider object")
            .remove("cacheLifetime");
        let saved: super::UiFullConfig =
            serde_json::from_value(ui).expect("an older client's body deserializes");
        assert!(saved.providers[0].cache_lifetime.is_default());
    }

    #[test]
    fn full_config_save_roundtrip_preserves_native_tools() {
        let mut config = MayConfig::default();
        let mut instance = ProviderInstance::new(ProviderType::OpenAI).with_api_key("sk-test");
        instance.native_tools.web_search = true;
        instance.native_tools.web_fetch = true;
        config.upsert_provider("openai", instance);

        // Simulate the web UI save flow: GET response serialized to the wire,
        // then POSTed back verbatim as the save body.
        let ui = serde_json::to_value(build_ui_full_config(&config)).expect("ui config serializes");
        let saved: super::UiFullConfig =
            serde_json::from_value(ui).expect("ui config deserializes");

        let providers = provider_map_from_ui(&config.provider, &saved.providers);
        let openai = providers.get("openai").expect("provider survives a save");
        assert!(openai.native_tools.web_search);
        assert!(openai.native_tools.web_fetch);
        assert_eq!(openai.api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn full_config_save_roundtrip_preserves_responses_api() {
        let mut config = MayConfig::default();
        let mut instance =
            ProviderInstance::new(ProviderType::Custom).with_base_url("http://127.0.0.1:8787/v1");
        instance.api = Some(crate::config::ProviderApi::Responses);
        instance.native_tools.web_search = true;
        config.upsert_provider("relay", instance);

        let ui = serde_json::to_value(build_ui_full_config(&config)).expect("ui config serializes");
        assert_eq!(ui["providers"][0]["api"], "responses");

        let saved: super::UiFullConfig =
            serde_json::from_value(ui).expect("ui config deserializes");
        let providers = provider_map_from_ui(&config.provider, &saved.providers);
        let relay = providers.get("relay").expect("provider survives a save");
        assert_eq!(relay.api, Some(crate::config::ProviderApi::Responses));
        assert!(relay.native_tools.web_search);
        relay
            .validate("relay")
            .expect("the saved provider stays valid");
    }

    #[test]
    fn full_config_roundtrips_a_chatgpt_provider() {
        let mut config = MayConfig::default();
        let mut instance = ProviderInstance::new(ProviderType::Chatgpt);
        instance.native_tools.web_search = true;
        config.upsert_provider("chatgpt", instance);

        let ui = serde_json::to_value(build_ui_full_config(&config)).expect("ui config serializes");
        assert_eq!(ui["providers"][0]["providerType"], "chatgpt");
        assert_eq!(ui["providers"][0]["nativeTools"]["webSearch"], true);
        assert_eq!(ui["providers"][0]["nativeTools"]["webFetch"], false);

        let saved: super::UiFullConfig =
            serde_json::from_value(ui).expect("ui config deserializes");
        let providers = provider_map_from_ui(&config.provider, &saved.providers);
        let chatgpt = providers.get("chatgpt").expect("provider survives a save");
        assert_eq!(chatgpt.resolve_type("chatgpt"), ProviderType::Chatgpt);
        assert!(chatgpt.native_tools.web_search);
        assert!(!chatgpt.native_tools.web_fetch);
    }

    #[test]
    fn full_config_serializes_sandbox_command_priority() {
        let ui = serde_json::to_value(build_ui_full_config(&MayConfig::default()))
            .expect("ui config serializes");
        assert_eq!(ui["sandbox"]["commandPriority"], "normal");

        let mut config = MayConfig::default();
        config.sandbox.command_priority = CommandPriority::Lowest;
        let ui = serde_json::to_value(build_ui_full_config(&config)).expect("ui config serializes");
        assert_eq!(ui["sandbox"]["commandPriority"], "lowest");
    }

    #[test]
    fn config_response_reports_key_presence_without_key_material() {
        let mut config = MayConfig::default();
        config.upsert_provider(
            "openai",
            ProviderInstance::new(ProviderType::OpenAI).with_api_key("sk-test"),
        );

        let ui = serde_json::to_value(build_ui_full_config(&config)).expect("ui config serializes");
        let provider = &ui["providers"][0];
        assert_eq!(provider["hasApiKey"], true);
        assert!(
            provider.get("apiKey").is_none(),
            "the config response must not carry key material"
        );
        assert!(!ui.to_string().contains("sk-test"));
    }

    #[test]
    fn config_save_without_a_new_key_keeps_the_stored_key() {
        let mut config = MayConfig::default();
        config.upsert_provider(
            "openai",
            ProviderInstance::new(ProviderType::OpenAI).with_api_key("sk-test"),
        );

        let ui = serde_json::to_value(build_ui_full_config(&config)).expect("ui config serializes");
        let saved: super::UiFullConfig =
            serde_json::from_value(ui).expect("ui config deserializes");

        let providers = provider_map_from_ui(&config.provider, &saved.providers);
        assert_eq!(
            providers["openai"].api_key.as_deref(),
            Some("sk-test"),
            "a save without key material keeps the stored key"
        );

        let mut cleared = saved.providers.clone();
        cleared[0].api_key = Some(crate::daemon::types::SecretUpdate::Remove);
        let providers = provider_map_from_ui(&config.provider, &cleared);
        assert!(
            providers["openai"].api_key.is_none(),
            "a remove clears the stored key"
        );

        let mut replaced = saved.providers;
        replaced[0].api_key = Some(crate::daemon::types::SecretUpdate::Replace(
            "sk-new".to_string(),
        ));
        let providers = provider_map_from_ui(&config.provider, &replaced);
        assert_eq!(
            providers["openai"].api_key.as_deref(),
            Some("sk-new"),
            "newly entered key material replaces the stored key"
        );
    }

    #[test]
    fn full_config_carries_the_stream_codec() {
        let ui = serde_json::to_value(build_ui_full_config(&MayConfig::default()))
            .expect("ui config serializes");
        assert_eq!(ui["stream"]["codec"], "auto");

        let mut config = MayConfig::default();
        config.stream.codec = crate::config::StreamCodec::Av1;
        let ui = serde_json::to_value(build_ui_full_config(&config)).expect("ui config serializes");
        assert_eq!(ui["stream"]["codec"], "av1");
        let saved: super::UiFullConfig = serde_json::from_value(ui).expect("deserializes");
        assert_eq!(saved.stream.codec, crate::config::StreamCodec::Av1);
    }

    #[test]
    fn full_config_rejects_an_unknown_command_priority() {
        let mut ui = serde_json::to_value(build_ui_full_config(&MayConfig::default()))
            .expect("ui config serializes");
        ui["sandbox"]["commandPriority"] = serde_json::json!("urgent");
        assert!(
            serde_json::from_value::<super::UiFullConfig>(ui).is_err(),
            "an unknown priority must be rejected"
        );
    }

    #[tokio::test]
    async fn invalid_full_config_returns_400_without_mutating_live_state() {
        use skyzen::HttpError as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut initial = MayConfig::default();
        initial.max_iterations = 42;
        initial
            .save_to(&dir.path().join("config.toml"))
            .await
            .expect("save initial");
        let state = std::sync::Arc::new(
            super::DaemonState::for_test(dir.path(), initial.clone())
                .await
                .expect("daemon state"),
        );

        let mut bad_selector = build_ui_full_config(&initial);
        bad_selector.models.advanced = vec!["invalid-without-separator".to_string()];
        bad_selector.max_iterations = 999;
        let err = super::run_save_full_config(state.clone(), bad_selector)
            .await
            .expect_err("invalid model selector must return 400");
        assert_eq!(err.status(), skyzen::StatusCode::BAD_REQUEST);
        assert_eq!(
            state.config_snapshot().max_iterations,
            42,
            "failed save must not mutate live DaemonState::config"
        );

        let mut bad_compaction = build_ui_full_config(&initial);
        bad_compaction.models.compaction_model = Some("bad_selector".to_string());
        bad_compaction.max_iterations = 777;
        let err = super::run_save_full_config(state.clone(), bad_compaction)
            .await
            .expect_err("invalid compaction selector must return 400");
        assert_eq!(err.status(), skyzen::StatusCode::BAD_REQUEST);
        assert_eq!(state.config_snapshot().max_iterations, 42);
    }

    #[tokio::test]
    async fn external_config_toml_edits_reload_live_and_emit_config_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let mut initial = MayConfig::default();
        initial.max_iterations = 15;
        initial
            .save_to(&config_path)
            .await
            .expect("initial config.toml");

        let state = std::sync::Arc::new(
            super::DaemonState::for_test(dir.path(), initial)
                .await
                .expect("daemon state"),
        );
        let mut rx = state.ui_event_bus.subscribe();
        super::spawn_config_file_watcher(&state);

        // External atomic editor save (write temp file + rename over config.toml).
        let mut edited = MayConfig::default();
        edited.max_iterations = 88;
        edited.preferences.show_decode_speed = true;
        let tmp_path = dir.path().join(".config.toml.swp");
        async_fs::write(&tmp_path, toml::to_string_pretty(&edited).expect("toml"))
            .await
            .expect("write temp");
        async_fs::rename(&tmp_path, &config_path)
            .await
            .expect("atomic rename");

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(topics) = rx.recv().await
                    && topics.contains(crate::ui_event_bus::UiEventTopics::CONFIG)
                    && state.config_snapshot().max_iterations == 88
                {
                    break;
                }
            }
        })
        .await
        .expect("watcher should reload config.toml and emit CONFIG without restart");
        assert!(state.config_snapshot().preferences.show_decode_speed);

        // Malformed TOML must not crash the daemon or clobber the valid config.
        async_fs::write(&config_path, "max_iterations = [unterminated")
            .await
            .expect("write broken toml");
        super::reload_config_from_disk(&state).await;
        assert_eq!(
            state.config_snapshot().max_iterations,
            88,
            "invalid external config.toml edit must keep the previous valid snapshot"
        );
    }
}

#[cfg(test)]
mod allowed_logins_tests {
    use super::normalize_logins;
    use crate::config::RemoteAccessConfig;
    use crate::daemon::types::UiRemoteAccessConfig;

    #[test]
    fn normalization_trims_and_drops_blanks_and_repeats() {
        let input = [" a@x.com ", "", "b@x.com", "a@x.com", "  "].map(String::from);
        assert_eq!(normalize_logins(&input), ["a@x.com", "b@x.com"]);
    }

    #[test]
    fn allowed_logins_round_trip_through_the_ui_config() {
        let config = RemoteAccessConfig {
            allowed_logins: vec!["a@x.com".into(), "b@x.com".into()],
            ..RemoteAccessConfig::default()
        };
        let json = serde_json::to_value(UiRemoteAccessConfig::from(&config)).expect("serializes");
        assert_eq!(
            json["allowedLogins"],
            serde_json::json!(["a@x.com", "b@x.com"])
        );
        let back: UiRemoteAccessConfig = serde_json::from_value(json).expect("deserializes");
        assert_eq!(back.allowed_logins, config.allowed_logins);
    }
}
