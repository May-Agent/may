use std::sync::Arc;

use skyzen::extract::Query;
use skyzen::Result as SkyResult;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};

use crate::config::{MayConfig, ProviderInstance, SshServerConfig};
use crate::copilot::{clear_copilot_credentials, has_copilot_token, save_copilot_token};
use crate::mcp::MayMcpServerConfig;
use crate::session::MayHomePaths;
use crate::skill::SkillManager;

use super::error::{bad_request_error, internal_error};
use super::helpers::save_config_to_disk;
use super::message_mapping::map_skill;
use super::model_resolution::persist_last_model;
use super::state::UiServerState;
use super::types::{
    ApiHealth, CopilotPollRequest, CopilotPollResponse, CopilotStartResponse,
    CopilotStatusResponse, LastModelRequest, McpAddRequest, MemoryPayload, MemorySaveRequest,
    SkillInfoDto, SkillInstallRequest, SshServerRequest, UiAppearanceConfig, UiConfigResponse,
    UiFullConfig, UiModelsConfig, UiPlaywrightConfig, UiPreferencesConfig, UiProviderConfig,
    UiRemoteAccessConfig,
};

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SkillScopeQuery {
    profile_id: String,
}

fn scoped_skill_manager(state: &UiServerState, profile_id: &str) -> SkillManager {
    let home_paths = MayHomePaths::new(state.may_dir.clone());
    SkillManager::from_profile_home(&home_paths.profile_home(profile_id))
}

pub(super) async fn get_ui_config(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<UiConfigResponse>> {
    let config = state.config.read().await;
    Ok(Json(UiConfigResponse {
        ssh_servers: config.ssh_servers.clone(),
        max_iterations: config.max_iterations,
        ui_font: config.appearance.font.clone(),
        ui_theme: config.appearance.theme.clone(),
    }))
}

pub(super) async fn get_full_config(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<UiFullConfig>> {
    let config = state.config.read().await;
    Ok(Json(build_ui_full_config(&config)))
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
            advanced: config.models.advanced.clone(),
            balanced: config.models.balanced.clone(),
            fast: config.models.fast.clone(),
            image_generation: config.models.image_generation.clone(),
            model_providers: config.models.model_providers.clone(),
        },
        preferences: UiPreferencesConfig {
            load_balance_strategy: config.preferences.load_balance_strategy,
            warn_on_model_change: config.preferences.warn_on_model_change,
            show_token_usage: config.preferences.show_token_usage,
            strict_model_matching: config.preferences.strict_model_matching,
            network_policy_mode: config.preferences.network_policy.mode,
            network_policy_hosts: config.preferences.network_policy.hosts.clone(),
        },
        appearance: UiAppearanceConfig {
            font: config.appearance.font.clone(),
            theme: config.appearance.theme.clone(),
        },
        last_model: config.last_model.clone(),
        max_iterations: config.max_iterations,
        playwright: UiPlaywrightConfig {
            enabled: config.playwright.enabled,
            browser: config.playwright.browser,
            port: config.playwright.port,
            screencast_quality: config.playwright.screencast_quality,
            screencast_max_width: config.playwright.screencast_max_width,
        },
        remote_access: UiRemoteAccessConfig::from(&config.remote_access),
    }
}

