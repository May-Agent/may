use std::sync::Arc;
use std::time::Duration;

use skyzen::Result as SkyResult;
use skyzen::extract::Query;
use skyzen::responder::Sse;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use tokio::time::MissedTickBehavior;

use crate::config::{MayConfig, ProviderInstance, SshServerConfig};
use crate::copilot::{clear_copilot_credentials, has_copilot_token, save_copilot_token};
use crate::mcp::MayMcpServerConfig;
use crate::model_selector::ModelSelector;
use crate::session::MayHomePaths;
use crate::skill::SkillManager;
use crate::ui_event_bus::UiEventTopics;

use super::error::{bad_request_error, internal_error};
use super::helpers::send_event;
use super::message_mapping::map_skill;
use super::model_resolution::persist_last_model;
use super::state::UiServerState;
use super::types::{
    ApiHealth, CopilotAuthEvent, CopilotAuthStatus, CopilotStartResponse, CopilotStatusResponse,
    LastModelRequest, McpCreateRequest, MemoryPayload, MemorySaveRequest, SkillInfoDto,
    SkillInstallRequest, SseHeartbeat, SshServerRequest, UiAppearanceConfig, UiBrowserConfig,
    UiConfigResponse, UiFullConfig, UiModelsConfig, UiPreferencesConfig, UiProviderConfig,
    UiRemoteAccessConfig, UiSandboxConfig,
};

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

fn scoped_skill_manager(state: &UiServerState, profile_id: &str) -> SkillManager {
    let home_paths = MayHomePaths::new(state.may_dir.clone());
    SkillManager::from_profile_home(&home_paths.profile_home(profile_id))
}

fn parse_selector_list(values: &[String]) -> Vec<ModelSelector> {
    values
        .iter()
        .map(|value| {
            value
                .parse::<ModelSelector>()
                .expect("ui config save must receive valid model selectors")
        })
        .collect()
}

pub(super) async fn list_skills_for_profile(
    state: &UiServerState,
    profile_id: &str,
) -> Result<Vec<SkillInfoDto>, skyzen::Error> {
    let skills = scoped_skill_manager(state, profile_id)
        .list_skills()
        .await
        .map_err(internal_error)?;
    Ok(skills.into_iter().map(map_skill).collect::<Vec<_>>())
}

pub(super) async fn get_ui_config(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<UiConfigResponse>> {
    let config = state.config_snapshot();
    Ok(Json(build_ui_config_response(&config)))
}

pub(super) fn build_ui_config_response(config: &MayConfig) -> UiConfigResponse {
    UiConfigResponse {
        ssh_servers: config.ssh_servers.clone(),
        max_iterations: config.max_iterations,
        ui_font: config.appearance.font.clone(),
        ui_theme: config.appearance.theme.clone(),
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
            api_key: instance.api_key.clone(),
            base_url: instance.base_url.clone(),
            display_name: instance.display_name.clone(),
            enabled: instance.enabled,
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
            embedding_model: config
                .models
                .embedding_model
                .as_ref()
                .map(ToString::to_string),
            embedding_dimensions: config.models.embedding_dimensions,
        },
        preferences: UiPreferencesConfig {
            warn_on_model_change: config.preferences.warn_on_model_change,
            show_token_usage: config.preferences.show_token_usage,
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
                crate::ui_server::types::UiSystemProfileConfig {
                    path: sp.path.display().to_string(),
                    copy_on_first_use: sp.copy_on_first_use,
                }
            }),
        },
        remote_access: UiRemoteAccessConfig::from(&config.remote_access),
        sandbox: UiSandboxConfig {
            command_priority: config.sandbox.command_priority,
        },
    }
}

pub(super) async fn save_full_config(
    Json(req): Json<UiFullConfig>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let tokio_handle = state.tokio_handle.clone();
    let response = tokio_handle
        .spawn({
            let state = state.clone();
            async move { run_save_full_config(state, req).await }
        })
        .await
        .map_err(|_| internal_error("config save task panicked before producing a response"))??;
    Ok(Json(response))
}

