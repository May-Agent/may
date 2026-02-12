//! May TUI - Terminal interface for May agent.
//!
//! Architecture:
//! - Main thread: Handles UI rendering and terminal events only
//! - Background tasks: Run agent processing, send events via channels
//! - Communication: All through async_channel (runtime-agnostic)

use std::io;
use std::panic;

use aither_agent::AgentEvent;
use anyhow::Context;

use async_channel::TryRecvError;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyCode, KeyModifiers,
    },
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use executor_core::tokio::TokioGlobal;
use executor_core::{spawn, try_init_global_executor};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::backend::CrosstermBackend;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use may_agent::agent::{AgentHandle, AgentWorker, TuiEvent};
use may_agent::ask_user::{self, AskUserQueue};
use may_agent::mcp::McpConfig;
use may_agent::tui::runtime::{RuntimeDeps, TitleGenContext};
use may_agent::tui::{App, AppEvent, EventHandler, SendAction, ui};
use may_agent::workspace_request::{self, WorkspaceRequestQueue};
use may_agent::{MayMcpServerConfig, McpManager, SkillManager};

const PANIC_MESSAGE: &str = include_str!("../texts/panic_message.txt");

fn ensure_rustls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = try_init_global_executor(TokioGlobal);
    ensure_rustls_provider();

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "ui") {
        return may_agent::ui_server::run_ui_server(&args[2..]).await;
    }

    // Initialize logging to file
    let log_dir = dirs::home_dir()
        .context("Failed to determine home directory")?
        .join(".may")
        .join("logs");
    let file_appender = tracing_appender::rolling::daily(log_dir, "may.log");
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(file_appender).with_ansi(false))
        .with(
            EnvFilter::from_default_env()
                .add_directive("may=info".parse()?)
                .add_directive("may_agent=debug".parse()?)
                .add_directive("aither_agent=debug".parse()?)
                .add_directive("aither_sandbox=info".parse()?)
                .add_directive("aither_cloud=debug".parse()?)
                .add_directive("aither_gemini=info".parse()?)
                .add_directive("aither_claude=info".parse()?)
                .add_directive("aither_openai=info".parse()?),
        )
        .init();

    // Set up panic hook to restore terminal state before panicking
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        // Best-effort terminal cleanup
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableBracketedPaste,
            DisableMouseCapture
        );
        let _ = execute!(
            io::stderr(),
            SetForegroundColor(Color::Red),
            Print(PANIC_MESSAGE),
            ResetColor
        );
        default_hook(panic_info);
    }));

    // Detect terminal image support (must be before raw mode)
    let image_support = may_agent::tui::ui::image::TerminalImageSupport::detect();

    // Setup terminal (alternate screen)
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app and run
    let result = run_app(&mut terminal, image_support).await;

    // Cleanup terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

async fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    image_support: Option<may_agent::tui::ui::image::TerminalImageSupport>,
) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let mut events = EventHandler::new();
    let mut app = App::new(events.sender(), image_support).await?;
    let mut worker: Option<AgentWorker> = None;

    // Create workspace request channel (runtime-agnostic)
    let (workspace_broker, workspace_queue) = workspace_request::channel();

    // Create ask user channel (runtime-agnostic)
    let (ask_user_broker, ask_user_queue) = ask_user::channel();
    tracing::info!("=== MAY STARTUP - channels created ===");

    // Load MCP configuration and connect to servers
    let mut mcp_manager = McpManager::load().unwrap_or_else(|e| {
        tracing::warn!("Failed to load MCP config: {}", e);
        McpManager::new(McpConfig::default())
    });

    // Connect to all configured MCP servers
    if mcp_manager.has_servers() {
        let results = mcp_manager.connect_all().await;
        for (name, result) in results {
            match result {
                Ok(()) => tracing::info!("Connected to MCP server: {}", name),
                Err(e) => tracing::warn!("Failed to connect to MCP server {}: {}", name, e),
            }
        }
    }

    // Update app's MCP server list
    app.mcp_servers = mcp_manager.server_info();

    let runtime_deps = RuntimeDeps::new(workspace_broker.clone(), ask_user_broker.clone());

    // Only start a session if configuration is complete
    if app.is_configured()
        && let Err(e) = runtime_deps.start_new_session(&mut app, &mut worker).await {
            tracing::warn!("Initial session start failed: {}", e);
            app.mode = may_agent::tui::AppMode::Chat;
            app.status = Some(format!(
                "Session start failed: {} (open /config to fix provider/model)",
                e
            ));
        }

    // Main event loop - only receives from channels, never blocks on agent processing
    main_event_loop(
        &mut app,
        &mut worker,
        &mut events,
        terminal,
        runtime_deps,
        workspace_queue,
        ask_user_queue,
        &mut mcp_manager,
    )
    .await
}