pub(super) async fn save_full_config(
    Json(req): Json<UiFullConfig>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let updated = {
        let mut config = state.config.write().await;

        let mut provider_map = std::collections::HashMap::new();
        for p in req.providers {
            let mut instance = ProviderInstance::new(p.provider_type);
            instance.api_key = p.api_key.filter(|v| !v.trim().is_empty());
            instance.base_url = p.base_url.filter(|v| !v.trim().is_empty());
            instance.display_name = p.display_name.filter(|v| !v.trim().is_empty());
            instance.enabled = p.enabled;
            provider_map.insert(p.id, instance);
        }

        config.provider = provider_map;
        config.models.advanced = req.models.advanced;
        config.models.balanced = req.models.balanced;
        config.models.fast = req.models.fast;
        config.models.image_generation = req.models.image_generation;
        config.models.model_providers = req.models.model_providers;
        config.preferences.load_balance_strategy = req.preferences.load_balance_strategy;
        config.preferences.warn_on_model_change = req.preferences.warn_on_model_change;
        config.preferences.show_token_usage = req.preferences.show_token_usage;
        config.preferences.strict_model_matching = req.preferences.strict_model_matching;
        config.preferences.network_policy.mode = req.preferences.network_policy_mode;
        config.preferences.network_policy.hosts = req
            .preferences
            .network_policy_hosts
            .into_iter()
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
            .collect();
        config.appearance.font = req.appearance.font;
        config.appearance.theme = req.appearance.theme;
        config.last_model = req.last_model;
        config.max_iterations = req.max_iterations;
        config.playwright.enabled = req.playwright.enabled;
        config.playwright.browser = req.playwright.browser;
        config.playwright.port = req.playwright.port;
        config.playwright.screencast_quality = req.playwright.screencast_quality;
        config.playwright.screencast_max_width = req.playwright.screencast_max_width;
        config.remote_access.enabled = req.remote_access.enabled;
        config.remote_access.cloudflared_binary = req.remote_access.cloudflared_binary;
        config.remote_access.tunnel_token = req
            .remote_access
            .tunnel_token
            .filter(|value| !value.trim().is_empty());
        config.remote_access.public_url = req
            .remote_access
            .public_url
            .filter(|value| !value.trim().is_empty());
        config.remote_access.allow_local_environment = req.remote_access.allow_local_environment;
        config.clone()
    };

    updated
        .preferences
        .network_policy
        .compile()
        .map_err(bad_request_error)?;

    let ssh_servers = updated.ssh_servers.clone();
    save_config_to_disk(updated).await.map_err(internal_error)?;
    state.task_runtime.set_ssh_servers(ssh_servers);
    state
        .task_runtime
        .set_network_policy(state.config.read().await.preferences.network_policy.clone())
        .map_err(internal_error)?;
    super::remote_access::sync_remote_access_from_state(&state)
        .await
        .map_err(internal_error)?;
    state.notify_ui_events();
    Ok(Json(ApiHealth { ok: true }))
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
    let memory_path = MayHomePaths::new(state.may_dir.clone()).memory_file();
    let content = async_fs::read_to_string(memory_path)
        .await
        .map_err(internal_error)?;
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
    let memory_path = MayHomePaths::new(state.may_dir.clone()).memory_file();
    async_fs::write(memory_path, payload.content)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

// ── SSH ────────────────────────────────────────

pub(super) async fn add_ssh_server(
    Json(req): Json<SshServerRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let updated = {
        let mut config = state.config.write().await;
        config.upsert_ssh_server(SshServerConfig {
            name: req.name,
            target: req.target,
        });
        config.clone()
    };
    let ssh_servers = updated.ssh_servers.clone();
    save_config_to_disk(updated).await.map_err(internal_error)?;
    state.task_runtime.set_ssh_servers(ssh_servers);
    state.notify_ui_events();
    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn remove_ssh_server(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let name = params.get("name").map_err(bad_request_error)?;
    let updated = {
        let mut config = state.config.write().await;
        let _ = config.remove_ssh_server(name);
        config.clone()
    };
    let ssh_servers = updated.ssh_servers.clone();
    save_config_to_disk(updated).await.map_err(internal_error)?;
    state.task_runtime.set_ssh_servers(ssh_servers);
    state.notify_ui_events();
    Ok(Json(ApiHealth { ok: true }))
}

// ── MCP ────────────────────────────────────────

pub(super) async fn list_mcp_servers(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<Vec<crate::mcp::McpServerInfo>>> {
    let manager = state.mcp_manager.lock().await;
    Ok(Json(manager.server_info()))
}

pub(super) async fn add_mcp_server(
    Json(req): Json<McpAddRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let mut manager = state.mcp_manager.lock().await;
    let config = MayMcpServerConfig {
        command: req.command,
        args: req.args,
        url: req.url,
        env: std::collections::HashMap::new(),
    };
    manager
        .add_server(req.name, config)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn remove_mcp_server(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let name = params.get("name").map_err(bad_request_error)?;
    let mut manager = state.mcp_manager.lock().await;
    manager.remove_server(name).await.map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn reconnect_mcp_server(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let name = params.get("name").map_err(bad_request_error)?;
    let mut manager = state.mcp_manager.lock().await;
    manager.reconnect(name).await.map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

// ── Skills ────────────────────────────────────────

pub(super) async fn list_skills(
    Query(query): Query<SkillScopeQuery>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<Vec<SkillInfoDto>>> {
    let skills = scoped_skill_manager(&state, query.profile_id.as_str())
        .list_skills()
        .await
        .map_err(internal_error)?;
    Ok(Json(skills.into_iter().map(map_skill).collect::<Vec<_>>()))
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

pub(super) async fn copilot_poll(
    Json(req): Json<CopilotPollRequest>,
) -> SkyResult<Json<CopilotPollResponse>> {
    match aither_copilot::auth::try_get_token(&req.device_code).await {
        Ok(token) => {
            save_copilot_token(&token).await.map_err(internal_error)?;
            Ok(Json(CopilotPollResponse {
                status: "success".to_string(),
                message: None,
            }))
        }
        Err(aither_copilot::CopilotError::AuthorizationPending) => Ok(Json(CopilotPollResponse {
            status: "pending".to_string(),
            message: None,
        })),
        Err(aither_copilot::CopilotError::DeviceCodeExpired) => Ok(Json(CopilotPollResponse {
            status: "expired".to_string(),
            message: Some("Device code expired. Please restart authentication.".to_string()),
        })),
        Err(aither_copilot::CopilotError::AccessDenied) => Ok(Json(CopilotPollResponse {
            status: "denied".to_string(),
            message: Some("Authorization denied by user.".to_string()),
        })),
        Err(err) => Ok(Json(CopilotPollResponse {
            status: "error".to_string(),
            message: Some(err.to_string()),
        })),
    }
}

pub(super) async fn copilot_logout() -> SkyResult<Json<ApiHealth>> {
    clear_copilot_credentials().await.map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}