async fn run_save_full_config(
    state: Arc<UiServerState>,
    req: UiFullConfig,
) -> SkyResult<ApiHealth> {
    let previous_browser_manager = state.browser_manager();
    let next_browser_manager = if req.browser.enabled {
        let next_browser_config = crate::config::BrowserConfig {
            enabled: req.browser.enabled,
            backend: req.browser.backend,
            port: req.browser.port,
            remote_cdp_endpoint: req
                .browser
                .remote_cdp_endpoint
                .clone()
                .filter(|value| !value.trim().is_empty()),
            screencast_jpeg_quality: req.browser.screencast_jpeg_quality,
            encoder_initial_bitrate_kbps: req.browser.encoder_initial_bitrate_kbps,
            screencast_max_width: req.browser.screencast_max_width,
            screencast_max_height: req.browser.screencast_max_height,
            every_nth_frame: req.browser.every_nth_frame,
            stealth_enabled: req.browser.stealth_enabled,
            system_profile: req.browser.system_profile.as_ref().map(|sp| {
                crate::config::SystemProfileConfig {
                    path: std::path::PathBuf::from(&sp.path),
                    copy_on_first_use: sp.copy_on_first_use,
                }
            }),
        };
        next_browser_config.validate().map_err(bad_request_error)?;
        crate::container::browser::prepare_browser_runtime(&next_browser_config, &state.may_dir)
            .await
            .map_err(internal_error)?;
        Some(Arc::new(
            crate::container::browser::BrowserManager::new(
                next_browser_config,
                state.may_dir.clone(),
                crate::container::browser::BrowserLifecyclePublisher::new(
                    state.browser_lifecycle_tx.clone(),
                ),
            )
            .map_err(internal_error)?,
        ))
    } else {
        None
    };

    let updated = state.update_config(|config| {
        let mut provider_map = std::collections::HashMap::new();
        for p in &req.providers {
            let mut instance = ProviderInstance::new(p.provider_type);
            instance.api_key = p.api_key.clone().filter(|v| !v.trim().is_empty());
            instance.base_url = p.base_url.clone().filter(|v| !v.trim().is_empty());
            instance.display_name = p.display_name.clone().filter(|v| !v.trim().is_empty());
            instance.enabled = p.enabled;
            provider_map.insert(p.id.clone(), instance);
        }

        config.provider = provider_map;
        config.models.advanced = parse_selector_list(&req.models.advanced);
        config.models.balanced = parse_selector_list(&req.models.balanced);
        config.models.fast = parse_selector_list(&req.models.fast);
        config.models.image_generation = parse_selector_list(&req.models.image_generation);
        config.models.embedding_model = req
            .models
            .embedding_model
            .clone()
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                value
                    .parse::<ModelSelector>()
                    .expect("ui config save must receive valid model selectors")
            });
        config.models.embedding_dimensions = req.models.embedding_dimensions;
        config.preferences.warn_on_model_change = req.preferences.warn_on_model_change;
        config.preferences.show_token_usage = req.preferences.show_token_usage;
        config.preferences.network_policy.mode = req.preferences.network_policy_mode;
        config.preferences.network_policy.hosts = req
            .preferences
            .network_policy_hosts
            .iter()
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
            .collect();
        config.appearance.font.clone_from(&req.appearance.font);
        config.appearance.theme = req.appearance.theme;
        config.last_model = req.last_model.clone().map(|value| {
            value
                .parse::<ModelSelector>()
                .expect("ui config save must receive valid model selectors")
        });
        config.max_iterations = req.max_iterations;
        config.sandbox.command_priority = req.sandbox.command_priority;
        config.browser.enabled = req.browser.enabled;
        config.browser.backend = req.browser.backend;
        config.browser.port = req.browser.port;
        config.browser.remote_cdp_endpoint = req
            .browser
            .remote_cdp_endpoint
            .clone()
            .filter(|value| !value.trim().is_empty());
        config.browser.screencast_jpeg_quality = req.browser.screencast_jpeg_quality;
        config.browser.encoder_initial_bitrate_kbps = req.browser.encoder_initial_bitrate_kbps;
        config.browser.screencast_max_width = req.browser.screencast_max_width;
        config.browser.screencast_max_height = req.browser.screencast_max_height;
        config.browser.every_nth_frame = req.browser.every_nth_frame;
        config.browser.stealth_enabled = req.browser.stealth_enabled;
        config.browser.system_profile =
            req.browser
                .system_profile
                .as_ref()
                .map(|sp| crate::config::SystemProfileConfig {
                    path: std::path::PathBuf::from(&sp.path),
                    copy_on_first_use: sp.copy_on_first_use,
                });
        config.remote_access.enabled = req.remote_access.enabled;
        config
            .remote_access
            .cloudflared_binary
            .clone_from(&req.remote_access.cloudflared_binary);
        config.remote_access.tunnel_token = req
            .remote_access
            .tunnel_token
            .clone()
            .filter(|value| !value.trim().is_empty());
        config.remote_access.public_url = req
            .remote_access
            .public_url
            .clone()
            .filter(|value| !value.trim().is_empty());
        config.remote_access.allow_local_environment = req.remote_access.allow_local_environment;
    });

    updated
        .preferences
        .network_policy
        .compile()
        .map_err(bad_request_error)?;

    updated.save().await.map_err(internal_error)?;
    if let Some(previous_browser_manager) = previous_browser_manager {
        previous_browser_manager
            .stop()
            .await
            .map_err(internal_error)?;
    }
    state.set_browser_manager(next_browser_manager);
    state
        .task_runtime
        .set_network_policy(&state.config_snapshot().preferences.network_policy)
        .map_err(internal_error)?;
    super::remote_access::sync_remote_access_from_state(&state)
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::CONFIG | UiEventTopics::BROWSER_STATUS);
    Ok(ApiHealth { ok: true })
}

