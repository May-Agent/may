mod artifacts;
mod attachments;
mod bash_parsing;
mod browser;
mod chat;
mod config;
mod error;
mod events;
mod helpers;
mod interaction;
mod local_url_mapper;
mod message_mapping;
mod model_resolution;
mod notifications;
mod profiles;
mod sessions;
mod state;
mod tasks;
mod title_generation;
mod types;

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
#[cfg(feature = "ui")]
use skyzen::EmbeddedStaticDir;
use skyzen::Result as SkyResult;
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::runtime::native;
use skyzen::utils::{Json, State};
use tokio::sync::Mutex;

use crate::config::MayConfig;
use crate::mcp::McpManager;
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
    skyzen::embed_dir!("$CARGO_MANIFEST_DIR/web-ui/dist");

pub async fn run_ui_server(args: &[String]) -> anyhow::Result<()> {
    let host = helpers::parse_arg_value(args, "--host").unwrap_or_else(|| "127.0.0.1".to_string());
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

    let mut mcp_manager = McpManager::load().context("failed to load MCP config")?;
    if mcp_manager.has_servers() {
        let _ = mcp_manager.connect_all().await;
    }

    let permission_approver = crate::agent::PermissionApprover::new();
    let ask_user_approver = AskUserApprover::new();
    let (register_subagents_broker, register_subagents_queue) =
        crate::agent::register_tool::subagents_channel();

    let config = helpers::load_config_from_disk()
        .await
        .context("Failed to load config")?;

    // Try to detect a container runtime (OrbStack/Docker/Podman)
    let container_manager = match crate::container::ContainerRuntime::detect().await {
        Ok(runtime) => {
            tracing::info!(kind = %runtime.kind, "Container runtime detected");
            Some(Arc::new(
                crate::container::lifecycle::ContainerManager::new(
                    Arc::new(runtime),
                    may_dir.clone(),
                ),
            ))
        }
        Err(e) => {
            tracing::debug!("No container runtime available: {e}");
            None
        }
    };

    // Initialize browser manager when Playwright is enabled
    let browser_manager = if config.playwright.enabled {
        tracing::info!("Playwright browser integration enabled");
        match crate::container::playwright::BrowserManager::new(config.playwright.clone()) {
            Ok(bm) => Some(Arc::new(bm)),
            Err(e) => {
                tracing::error!("Failed to initialize browser manager: {e}");
                None
            }
        }
    } else {
        None
    };

    let session_manager = SessionManager::new().await?;
    let db_for_archive = session_manager.db().clone();
    let (title_generation_queue, title_generation_rx) =
        crate::ui_server::state::TitleGenerationQueue::new();

    // Initialize memory processor with fast-tier provider chain
    let memory_processor = match crate::agent::build_fast_provider_chain(&config).await {
        Ok(memory_providers) => {
            tracing::info!("Profile memory processor enabled");
            Some(crate::memory::MemoryProcessor::new(
                db_for_archive.clone(),
                memory_providers,
            ))
        }
        Err(e) => {
            tracing::debug!("Profile memory processor unavailable (no fast model): {e}");
            None
        }
    };

    // Initialize Web Push notification manager
    let notification_manager =
        crate::notification::NotificationManager::new(&may_dir, db_for_archive.clone())
            .map_err(|e| anyhow!("failed to initialize web push notifications: {e}"))?;
    let notification_manager = Arc::new(notification_manager);
    tracing::info!("Web Push notifications enabled");

    let state = Arc::new(UiServerState {
        session_manager: Mutex::new(session_manager),
        mcp_manager: Mutex::new(mcp_manager),
        skill_manager: SkillManager::new(&may_dir),
        permission_approver,
        ask_user_approver,
        active_streaming_sessions: Mutex::new(HashSet::new()),
        request_logs: Mutex::new(VecDeque::new()),
        models_cache: state::RefreshableCache::new(Duration::from_secs(20)),
        provider_models_cache: state::RefreshableCache::new(Duration::from_secs(20)),
        config: tokio::sync::RwLock::new(config),
        ui_events_notify: Arc::new(tokio::sync::Notify::new()),
        container_prepare_statuses: Mutex::new(HashMap::new()),
        container_prepare_inflight: Mutex::new(HashSet::new()),
        container_manager,
        browser_manager,
        notification_manager: notification_manager.clone(),
        browser_takeover_session: Mutex::new(None),
        browser_takeover_alert_pending: Mutex::new(HashSet::new()),
        memory_processor,
        local_url_port_mapper: Arc::new(local_url_mapper::LocalUrlPortMapper::new()),
        title_generation_queue,
        register_subagents_broker,
        location_provider: crate::location::LocationProvider::new(),
        cwd,
        may_dir,
    });

    // Spawn background tasks
    let _auto_archive = crate::tasks::spawn_auto_archive(db_for_archive.clone());
    if let Some(ref cm) = state.container_manager {
        let _auto_stop = cm.spawn_auto_stop_task();
    }
    chat::spawn_register_subagents_worker(state.clone(), register_subagents_queue);
    title_generation::spawn_worker(state.clone(), title_generation_rx);
    let scheduler =
        crate::scheduler::TaskScheduler::new(db_for_archive.clone(), state.may_dir.clone());
    let scheduler = if let Some(ref manager) = state.container_manager {
        scheduler.with_container_manager(manager.clone())
    } else {
        scheduler
    };
    let scheduler = scheduler.with_notifier(notification_manager.clone());
    let _scheduler = scheduler.spawn();

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

fn api_routes() -> Route {
    use artifacts::*;
    use browser::*;
    use chat::*;
    use config::*;
    use events::*;
    use interaction::*;
    use notifications::*;
    use profiles::*;
    use sessions::*;
    use tasks::*;

    Route::new((
        Route::new((
            "/health".at(health),
            "/models".route(Route::new((
                "".at(models),
                "/by-provider".at(models_by_provider),
            ))),
            "/events".at(stream_events),
            "/logs".route(Route::new(("/requests".at(get_request_logs),))),
            "/sessions".route(Route::new((
                "".at(list_sessions),
                "".post(create_session),
                "/delete-all".post(delete_all_sessions),
                "/{id}".route(Route::new((
                    Route::new((
                        "".at(get_session),
                        "".put(update_session),
                        "".delete(delete_session),
                        "/runtime".at(get_session_runtime),
                        "/context-debug".at(get_session_context_debug),
                        "/artifacts".at(list_session_artifacts),
                        "/artifacts/preview".at(get_artifact_preview),
                        "/artifacts/slidev/preview".at(prepare_slidev_preview),
                        "/artifacts/slidev-static/{buildKey}/{*path}".at(get_slidev_static_asset),
                        "/outputs/preview".at(get_output_preview),
                        "/artifacts/local-url-map".post(map_container_local_url),
                    )),
                    Route::new((
                        "/artifacts/stream".at(stream_session_artifacts),
                        "/pin".post(pin_session),
                        "/unpin".post(unpin_session),
                        "/archive".post(archive_session),
                        "/unarchive".post(unarchive_session),
                        "/move-to-folder".post(move_to_folder),
                    )),
                ))),
            ))),
            "/profiles".route(Route::new((
                "".at(list_profiles),
                "".post(create_profile),
                "/{id}".put(update_profile),
                "/{id}".delete(delete_profile),
                "/{id}/sessions".at(get_profile_sessions),
                "/{id}/folders".post(create_folder),
            ))),
        )),
        Route::new((
            "/folders".route(Route::new((
                "/{id}".put(update_folder),
                "/{id}".delete(delete_folder),
            ))),
            "/chat".route(Route::new(("".post(chat), "/stream".post(chat_stream)))),
            "/permissions".route(Route::new(("/respond".post(respond_permission_request),))),
            "/ask-user".route(Route::new(("/respond".post(respond_ask_user_request),))),
            "/config".route(Route::new((
                "".at(get_ui_config),
                "/full".at(get_full_config),
                "/full".post(save_full_config),
                "/last-model".post(set_last_model),
            ))),
        )),
        Route::new((
            "/memory".route(Route::new(("".at(get_memory), "".post(save_memory)))),
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
            "/tasks".route(Route::new((
                "".at(list_tasks),
                "".post(create_task),
                "/{id}".at(get_task),
                "/{id}".put(update_task),
                "/{id}".delete(delete_task),
                "/{id}/approve".post(approve_task),
                "/{id}/run".post(run_task),
            ))),
            "/copilot".route(Route::new((
                "/status".at(copilot_status),
                "/start".post(copilot_start),
                "/poll".post(copilot_poll),
                "/logout".post(copilot_logout),
            ))),
            "/browser".route(Route::new((
                "/status".at(browser_status),
                "/start".post(browser_start),
                "/stop".post(browser_stop),
                "/offer".post(browser_offer),
                "/takeover/start".post(browser_takeover_start),
                "/takeover/stop".post(browser_takeover_stop),
            ))),
            "/push".route(Route::new((
                "/vapid-key".at(get_vapid_key),
                "/subscribe".post(subscribe),
                "/unsubscribe".post(unsubscribe),
                "/test".post(test_notification),
            ))),
        )),
    ))
}

fn build_router(state: Arc<UiServerState>) -> Router {
    let api = "/api".route(api_routes());

    #[cfg(feature = "ui")]
    {
        return Route::new((api, EmbeddedStaticDir::new("/", &WEB_UI_DIST)))
            .middleware(State(state))
            .build();
    }

    #[cfg(not(feature = "ui"))]
    {
        Route::new((api,)).middleware(State(state)).build()
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

async fn models(State(state): State<Arc<UiServerState>>) -> SkyResult<Json<ModelsResponse>> {
    let config = state.config.read().await.clone();
    let result = state
        .models_cache
        .get_or_refresh(move || async move {
            let fetched = fetch_all_models(&config).await;
            build_models_response(&config, fetched, true)
        })
        .await;
    Ok(Json(result))
}

fn build_models_response(
    config: &MayConfig,
    fetched: Vec<crate::models::FetchedModel>,
    include_static_metadata: bool,
) -> ModelsResponse {
    let mut model_map = std::collections::HashMap::<String, ModelInfoItem>::new();

    for item in fetched {
        let new_entry = ModelInfoItem {
            id: item.id.clone(),
            nickname: item.nickname,
            context_window: item.context_window,
            abilities: item.abilities,
        };
        model_map
            .entry(item.id)
            .and_modify(|existing| {
                // Prefer the entry with more abilities (richer metadata).
                if new_entry.abilities.len() > existing.abilities.len() {
                    *existing = new_entry.clone();
                }
            })
            .or_insert(new_entry);
    }

    for tier in crate::config::ModelTier::ALL {
        for model in config.models.models_for_tier(*tier) {
            model_map.entry(model.clone()).or_insert_with(|| {
                let static_info = if include_static_metadata {
                    lookup_model_info(model)
                } else {
                    None
                };
                let (nickname, context_window, abilities) = if let Some(info) = static_info {
                    (
                        info.id().to_string(),
                        info.max_input_tokens(),
                        info.abilities()
                            .iter()
                            .map(|a| crate::models::ability_to_label(*a))
                            .collect::<Vec<_>>(),
                    )
                } else {
                    (model.clone(), None, Vec::new())
                };
                ModelInfoItem {
                    id: model.clone(),
                    nickname,
                    context_window,
                    abilities,
                }
            });
        }
    }

    // Canonicalize nicknames: prefer aither_models as the single source of truth for
    // display names. This eliminates placeholder names from providers (e.g. Copilot
    // returning "OpenAI model" or "github") without needing a hardcoded blocklist.
    if include_static_metadata {
        for item in model_map.values_mut() {
            if let Some(info) = lookup_model_info(&item.id) {
                item.nickname = info.id().to_string();
            }
        }
    }

    let mut details = model_map.into_values().collect::<Vec<_>>();
    details.sort_by(|a, b| a.id.cmp(&b.id));
    let models_list = details.iter().map(|d| d.id.clone()).collect::<Vec<_>>();
    ModelsResponse {
        models: models_list,
        details,
    }
}

async fn models_by_provider(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ProviderModelsResponse>> {
    let config = state.config.read().await.clone();
    let result = state
        .provider_models_cache
        .get_or_refresh(move || async move {
            let fetched = fetch_all_models(&config).await;
            build_provider_models_response(fetched)
        })
        .await;
    Ok(Json(result))
}

fn build_provider_models_response(
    fetched: Vec<crate::models::FetchedModel>,
) -> ProviderModelsResponse {
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
    ProviderModelsResponse { providers }
}
