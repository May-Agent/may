//! The daemon's API server: state construction, background workers, and
//! the REST/SSE router every client (TUI, oneshot, web UI) talks to.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
#[cfg(feature = "ui")]
use skyzen::EmbeddedStaticDir;
use skyzen::Result as SkyResult;
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::utils::{Json, State};

use crate::config::MayConfig;
use crate::mcp::McpManager;
use crate::models::fetch_all_models;
use aither_models::lookup as lookup_model_info;

use super::error::internal_error;
use super::state::{AskUserApprover, DaemonState, WorkspaceApprover, tool_dispatch_channels};
use super::types::{
    ApiHealth, ModelInfoItem, ModelsResponse, ProviderModelsItem, ProviderModelsResponse,
    RequestLogCacheSummary, RequestLogEntry, RequestLogsResponse,
};

/// Applies the configured remote access, once the daemon listens on TCP.
///
/// # Errors
/// Fails when the profiles cannot be read.
pub async fn sync_remote_access(state: &DaemonState) -> anyhow::Result<()> {
    super::remote_access::sync_remote_access_from_state(state)
        .await
        .context("failed to sync remote access state")
}

#[cfg(feature = "ui")]
static WEB_UI_DIST: include_dir::Dir<'static> =
    skyzen::embed_dir!("$CARGO_MANIFEST_DIR/web-ui/dist");

/// HTTP server mode selected by the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonMode {
    /// Serve the REST API plus the embedded web UI.
    Ui,
    /// Serve the REST API only.
    Rest,
}

impl DaemonMode {
    #[cfg(not(feature = "ui"))]
    fn ensure_supported(self) -> anyhow::Result<()> {
        match self {
            Self::Rest => Ok(()),
            Self::Ui => Err(anyhow!(
                "`may ui` requires the `ui` cargo feature; use `may rest` for the REST API server"
            )),
        }
    }
}

/// Parses `--host` / `--port` from `may ui` / `may rest` arguments.
///
/// # Errors
/// Fails when the pair does not form a socket address.
pub fn parse_listen_args(args: &[String]) -> anyhow::Result<SocketAddr> {
    let host =
        super::helpers::parse_arg_value(args, "--host").unwrap_or_else(|| "127.0.0.1".to_string());
    let port = super::helpers::parse_arg_value(args, "--port")
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    format!("{host}:{port}")
        .parse()
        .with_context(|| format!("Invalid host/port: {host}:{port}"))
}

/// TCP port the daemon serves the API and web UI on by default.
pub const DEFAULT_PORT: u16 = 8787;

/// A started server: shared state with its background work running, and
/// the router that serves it.
pub struct StartedDaemon {
    /// Shared server state.
    pub(crate) state: Arc<DaemonState>,
    /// Router serving the API (and the web UI when embedded).
    pub(crate) router: Router,
}

