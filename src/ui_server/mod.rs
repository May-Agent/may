mod artifacts;
mod chat;
mod config;
mod helpers;
mod interaction;
mod sessions;
mod state;
mod types;

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::runtime::native;
use skyzen::utils::{Json, State};
use skyzen::Result as SkyResult;
#[cfg(feature = "ui")]
use skyzen::EmbeddedStaticDir;
use tokio::sync::Mutex;
use tokio::time::Instant as TokioInstant;

use crate::config::MayConfig;
use crate::mcp::{McpConfig, McpManager};
use crate::models::fetch_all_models;
use crate::session::SessionManager;
use crate::skill::SkillManager;
use aither_models::lookup as lookup_model_info;

use state::{AskUserApprover, UiServerState};
use types::{
    ApiHealth, ModelInfoItem, ModelsResponse, ProviderModelsItem, ProviderModelsResponse,
    RequestLogsResponse,
};

#[cfg(feature = "ui")]
static WEB_UI_DIST: include_dir::Dir<'static> =
    skyzen::embed_dir!(include_dir::include_dir!("$CARGO_MANIFEST_DIR/web-ui/dist"));

pub async fn run_ui_server(args: &[String]) -> anyhow::Result<()> {
    let host =
        helpers::parse_arg_value(args, "--host").unwrap_or_else(|| "127.0.0.1".to_string());
    let port = helpers::parse_arg_value(args, "--port")
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(8787);

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .with_context(|| format!("Invalid host/port: {host}:{port}"))?;

    // SAFETY: Called before any other threads are spawned. The skyzen framework
    // reads SKYZEN_ADDRESS at startup to discover the server's listen address.
    unsafe {
        std::env::set_var("SKYZEN_ADDRESS", addr.to_string());
    }

    let cwd = std::env::current_dir().context("Failed to get current working directory")?;
    let may_dir = MayConfig::may_dir().context("Failed to resolve ~/.may")?;

    let mut mcp_manager = McpManager::load().unwrap_or_else(|e| {
        tracing::warn!("Failed to load MCP config, using defaults: {}", e);
        McpManager::new(McpConfig::default())
    });
    if mcp_manager.has_servers() {
        let _ = mcp_manager.connect_all().await;
    }

    let permission_approver = crate::agent::PermissionApprover::new();
    let ask_user_approver = AskUserApprover::new();

    let config = MayConfig::load().context("Failed to load config")?;

    let state = Arc::new(UiServerState {
        session_manager: Mutex::new(SessionManager::new().await?),
        mcp_manager: Mutex::new(mcp_manager),
        skill_manager: SkillManager::new(&may_dir),
        permission_approver,
        ask_user_approver,
        active_streaming_sessions: Mutex::new(HashSet::new()),
        request_logs: Mutex::new(VecDeque::new()),
        models_cache: Mutex::new(None),
        config: tokio::sync::RwLock::new(config),
        cwd,
        may_dir,
    });

    let state_for_server = state.clone();
    tokio::task::spawn_blocking(move || {
        native::init_logging();
        native::launch(move || {
            let state = state_for_server.clone();
            async move { build_router(state) }
        });
    })
    .await
    .context("UI server task failed")?;

    Ok(())
}