pub(super) async fn set_last_model(
    Json(req): Json<LastModelRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let model = req.model.trim();
    if model.is_empty() {
        return Err(bad_request_error("model is required"));
    }

    persist_last_model(&state, model)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn get_memory(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<MemoryPayload>> {
    let content = state
        .db()
        .get_global_memory()
        .await
        .map_err(internal_error)?
        .unwrap_or_default();
    Ok(Json(MemoryPayload { content }))
}

pub(super) async fn save_memory(
    Json(payload): Json<MemorySaveRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    if !payload.explicit_user_request {
        return Err(bad_request_error(
            "memory updates require explicit user request confirmation",
        ));
    }
    state
        .db()
        .set_global_memory(&payload.content)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

// ── SSH ────────────────────────────────────────

pub(super) async fn add_ssh_server(
    Json(req): Json<SshServerRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let updated = state.update_config(|config| {
        config.upsert_ssh_server(SshServerConfig {
            name: req.name,
            target: req.target,
        });
    });
    updated.save().await.map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::CONFIG);
    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn remove_ssh_server(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let name = params.get("name").map_err(bad_request_error)?;
    let updated = state.update_config(|config| {
        let _ = config.remove_ssh_server(name);
    });
    updated.save().await.map_err(internal_error)?;
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
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<Vec<crate::mcp::McpServerInfo>>> {
    Ok(Json(state.mcp().server_info().await))
}

pub(super) async fn add_mcp_server(
    Json(req): Json<McpCreateRequest>,
    State(state): State<Arc<UiServerState>>,
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
    State(state): State<Arc<UiServerState>>,
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
    State(state): State<Arc<UiServerState>>,
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
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<Vec<SkillInfoDto>>> {
    Ok(Json(
        list_skills_for_profile(state.as_ref(), query.profile_id.as_str()).await?,
    ))
}

pub(super) async fn install_skill(
    Query(query): Query<SkillScopeQuery>,
    Json(req): Json<SkillInstallRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<Vec<SkillInfoDto>>> {
    let (owner, repo) = SkillManager::parse_github_repo(&req.repo).map_err(bad_request_error)?;
    let installed = scoped_skill_manager(&state, query.profile_id.as_str())
        .install_from_github(owner, repo)
        .await
        .map_err(internal_error)?;
    Ok(Json(installed.into_iter().map(map_skill).collect()))
}

pub(super) async fn delete_skill(
    params: Params,
    Query(query): Query<SkillScopeQuery>,
    State(state): State<Arc<UiServerState>>,
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
    State(state): State<Arc<UiServerState>>,
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

#[cfg(test)]
mod tests {
    use super::build_ui_full_config;
    use crate::config::{CommandPriority, MayConfig};

    #[test]
    fn full_config_serializes_sandbox_command_priority() {
        let ui = serde_json::to_value(build_ui_full_config(&MayConfig::default()))
            .expect("ui config serializes");
        assert_eq!(ui["sandbox"]["commandPriority"], "low");

        let mut config = MayConfig::default();
        config.sandbox.command_priority = CommandPriority::Lowest;
        let ui = serde_json::to_value(build_ui_full_config(&config)).expect("ui config serializes");
        assert_eq!(ui["sandbox"]["commandPriority"], "lowest");
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
}