/// Builds the server state, starts its background work (scheduler, title
/// worker, tool dispatchers, auto-archive, container auto-stop, remote
/// access) and returns the router to serve.
///
/// # Errors
/// Fails when configuration, persistence or a required service cannot be
/// initialized.
pub async fn start(mode: DaemonMode) -> anyhow::Result<StartedDaemon> {
    #[cfg(not(feature = "ui"))]
    mode.ensure_supported()?;

    let cwd = std::env::current_dir().context("Failed to get current working directory")?;
    let may_dir = MayConfig::may_dir().context("Failed to resolve ~/.may")?;

    let mcp_manager = McpManager::load()
        .await
        .context("failed to load MCP config")?;
    // Note: MCP connection is deferred until the service is spawned.
    // The McpService handles connection internally, avoiding startup blocking.

    let (tool_dispatchers, tool_queues) = tool_dispatch_channels();

    let config = MayConfig::load().await.context("Failed to load config")?;
    let tool_distribution = Arc::new(
        crate::tools::embedded_distribution().context("embedded tool distribution is invalid")?,
    );

    // Try to detect a container runtime (OrbStack/Docker/Podman)
    let container_manager = match crate::container::ContainerRuntime::detect().await {
        Ok(runtime) => {
            tracing::info!(kind = %runtime.kind, "Container runtime detected");
            Some(Arc::new(
                crate::container::lifecycle::ContainerManager::new(
                    Arc::new(runtime),
                    may_dir.clone(),
                    tool_distribution.clone(),
                ),
            ))
        }
        Err(e) => {
            tracing::debug!("No container runtime available: {e}");
            None
        }
    };

    // Capacity must be high enough that a slow SSE client cannot block the
    // browser actor's hot path; lifecycle events are small and high-volume
    // during Chrome boot / page navigations.
    let (browser_lifecycle_tx, _) = tokio::sync::broadcast::channel(256);

    // Initialize the May-managed browser runtime only when the feature is enabled.
    let browser_manager = if config.browser.enabled {
        tracing::info!(backend = ?config.browser.backend, "Browser integration enabled");
        match crate::container::browser::prepare_browser_runtime(&config.browser, &may_dir).await {
            Ok(()) => match crate::container::browser::BrowserManager::new(
                config.browser.clone(),
                may_dir.clone(),
                crate::container::browser::BrowserLifecyclePublisher::new(
                    browser_lifecycle_tx.clone(),
                ),
            ) {
                Ok(manager) => Some(Arc::new(manager)),
                Err(error) => {
                    tracing::error!("Failed to initialize browser manager: {error}");
                    None
                }
            },
            Err(error) => {
                tracing::error!("Failed to provision managed browser runtime: {error}");
                None
            }
        }
    } else {
        None
    };

    let (_session_store, db_for_archive, default_profile_id) =
        crate::session::initialize_session_persistence(&may_dir).await?;
    let (title_generation_queue, title_generation_rx) =
        crate::daemon::state::TitleGenerationQueue::new();
    let ui_event_bus = crate::ui_event_bus::UiEventBus::new();
    // Request handlers acquire runs from outside the tokio runtime, so the
    // lease tasks are spawned on its handle.
    let lease_handle = tokio::runtime::Handle::current();
    let runtime_service = Arc::new(
        crate::session::SessionRuntimeService::new(db_for_archive.clone(), ui_event_bus.clone())
            .with_spawner(Arc::new(move |task| {
                lease_handle.spawn(task);
            })),
    );
    runtime_service
        .runs()
        .recover_after_restart()
        .await
        .context("failed to reconcile persisted session runtime state after restart")?;
    let remote_access_manager = Arc::new(super::remote_access::RemoteAccessManager::new(
        ui_event_bus.clone(),
    ));
    let permission_approver =
        crate::agent::PermissionApprover::new().with_runtime_service(runtime_service.clone());
    let workspace_approver = WorkspaceApprover::new().with_runtime_service(runtime_service.clone());
    let ask_user_approver = AskUserApprover::new().with_runtime_service(runtime_service.clone());

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
            .await
            .map_err(|e| anyhow!("failed to initialize web push notifications: {e}"))?;
    let notification_manager = Arc::new(notification_manager);
    tracing::info!("Web Push notifications enabled");
    let notification_service = Arc::new(crate::notification::NotificationService::new(
        db_for_archive.clone(),
        notification_manager.clone(),
        ui_event_bus.clone(),
    ));
    notification_service
        .redeliver_pending()
        .await
        .map_err(|e| anyhow!(e.to_string()))?;
    let mut task_runtime = crate::scheduler::TaskRuntime::load(
        db_for_archive.clone(),
        may_dir.clone(),
        tool_distribution.clone(),
    )
    .await
    .context("failed to load task runtime state")?
    .with_notifier(notification_service.clone())
    .with_ui_event_bus(ui_event_bus.clone())
    .with_network_policy(&config.preferences.network_policy)
    .map_err(|error| anyhow!(error))?;
    if let Some(ref manager) = container_manager {
        task_runtime = task_runtime.with_container_manager(manager.clone());
    }
    let task_runtime = Arc::new(task_runtime);
    let container_prepare = super::state::ContainerPrepareTracker::new(ui_event_bus.clone());
    let (search_index_wake_tx, search_index_wake) = async_channel::bounded(1);

    let state = Arc::new(DaemonState {
        tokio_handle: tokio::runtime::Handle::current(),
        db: db_for_archive.clone(),
        default_profile_id,
        mcp_service: super::state::McpService::spawn(mcp_manager),
        permission_approver,
        workspace_approver,
        ask_user_approver,
        tool_dispatchers,
        fetched_models_cache: super::state::RefreshableCache::new(Duration::from_secs(20)),
        config: arc_swap::ArcSwap::from_pointee(config),
        ui_event_bus,
        container_prepare,
        container_manager,
        browser_manager: arc_swap::ArcSwapOption::from(browser_manager),
        browser_owner_session_id: arc_swap::ArcSwapOption::empty(),
        browser_lifecycle_tx,
        #[cfg(target_os = "macos")]
        session_displays: Arc::new(tokio::sync::Mutex::new(
            crate::computer::SessionDisplays::default(),
        )),
        #[cfg(target_os = "macos")]
        session_streams: Arc::new(tokio::sync::Mutex::new(
            crate::computer::SessionStreams::default(),
        )),
        remote_access_manager,
        notification_service: notification_service.clone(),
        memory_processor,
        local_url_port_mapper: Arc::new(super::local_url_mapper::LocalUrlPortMapper::new()),
        title_generation_queue,
        location_provider: crate::location::LocationProvider::new(),
        runtime_service,
        task_runtime: task_runtime.clone(),
        tool_distribution,
        tool_environments: crate::agent::ToolEnvironmentCache::spawn(),
        cwd,
        may_dir,
        search_index_wake_tx,
        search_index_wake,
        search_index_status: crate::repo_search::indexer::IndexerStatus::default(),
        daemon_shutdown: Arc::new(tokio::sync::Notify::new()),
        build_id: crate::daemon::current_build_id()
            .context("failed to identify the running build")?,
        turn_hub: Arc::new(super::chat::TurnHub::default()),
        parked_agents: super::chat::ParkedAgents::default(),
        session_background: super::chat::SessionBackgrounds::default(),
    });

    super::chat::resume_pending_follow_up_runs(state.clone())
        .await
        .context("failed to resume persisted pending session runs")?;

    // Spawn background tasks
    let _auto_archive = crate::tasks::spawn_auto_archive(db_for_archive.clone());
    if let Some(ref cm) = state.container_manager {
        let _auto_stop = cm.spawn_auto_stop_task();
    }
    super::chat::spawn_shared_tool_dispatchers(state.clone(), tool_queues);
    super::title_generation::spawn_worker(state.clone(), title_generation_rx);
    super::chat::spawn_background_watcher(state.clone(), state.ui_event_bus.clone());
    let scheduler = crate::scheduler::TaskScheduler::new(task_runtime);
    let _scheduler = scheduler.spawn();
    let router = build_router(state.clone(), mode);
    Ok(StartedDaemon { state, router })
}