/// The main event loop - handles terminal events and channel messages.
///
/// This loop NEVER directly calls agent.run() or stream.next() - all agent processing
/// happens in background tasks that communicate via channels.
#[allow(clippy::too_many_arguments)]
async fn main_event_loop<B: Backend>(
    app: &mut App,
    worker: &mut Option<AgentWorker>,
    events: &mut EventHandler,
    terminal: &mut Terminal<B>,
    runtime_deps: RuntimeDeps,
    workspace_queue: WorkspaceRequestQueue,
    ask_user_queue: AskUserQueue,
    mcp_manager: &mut McpManager,
) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    // Handle to a running agent task (if any)
    let mut agent_handle: Option<AgentHandle> = None;

    // For title generation
    let mut pending_title_gen: Option<TitleGenContext> = None;
    let mut assistant_response = String::new();

    loop {
        tracing::info!("=== LOOP START ===");

        // First draw: updates content_height/view_height for accurate overflow calculation.
        terminal.draw(|frame| ui::render(frame, app))?;
        tracing::info!("after draw");

        // Check for model fetch results (non-blocking)
        if let Some(ref mut rx) = app.model_fetch_rx
            && let Ok(models) = rx.try_recv() {
                app.handle_fetched_models(models);
                app.model_fetch_rx = None;
            }

        // Check for clipboard paste results (non-blocking)
        if let Some(ref mut rx) = app.clipboard_rx {
            match rx.try_recv() {
                Ok(result) => {
                    app.clipboard_rx = None;
                    app.pending_attachment_jobs = app.pending_attachment_jobs.saturating_sub(1);
                    app.enqueue_paste_result(result);
                }
                Err(TryRecvError::Closed) => {
                    app.clipboard_rx = None;
                    app.pending_attachment_jobs = app.pending_attachment_jobs.saturating_sub(1);
                    return Err(anyhow::anyhow!(
                        "Clipboard task closed before sending a result"
                    ));
                }
                Err(TryRecvError::Empty) => {}
            }
        }

        while let Some(result) = app.dequeue_paste_result() {
            app.handle_paste_result(result).await?;
        }

        // Check for queued messages - start processing if agent is idle
        if agent_handle.is_none()
            && let Some(msg) = app.next_queued()
                && let Some(w) = worker.as_ref() {
                    agent_handle =
                        Some(runtime_deps.start_agent_task(app, w, msg, &mut pending_title_gen)?);
                    assistant_response.clear();
                    continue;
                }

        // DIAGNOSTIC: Check channel states before entering select
        tracing::info!(
            ask_user_empty = ask_user_queue.is_empty(),
            workspace_empty = workspace_queue.is_empty(),
            has_agent_handle = agent_handle.is_some(),
            "channel states before select"
        );

        enum Event {
            AskUser(may_agent::ask_user::AskUserRequest),
            Workspace(may_agent::workspace_request::WorkspaceRequest),
            Agent(TuiEvent),
            Terminal(AppEvent),
        }

        tracing::info!("entering select");

        // Use futures_lite::future::or chain
        let event = futures_lite::future::or(
            async {
                let result = ask_user_queue.next().await;
                tracing::info!("ask_user_queue.next() RETURNED: {:?}", result.is_some());
                result.map(Event::AskUser)
            },
            futures_lite::future::or(
                async {
                    let result = workspace_queue.next().await;
                    tracing::info!("workspace_queue.next() RETURNED");
                    result.map(Event::Workspace)
                },
                futures_lite::future::or(
                    async {
                        if let Some(ref handle) = agent_handle {
                            let result = handle.recv().await;
                            tracing::info!("agent handle.recv() RETURNED");
                            result.map(Event::Agent)
                        } else {
                            futures_lite::future::pending().await
                        }
                    },
                    async {
                        let result = events.next().await;
                        // Don't log ticks, too noisy
                        if !matches!(result, Some(AppEvent::Tick)) {
                            tracing::info!(
                                "events.next() RETURNED: {:?}",
                                result.as_ref().map(|_| "event")
                            );
                        }
                        result.map(Event::Terminal)
                    },
                ),
            ),
        )
        .await;

        tracing::info!(
            "select completed, event type: {}",
            match &event {
                Some(Event::AskUser(_)) => "AskUser",
                Some(Event::Workspace(_)) => "Workspace",
                Some(Event::Agent(_)) => "Agent",
                Some(Event::Terminal(_)) => "Terminal",
                None => "None",
            }
        );

        let Some(event) = event else {
            tracing::warn!("All channels closed, exiting");
            break;
        };

        match event {
            Event::AskUser(request) => {
                let questions = request.args.clone().into_questions();
                tracing::info!(question_count = questions.len(), "User question requested");
                let state = may_agent::tui::AskUserState::new(request, questions);
                app.mode = may_agent::tui::AppMode::AskUser(state);
            }

            Event::Workspace(request) => {
                tracing::info!(
                    path = %request.args.path,
                    reason = %request.args.reason,
                    "Workspace access requested"
                );
                app.mode = may_agent::tui::AppMode::WorkspaceConfirm(request);
            }

            Event::Agent(tui_event) => {
                match tui_event {
                    TuiEvent::Agent(result) => {
                        // Capture text for title generation
                        if let Ok(AgentEvent::Text(ref text)) = result {
                            assistant_response.push_str(text);
                        }
                        let ended = app.handle_agent_event(result);
                        if ended {
                            runtime_deps
                                .finish_agent_task(
                                    app,
                                    &mut agent_handle,
                                    &mut pending_title_gen,
                                    &assistant_response,
                                )
                                .await;
                        }
                    }
                    TuiEvent::Compacted(result) => match result {
                        Ok(Some(stats)) => {
                            app.status =
                                Some(format!("Compacted {} messages", stats.messages_compacted));
                        }
                        Ok(None) => {
                            app.status = Some("Nothing to compact".to_string());
                        }
                        Err(err) => {
                            app.chat.push_error(&format!("Compaction failed: {err}"));
                        }
                    },
                    TuiEvent::Complete => {
                        runtime_deps
                            .finish_agent_task(
                                app,
                                &mut agent_handle,
                                &mut pending_title_gen,
                                &assistant_response,
                            )
                            .await;
                    }
                    TuiEvent::Error(err) => {
                        app.chat.push_error(&err);
                        runtime_deps
                            .finish_agent_task(
                                app,
                                &mut agent_handle,
                                &mut pending_title_gen,
                                &assistant_response,
                            )
                            .await;
                    }
                }
            }

            Event::Terminal(terminal_event) => {
                // Check for interrupt during agent processing
                if agent_handle.is_some()
                    && let AppEvent::Key(key_code, modifiers) = &terminal_event {
                        let is_ctrl_c = *key_code == KeyCode::Char('c')
                            && modifiers.contains(KeyModifiers::CONTROL);
                        let is_esc = *key_code == KeyCode::Esc;
                        if is_ctrl_c || is_esc {
                            tracing::info!("Agent interrupted by user");
                            if let Some(handle) = agent_handle.take() {
                                handle.cancel();
                            }
                            // Clean up loading/streaming state before adding interrupt message
                            app.chat.end_loading();
                            app.chat.end_text();
                            app.chat.push_text("\n\n[Interrupted]");
                            app.chat.end_text();
                            app.finish_processing();
                            continue;
                        }
                    }

                // Handle normal terminal events
                match app.handle_event(terminal_event)? {
                    Some(SendAction::Quit) => break,
                    Some(SendAction::NewSession) => {
                        runtime_deps.start_new_session(app, worker).await?;
                    }
                    Some(SendAction::LoadSession(id)) => {
                        runtime_deps.load_session(app, worker, &id).await?;
                    }
                    Some(SendAction::Send(msg)) => {
                        if agent_handle.is_none() {
                            if let Some(w) = worker.as_ref() {
                                agent_handle = Some(runtime_deps.start_agent_task(
                                    app,
                                    w,
                                    msg,
                                    &mut pending_title_gen,
                                )?);
                                assistant_response.clear();
                            }
                        } else {
                            // Agent is busy, queue the message
                            app.queue.enqueue(msg);
                        }
                    }
                    Some(SendAction::StartCopilotAuth) => {
                        run_copilot_auth(app, events, terminal).await?;
                    }
                    Some(SendAction::SwitchModel(model)) => {
                        if let Err(e) = runtime_deps.switch_model(app, worker, &model).await {
                            let err_msg = format!("Failed to switch model: {}", e);
                            app.chat.push_error(&err_msg);
                        } else {
                            tracing::info!("Switched model to: {}", model);
                            app.status = Some(format!("Model: {}", model));
                            if let Err(e) =
                                app.session_manager.set_current_model(model.clone()).await
                            {
                                tracing::warn!("Failed to save model to session: {}", e);
                            }
                            app.config.last_model = Some(model);
                            if let Err(e) = app.config.save() {
                                tracing::warn!("Failed to save config: {}", e);
                            }
                        }
                    }
                    Some(SendAction::OpenCopilotAuthUrl { url, code }) => {
                        if let Ok(mut clipboard) = arboard::Clipboard::new()
                            && let Err(e) = clipboard.set_text(&code) {
                                tracing::warn!("Failed to copy code to clipboard: {}", e);
                            }
                        if let Err(e) = open::that(&url) {
                            tracing::warn!("Failed to open URL in browser: {}", e);
                        }
                    }
                    Some(SendAction::Compact(focus)) => {
                        if agent_handle.is_none() {
                            if let Some(w) = worker.as_ref() {
                                app.processing = true;
                                app.status = Some("Compacting conversation...".to_string());
                                agent_handle = Some(w.compact(focus));
                            }
                        } else {
                            app.status = Some("Agent is busy".to_string());
                        }
                    }
                    Some(SendAction::McpAdd {
                        name,
                        command,
                        args,
                    }) => {
                        let config = MayMcpServerConfig {
                            command: Some(command),
                            args,
                            url: None,
                            env: std::collections::HashMap::new(),
                        };
                        match mcp_manager.add_server(&name, config).await {
                            Ok(()) => {
                                app.status = Some(format!("Added MCP server: {}", name));
                                app.mcp_servers = mcp_manager.server_info();
                                tracing::info!("Added MCP server: {}", name);
                            }
                            Err(e) => {
                                let err_msg = format!("Failed to add MCP server: {}", e);
                                app.chat.push_error(&err_msg);
                                tracing::error!("{}", err_msg);
                            }
                        }
                    }
                    Some(SendAction::McpRemove { name }) => {
                        match mcp_manager.remove_server(&name).await {
                            Ok(()) => {
                                app.status = Some(format!("Removed MCP server: {}", name));
                                app.mcp_servers = mcp_manager.server_info();
                                if app.mcp_selected >= app.mcp_servers.len()
                                    && !app.mcp_servers.is_empty()
                                {
                                    app.mcp_selected = app.mcp_servers.len() - 1;
                                }
                                tracing::info!("Removed MCP server: {}", name);
                            }
                            Err(e) => {
                                let err_msg = format!("Failed to remove MCP server: {}", e);
                                app.chat.push_error(&err_msg);
                                tracing::error!("{}", err_msg);
                            }
                        }
                    }
                    Some(SendAction::McpReconnect { name }) => {
                        match mcp_manager.reconnect(&name).await {
                            Ok(()) => {
                                app.status = Some(format!("Reconnected to MCP server: {}", name));
                                app.mcp_servers = mcp_manager.server_info();
                                tracing::info!("Reconnected to MCP server: {}", name);
                            }
                            Err(e) => {
                                let err_msg = format!("Failed to reconnect to MCP server: {}", e);
                                app.chat.push_error(&err_msg);
                                app.mcp_servers = mcp_manager.server_info();
                                tracing::error!("{}", err_msg);
                            }
                        }
                    }
                    Some(SendAction::SshAdd { name, target }) => {
                        app.config
                            .upsert_ssh_server(may_agent::config::SshServerConfig {
                                name: name.clone(),
                                target,
                            });
                        if let Err(e) = app.config.save() {
                            let err_msg = format!("Failed to save SSH server: {}", e);
                            app.chat.push_error(&err_msg);
                        } else {
                            app.status = Some(format!("Added SSH server: {}", name));
                            if app.ssh_selected + 1 < app.config.ssh_servers.len() {
                                app.ssh_selected = app.config.ssh_servers.len() - 1;
                            }
                        }
                    }
                    Some(SendAction::SshRemove { name }) => {
                        if app.config.remove_ssh_server(&name) {
                            if let Err(e) = app.config.save() {
                                let err_msg = format!("Failed to save SSH config: {}", e);
                                app.chat.push_error(&err_msg);
                            } else {
                                app.status = Some(format!("Removed SSH server: {}", name));
                                if app.ssh_selected >= app.config.ssh_servers.len()
                                    && !app.config.ssh_servers.is_empty()
                                {
                                    app.ssh_selected = app.config.ssh_servers.len() - 1;
                                }
                            }
                        }
                    }
                    Some(SendAction::SkillInstall { repo }) => {
                        let tx = events.sender();
                        let may_dir = app.session_manager.may_dir().to_path_buf();
                        app.status = Some(format!("Installing from {}...", repo));
                        spawn(async move {
                            let manager = SkillManager::new(&may_dir);
                            let result = match SkillManager::parse_github_repo(&repo) {
                                Ok((owner, repo_name)) => {
                                    manager.install_from_github(owner, repo_name).await
                                }
                                Err(e) => Err(e),
                            };
                            // Refresh skill list after install
                            let final_result = match result {
                                Ok(_) => manager.list_skills().await,
                                Err(e) => Err(e),
                            };
                            let _ = tx
                                .send(AppEvent::SkillInstalled(
                                    final_result.map_err(anyhow::Error::from),
                                ))
                                .await;
                        })
                        .detach();
                    }
                    Some(SendAction::SkillDelete { dir_name }) => {
                        let tx = events.sender();
                        let may_dir = app.session_manager.may_dir().to_path_buf();
                        app.status = Some(format!("Deleting {}...", dir_name));
                        spawn(async move {
                            let manager = SkillManager::new(&may_dir);
                            let result = manager.delete_skill(&dir_name).await;
                            // Refresh skill list after delete
                            let final_result = match result {
                                Ok(()) => manager.list_skills().await,
                                Err(e) => Err(e),
                            };
                            let _ = tx
                                .send(AppEvent::SkillDeleted(
                                    final_result.map_err(anyhow::Error::from),
                                ))
                                .await;
                        })
                        .detach();
                    }
                    Some(SendAction::Queued) | None => {}
                }
            }
        }
    }

    Ok(())
}