fn build_router(state: Arc<UiServerState>) -> Router {
    use artifacts::*;
    use chat::*;
    use config::*;
    use interaction::*;
    use sessions::*;

    #[cfg(feature = "ui")]
    {
        return Route::new((
            "/api".route(Route::new((
                "/health".at(health),
                "/models".at(models),
                "/models/by-provider".at(models_by_provider),
                "/logs".route(Route::new(("/requests".at(get_request_logs),))),
                "/sessions".route(Route::new((
                    "".at(list_sessions),
                    "".post(create_session),
                    "/delete-all".post(delete_all_sessions),
                    "/{id}".put(update_session),
                    "/{id}".delete(delete_session),
                    "/{id}/runtime".at(get_session_runtime),
                    "/{id}/artifacts".at(list_session_artifacts),
                    "/{id}/artifacts/preview".at(get_artifact_preview),
                    "/{id}/artifacts/stream".at(stream_session_artifacts),
                ))),
                "/chat".route(Route::new(("".post(chat), "/stream".post(chat_stream)))),
                "/permissions".route(Route::new((
                    "/next".at(poll_permission_request),
                    "/respond".post(respond_permission_request),
                ))),
                "/ask-user".route(Route::new((
                    "/next".at(poll_ask_user_request),
                    "/respond".post(respond_ask_user_request),
                ))),
                "/config".route(Route::new((
                    "".at(get_ui_config),
                    "/stream".at(config_stream),
                    "/full".at(get_full_config),
                    "/full".post(save_full_config),
                ))),
                "/memory".at(get_memory),
                "/memory".post(save_memory),
                "/ssh".route(Route::new((
                    "".post(add_ssh_server),
                    "/{name}".delete(remove_ssh_server),
                ))),
                "/mcp".route(Route::new((
                    "".at(list_mcp_servers),
                    "".post(add_mcp_server),
                    "/{name}".delete(remove_mcp_server),
                    "/{name}/reconnect".post(reconnect_mcp_server),
                ))),
                "/skills".route(Route::new((
                    "".at(list_skills),
                    "/install".post(install_skill),
                    "/{dir}".delete(delete_skill),
                ))),
                "/copilot".route(Route::new((
                    "/status".at(copilot_status),
                    "/start".post(copilot_start),
                    "/poll".post(copilot_poll),
                    "/logout".post(copilot_logout),
                ))),
            ))),
            EmbeddedStaticDir::new("/", &WEB_UI_DIST),
        ))
        .middleware(State(state))
        .build();
    }

    #[cfg(not(feature = "ui"))]
    {
        Route::new((
            "/api".route(Route::new((
                "/health".at(health),
                "/models".at(models),
                "/models/by-provider".at(models_by_provider),
                "/logs".route(Route::new(("/requests".at(get_request_logs),))),
                "/sessions".route(Route::new((
                    "".at(list_sessions),
                    "".post(create_session),
                    "/delete-all".post(delete_all_sessions),
                    "/{id}".put(update_session),
                    "/{id}".delete(delete_session),
                    "/{id}/runtime".at(get_session_runtime),
                    "/{id}/artifacts".at(list_session_artifacts),
                    "/{id}/artifacts/preview".at(get_artifact_preview),
                    "/{id}/artifacts/stream".at(stream_session_artifacts),
                ))),
                "/chat".route(Route::new(("".post(chat), "/stream".post(chat_stream)))),
                "/permissions".route(Route::new((
                    "/next".at(poll_permission_request),
                    "/respond".post(respond_permission_request),
                ))),
                "/ask-user".route(Route::new((
                    "/next".at(poll_ask_user_request),
                    "/respond".post(respond_ask_user_request),
                ))),
                "/config".route(Route::new((
                    "".at(get_ui_config),
                    "/stream".at(config_stream),
                    "/full".at(get_full_config),
                    "/full".post(save_full_config),
                ))),
                "/memory".at(get_memory),
                "/memory".post(save_memory),
                "/ssh".route(Route::new((
                    "".post(add_ssh_server),
                    "/{name}".delete(remove_ssh_server),
                ))),
                "/mcp".route(Route::new((
                    "".at(list_mcp_servers),
                    "".post(add_mcp_server),
                    "/{name}".delete(remove_mcp_server),
                    "/{name}/reconnect".post(reconnect_mcp_server),
                ))),
                "/skills".route(Route::new((
                    "".at(list_skills),
                    "/install".post(install_skill),
                    "/{dir}".delete(delete_skill),
                ))),
                "/copilot".route(Route::new((
                    "/status".at(copilot_status),
                    "/start".post(copilot_start),
                    "/poll".post(copilot_poll),
                    "/logout".post(copilot_logout),
                ))),
            ))),
        ))
        .middleware(State(state))
        .build()
    }
}

// ── System / Meta handlers ────────────────────────────────

async fn health() -> SkyResult<Json<ApiHealth>> {
    Ok(Json(ApiHealth { ok: true }))
}

async fn get_request_logs(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<RequestLogsResponse>> {
    let logs = state
        .request_logs
        .lock()
        .await
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    Ok(Json(RequestLogsResponse { logs }))
}

async fn models(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ModelsResponse>> {
    const MODELS_CACHE_TTL: Duration = Duration::from_secs(20);

    {
        let cache = state.models_cache.lock().await;
        if let Some((at, cached)) = &*cache {
            if at.elapsed() < MODELS_CACHE_TTL {
                return Ok(Json(cached.clone()));
            }
        }
    }

    let config = state.config.read().await;

    let fetched = fetch_all_models(&config).await;
    let mut model_map = std::collections::HashMap::<String, ModelInfoItem>::new();

    for item in fetched {
        model_map.entry(item.id.clone()).or_insert(ModelInfoItem {
            id: item.id,
            nickname: item.nickname,
            context_window: item.context_window,
            abilities: item.abilities,
        });
    }

    for tier in crate::config::ModelTier::ALL {
        for model in config.models.models_for_tier(*tier) {
            model_map.entry(model.clone()).or_insert_with(|| {
                let fallback = lookup_model_info(model);
                ModelInfoItem {
                    id: model.clone(),
                    nickname: fallback
                        .map(|m| m.name.to_string())
                        .unwrap_or_else(|| model.clone()),
                    context_window: fallback.map(|m| m.context_window),
                    abilities: fallback
                        .map(|m| {
                            m.abilities
                                .iter()
                                .map(|a| crate::models::ability_to_label(*a))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                }
            });
        }
    }

    let mut details = model_map.into_values().collect::<Vec<_>>();
    details.sort_by(|a, b| a.id.cmp(&b.id));
    let models_list = details.iter().map(|d| d.id.clone()).collect::<Vec<_>>();
    let response = ModelsResponse {
        models: models_list,
        details,
    };

    let mut cache = state.models_cache.lock().await;
    *cache = Some((TokioInstant::now(), response.clone()));

    Ok(Json(response))
}

async fn models_by_provider(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ProviderModelsResponse>> {
    let config = state.config.read().await;
    let fetched = fetch_all_models(&config).await;
    let mut grouped: std::collections::HashMap<String, Vec<ModelInfoItem>> =
        std::collections::HashMap::new();

    for item in fetched {
        grouped
            .entry(item.provider_id)
            .or_default()
            .push(ModelInfoItem {
                id: item.id,
                nickname: item.nickname,
                context_window: item.context_window,
                abilities: item.abilities,
            });
    }

    let mut providers = grouped
        .into_iter()
        .map(|(provider_id, mut details)| {
            details.sort_by(|a, b| a.id.cmp(&b.id));
            details.dedup_by(|a, b| a.id == b.id);
            let models_list = details.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
            ProviderModelsItem {
                provider_id,
                models: models_list,
                details,
            }
        })
        .collect::<Vec<_>>();

    providers.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
    Ok(Json(ProviderModelsResponse { providers }))
}