fn api_routes() -> Route {
    use super::artifacts::{
        get_artifact_preview, get_artifact_runtime_bridge, get_artifact_runtime_preview,
        get_output_preview, get_session_runtime, get_slidev_static_asset, list_session_artifacts,
        map_container_local_url, prepare_slidev_preview, report_artifact_diagnostics,
        stream_session_artifacts,
    };
    use super::bootstrap::get_bootstrap;
    use super::browser::{
        browser_offer, browser_resize_viewport, browser_start, browser_status, browser_stop,
        browser_takeover_start, browser_takeover_stop,
    };
    use super::chat::{enqueue_chat, remove_pending_message, send_message, stop_session_run};
    use super::config::{
        add_mcp_server, add_ssh_server, copilot_logout, copilot_start, copilot_status,
        copilot_stream, delete_skill, get_memory, get_ui_config, install_skill, list_mcp_servers,
        list_skills, reconnect_mcp_server, remove_mcp_server, remove_ssh_server, save_full_config,
        save_memory, set_last_model,
    };
    use super::events::stream_events;
    use super::interaction::{
        respond_ask_user_request, respond_permission_request, respond_workspace_request,
    };
    use super::notifications::{
        consume_notification_callback, get_vapid_key, list_network_access_logs,
        list_notification_mailbox, send_notification, subscribe, test_notification, unsubscribe,
    };
    use super::profiles::{
        archive_session, create_folder, create_profile, delete_folder, delete_profile,
        get_profile_remote_servers, get_profile_sessions, list_profiles, move_to_folder,
        pin_session, set_profile_remote_servers, unarchive_session, unpin_session, update_folder,
        update_profile,
    };
    use super::remote_access::{get_remote_access_status, start_remote_access, stop_remote_access};
    use super::sessions::{
        append_session_history, create_session, delete_all_sessions, delete_session, get_session,
        get_session_context_debug, get_session_history, get_session_tasks, list_sessions,
        lookup_issues, rewind_session, save_session_tasks, set_session_plan_state, update_session,
    };
    use super::tasks::{
        approve_task, create_task, delete_task, get_task, invoke_webhook_task, list_tasks,
        run_task, update_task,
    };

    Route::new((
        Route::new((
            "/health".at(health),
            "/daemon/shutdown".post(shutdown_daemon),
            "/artifact-runtime.js".at(get_artifact_runtime_bridge),
            "/bootstrap".at(get_bootstrap),
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
                        "/history".at(get_session_history),
                        "/history".post(append_session_history),
                        "/rewind".post(rewind_session),
                        "/issues/lookup".post(lookup_issues),
                    )),
                    Route::new((
                        "/tasks".at(get_session_tasks),
                        "/tasks".put(save_session_tasks),
                        "/tasks/plan-state".put(set_session_plan_state),
                        "/stop".post(stop_session_run),
                        "/runtime".route(Route::new((
                            "".at(get_session_runtime),
                            "/pending".delete(remove_pending_message),
                        ))),
                        "/context-debug".at(get_session_context_debug),
                        "/artifacts".at(list_session_artifacts),
                        "/artifacts/preview".at(get_artifact_preview),
                        "/artifacts/slidev/preview".at(prepare_slidev_preview),
                        "/artifacts/slidev-static/{buildKey}/{*path}".at(get_slidev_static_asset),
                        "/outputs/preview".at(get_output_preview),
                        "/artifacts/local-url-map".post(map_container_local_url),
                    )),
                    Route::new(("/computer".route(Route::new((
                        "/status".at(super::computer::computer_status),
                        "/start".post(super::computer::computer_start),
                        "/stop".post(super::computer::computer_stop),
                        "/offer".post(super::computer::computer_offer),
                        "/takeover/start".post(super::computer::computer_takeover_start),
                        "/takeover/stop".post(super::computer::computer_takeover_stop),
                    ))),)),
                    Route::new((
                        "/messages".post(send_message),
                        "/compact".post(super::chat::compact_session),
                        "/context-report".at(super::chat::context_report),
                        "/context-dump".post(super::chat::context_dump),
                        "/btw".post(super::chat::btw),
                        "/agent".delete(super::chat::release_agent),
                    )),
                    Route::new((
                        "/background".at(super::chat::get_background),
                        "/jobs/read".at(super::chat::read_job),
                        "/jobs/{pid}".at(super::chat::get_job),
                        "/jobs/{pid}/kill".post(super::chat::kill_job),
                        "/subagents".delete(super::chat::cancel_subagents),
                        "/subagents/{runId}".delete(super::chat::cancel_subagent),
                    )),
                    Route::new((
                        "/artifacts/runtime/preview".at(get_artifact_runtime_preview),
                        "/artifacts/stream".at(stream_session_artifacts),
                        "/artifacts/diagnostics".post(report_artifact_diagnostics),
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
                "/{id}/remotes".at(get_profile_remote_servers),
                "/{id}/remotes".put(set_profile_remote_servers),
            ))),
        )),
        Route::new((
            "/folders".route(Route::new((
                "/{id}".put(update_folder),
                "/{id}".delete(delete_folder),
            ))),
            "/chat".route(Route::new(("/enqueue".post(enqueue_chat),))),
            "/permissions".route(Route::new(("/respond".post(respond_permission_request),))),
            "/workspace".route(Route::new(("/respond".post(respond_workspace_request),))),
            "/ask-user".route(Route::new(("/respond".post(respond_ask_user_request),))),
            "/config".route(Route::new((
                "".at(get_ui_config),
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
                "/webhook/{token}".post(invoke_webhook_task),
                "/{id}".at(get_task),
                "/{id}".put(update_task),
                "/{id}".delete(delete_task),
                "/{id}/approve".post(approve_task),
                "/{id}/run".post(run_task),
            ))),
            "/copilot".route(Route::new((
                "/status".at(copilot_status),
                "/start".post(copilot_start),
                "/stream".at(copilot_stream),
                "/logout".post(copilot_logout),
            ))),
            "/browser".route(Route::new((
                "/status".at(browser_status),
                "/start".post(browser_start),
                "/stop".post(browser_stop),
                "/offer".post(browser_offer),
                "/viewport".post(browser_resize_viewport),
                "/takeover/start".post(browser_takeover_start),
                "/takeover/stop".post(browser_takeover_stop),
            ))),
            "/remote-access".route(Route::new((
                "/status".at(get_remote_access_status),
                "/start".post(start_remote_access),
                "/stop".post(stop_remote_access),
            ))),
            "/push".route(Route::new((
                "/vapid-key".at(get_vapid_key),
                "/subscribe".post(subscribe),
                "/unsubscribe".post(unsubscribe),
                "/test".post(test_notification),
            ))),
            "/notifications".route(Route::new((
                "".post(send_notification),
                "/mailbox".at(list_notification_mailbox),
                "/{id}/callback".post(consume_notification_callback),
                "/network-access".at(list_network_access_logs),
            ))),
        )),
    ))
}