/// Run the GitHub Copilot OAuth device flow.
async fn run_copilot_auth<B: Backend>(
    app: &mut App,
    events: &mut EventHandler,
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    use aither_copilot::auth;
    use may_agent::copilot::save_copilot_token;

    app.start_copilot_auth();
    terminal.draw(|frame| ui::render(frame, app))?;

    let device_code = match auth::request_device_code().await {
        Ok(dc) => dc,
        Err(e) => {
            app.copilot_auth_failed(e.to_string());
            wait_for_auth_dismiss(app, events, terminal).await?;
            return Ok(());
        }
    };

    app.set_copilot_device_code(
        device_code.user_code.clone(),
        device_code.verification_uri.clone(),
    );
    terminal.draw(|frame| ui::render(frame, app))?;

    let poll_interval = std::time::Duration::from_secs(device_code.interval.max(5));

    loop {
        let deadline = tokio::time::Instant::now() + poll_interval;

        loop {
            let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
            if timeout.is_zero() {
                break;
            }

            let event_timeout = timeout.min(std::time::Duration::from_millis(100));
            match tokio::time::timeout(event_timeout, events.next()).await {
                Ok(Some(event)) => {
                    if let Some(action) = app.handle_event(event)? {
                        match action {
                            SendAction::Quit => return Ok(()),
                            SendAction::OpenCopilotAuthUrl { url, code } => {
                                if let Ok(mut clipboard) = arboard::Clipboard::new()
                                    && let Err(e) = clipboard.set_text(&code) {
                                        tracing::warn!("Failed to copy code to clipboard: {}", e);
                                    }
                                if let Err(e) = open::that(&url) {
                                    tracing::warn!("Failed to open URL in browser: {}", e);
                                }
                            }
                            _ => {}
                        }
                    }
                    if !matches!(app.mode, may_agent::tui::AppMode::CopilotAuth) {
                        return Ok(());
                    }
                    terminal.draw(|frame| ui::render(frame, app))?;
                }
                Ok(None) => return Ok(()),
                Err(_) => {}
            }
        }

        match auth::try_get_token(&device_code.device_code).await {
            Ok(token) => {
                if let Err(e) = save_copilot_token(&token).await {
                    app.copilot_auth_failed(format!("Failed to save token: {}", e));
                    wait_for_auth_dismiss(app, events, terminal).await?;
                    return Ok(());
                }
                let instance =
                    may_agent::config::ProviderInstance::new(may_agent::config::ProviderType::Copilot);
                app.config.upsert_provider("copilot", instance);
                app.config_state.load_providers(&app.config);
                if let Err(e) = app.config.save() {
                    tracing::error!("Failed to save config: {}", e);
                }
                app.copilot_auth_success();
                wait_for_auth_dismiss(app, events, terminal).await?;
                return Ok(());
            }
            Err(aither_copilot::CopilotError::AuthorizationPending) => {
                terminal.draw(|frame| ui::render(frame, app))?;
                continue;
            }
            Err(e) => {
                app.copilot_auth_failed(e.to_string());
                wait_for_auth_dismiss(app, events, terminal).await?;
                return Ok(());
            }
        }
    }
}

/// Wait for the user to dismiss the auth result screen.
async fn wait_for_auth_dismiss<B: Backend>(
    app: &mut App,
    events: &mut EventHandler,
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    terminal.draw(|frame| ui::render(frame, app))?;

    loop {
        if let Some(event) = events.next().await {
            app.handle_event(event)?;
            if !matches!(app.mode, may_agent::tui::AppMode::CopilotAuth) {
                break;
            }
        }
        terminal.draw(|frame| ui::render(frame, app))?;
    }

    Ok(())
}