fn build_router(state: Arc<DaemonState>, mode: DaemonMode) -> Router {
    let api = "/api".route(api_routes());

    match mode {
        DaemonMode::Rest => Route::new((api,)).middleware(State(state)).build(),
        DaemonMode::Ui => {
            #[cfg(feature = "ui")]
            {
                Route::new((api, EmbeddedStaticDir::new("/", &WEB_UI_DIST).spa()))
                    .middleware(State(state))
                    .build()
            }

            #[cfg(not(feature = "ui"))]
            {
                panic!(
                    "`may ui` should fail before router construction when the `ui` feature is disabled"
                );
            }
        }
    }
}

// ── System / Meta handlers ────────────────────────────────

async fn health(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<super::types::DaemonHealth>> {
    Ok(Json(super::types::DaemonHealth {
        ok: true,
        pid: std::process::id(),
        build: state.build_id.clone(),
        search_index: state.search_index_status.snapshot(),
    }))
}

async fn shutdown_daemon(State(state): State<Arc<DaemonState>>) -> SkyResult<Json<ApiHealth>> {
    state.daemon_shutdown.notify_one();
    Ok(Json(super::types::API_OK))
}

async fn get_request_logs(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<RequestLogsResponse>> {
    let logs = state
        .db()
        .list_request_logs(300)
        .await
        .map_err(internal_error)?
        .into_iter()
        .map(RequestLogEntry::from)
        .collect::<Vec<_>>();
    let cache_summary = state
        .db()
        .assistant_cache_usage_summary()
        .await
        .map(RequestLogCacheSummary::from)
        .map_err(internal_error)?;
    Ok(Json(RequestLogsResponse {
        logs,
        cache_summary,
    }))
}

async fn models(State(state): State<Arc<DaemonState>>) -> SkyResult<Json<ModelsResponse>> {
    let tokio_handle = state.tokio_handle.clone();
    let result = tokio_handle
        .spawn(async move {
            let config = (*state.config_snapshot()).clone();
            let refresh_config = config.clone();
            let fetched = state
                .fetched_models_cache
                .get_or_refresh(move || async move { fetch_all_models(&refresh_config).await })
                .await;
            build_models_response(&config, fetched, true)
        })
        .await;
    let result = result.map_err(internal_error)?;
    Ok(Json(result))
}

fn build_models_response(
    config: &MayConfig,
    fetched: Vec<crate::models::FetchedModel>,
    include_static_metadata: bool,
) -> ModelsResponse {
    let mut model_map = std::collections::HashMap::<String, ModelInfoItem>::new();

    for item in fetched {
        let selector = crate::ModelSelector::new(&item.provider_id, &item.id).to_string();
        let new_entry = ModelInfoItem {
            id: selector.clone(),
            model_id: item.id.clone(),
            provider_id: item.provider_id.clone(),
            nickname: item.nickname,
            context_window: item.context_window,
            abilities: item.abilities,
        };
        model_map
            .entry(selector)
            .and_modify(|existing| {
                // Prefer the entry with more abilities (richer metadata).
                if new_entry.abilities.len() > existing.abilities.len() {
                    *existing = new_entry.clone();
                }
            })
            .or_insert(new_entry);
    }

    for tier in crate::config::ModelTier::ALL {
        for selector in config.models.models_for_tier(*tier) {
            let selector_key = selector.to_string();
            model_map.entry(selector_key.clone()).or_insert_with(|| {
                let static_info = if include_static_metadata {
                    lookup_model_info(selector.model_id())
                } else {
                    None
                };
                let (nickname, context_window, abilities) = static_info.map_or_else(
                    || (selector.model_id().to_string(), None, Vec::new()),
                    |info| {
                        (
                            info.id().to_string(),
                            info.max_input_tokens(),
                            info.abilities()
                                .iter()
                                .map(|a| crate::models::ability_to_label(*a))
                                .collect::<Vec<_>>(),
                        )
                    },
                );
                ModelInfoItem {
                    id: selector_key,
                    model_id: selector.model_id().to_string(),
                    provider_id: selector.provider_id().to_string(),
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
            if let Some(info) = lookup_model_info(&item.model_id) {
                item.nickname = info.id().to_string();
            }
        }
    }

    let mut details = model_map.into_values().collect::<Vec<_>>();
    details.sort_by(|a, b| {
        a.provider_id
            .cmp(&b.provider_id)
            .then(a.model_id.cmp(&b.model_id))
    });
    let models_list = details.iter().map(|d| d.id.clone()).collect::<Vec<_>>();
    ModelsResponse {
        models: models_list,
        details,
    }
}

async fn models_by_provider(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ProviderModelsResponse>> {
    let tokio_handle = state.tokio_handle.clone();
    let result = tokio_handle
        .spawn(async move {
            let config = (*state.config_snapshot()).clone();
            let refresh_config = config.clone();
            let fetched = state
                .fetched_models_cache
                .get_or_refresh(move || async move { fetch_all_models(&refresh_config).await })
                .await;
            build_provider_models_response(fetched)
        })
        .await;
    let result = result.map_err(internal_error)?;
    Ok(Json(result))
}

fn build_provider_models_response(
    fetched: Vec<crate::models::FetchedModel>,
) -> ProviderModelsResponse {
    let mut grouped: std::collections::HashMap<String, Vec<ModelInfoItem>> =
        std::collections::HashMap::new();

    for item in fetched {
        let selector = crate::ModelSelector::new(&item.provider_id, &item.id).to_string();
        grouped
            .entry(item.provider_id.clone())
            .or_default()
            .push(ModelInfoItem {
                id: selector,
                model_id: item.id,
                provider_id: item.provider_id,
                nickname: item.nickname,
                context_window: item.context_window,
                abilities: item.abilities,
            });
    }

    let mut providers = grouped
        .into_iter()
        .map(|(provider_id, mut details)| {
            details.sort_by(|a, b| a.model_id.cmp(&b.model_id));
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

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "ui"))]
    use super::DaemonMode;

    #[cfg(not(feature = "ui"))]
    #[test]
    fn rest_mode_is_always_supported() {
        DaemonMode::Rest
            .ensure_supported()
            .expect("rest mode should always be available");
    }

    #[cfg(not(feature = "ui"))]
    #[test]
    fn ui_mode_requires_ui_feature() {
        let error = DaemonMode::Ui
            .ensure_supported()
            .expect_err("ui mode should fail without the ui feature");
        assert_eq!(
            error.to_string(),
            "`may ui` requires the `ui` cargo feature; use `may rest` for the REST API server"
        );
    }
}
