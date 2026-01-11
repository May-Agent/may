//! May TUI - Terminal interface for May agent.

use std::io;
use std::panic;
use std::pin::pin;

use aither_agent::{AgentError, AgentEvent};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyCode, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_lite::StreamExt;
use ratatui::prelude::*;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use may_agent::tui::{App, AppEvent, Attachment, EventHandler, PendingMessage, SendAction, ui};
use may_agent::tui::queue::{LARGE_PASTE_THRESHOLD, PASTE_TRUNCATE_LIMIT};
use may_agent::workspace_request::{self, WorkspaceRequest, WorkspaceRequestReceiver, WorkspaceRequestSender};
use may_agent::{generate_session_title, MayAgent, McpManager, MayMcpServerConfig, SessionId};
use may_agent::mcp::McpConfig;
use serde::Serialize;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging to file
    let file_appender = tracing_appender::rolling::daily(
        dirs::home_dir().unwrap_or_default().join(".may").join("logs"),
        "may.log",
    );
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(file_appender).with_ansi(false))
        .with(
            EnvFilter::from_default_env()
                .add_directive("may_agent=debug".parse()?)
                .add_directive("aither_agent=debug".parse()?)
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
        // Print a blank line to separate from any partial output
        eprintln!();
        // Call the default panic handler
        default_hook(panic_info);
    }));

    // Setup terminal
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
    let result = run_app(&mut terminal).await;

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

/// Event source for the main loop.
enum Event {
    Terminal(AppEvent),
    Agent(Result<AgentEvent, AgentError>),
    WorkspaceRequest(WorkspaceRequest),
    AgentDone,
}

async fn run_app<B: Backend>(terminal: &mut Terminal<B>) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let mut app = App::new()?;
    let mut events = EventHandler::new();
    let mut agent: Option<MayAgent> = None;

    // Create workspace request channel
    let (workspace_tx, mut workspace_rx) = workspace_request::channel();

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

    // Only start a session if configuration is complete
    if app.is_configured() {
        start_new_session(&mut app, &mut agent, workspace_tx.clone()).await?;
    }

    loop {
        // Draw UI
        terminal.draw(|frame| ui::render(frame, &app))?;

        // Check for model fetch results (non-blocking)
        if let Some(ref mut rx) = app.model_fetch_rx {
            if let Ok(models) = rx.try_recv() {
                app.handle_fetched_models(models);
                app.model_fetch_rx = None;
            }
        }

        // Check for queued messages first
        if let Some(msg) = app.next_queued() {
            if let Some(ref mut ag) = agent {
                process_message(&mut app, ag, msg, &mut events, &mut workspace_rx, terminal).await?;
                continue;
            }
        }

        // Wait for terminal events (we handle streaming separately)
        let Some(terminal_event) = events.next().await else {
            break;
        };

        // Handle the event
        match app.handle_event(terminal_event)? {
            Some(SendAction::Quit) => break,
            Some(SendAction::NewSession) => {
                start_new_session(&mut app, &mut agent, workspace_tx.clone()).await?;
            }
            Some(SendAction::LoadSession(id)) => {
                load_session(&mut app, &mut agent, &id, workspace_tx.clone()).await?;
            }
            Some(SendAction::Send(msg)) => {
                if let Some(ref mut ag) = agent {
                    process_message(&mut app, ag, msg, &mut events, &mut workspace_rx, terminal).await?;
                    // Continue to immediately check for queued messages
                    continue;
                }
            }
            Some(SendAction::StartCopilotAuth) => {
                run_copilot_auth(&mut app, &mut events, terminal).await?;
            }
            Some(SendAction::SwitchModel(model)) => {
                if let Err(e) = switch_model(&mut app, &mut agent, workspace_tx.clone(), &model).await {
                    let err_msg = format!("Failed to switch model: {}", e);
                    app.chat.push_error(&err_msg);
                } else {
                    tracing::info!("Switched model to: {}", model);
                    app.status = Some(format!("Model: {}", model));
                    // Save model to session metadata
                    if let Err(e) = app.session_manager.set_current_model(model.clone()) {
                        tracing::warn!("Failed to save model to session: {}", e);
                    }
                    // Save as last_model for future sessions
                    app.config.last_model = Some(model);
                    if let Err(e) = app.config.save() {
                        tracing::warn!("Failed to save config: {}", e);
                    }
                }
            }
            Some(SendAction::OpenCopilotAuthUrl { url, code }) => {
                // Copy code to clipboard
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    if let Err(e) = clipboard.set_text(&code) {
                        tracing::warn!("Failed to copy code to clipboard: {}", e);
                    }
                }
                // Open URL in browser
                if let Err(e) = open::that(&url) {
                    tracing::warn!("Failed to open URL in browser: {}", e);
                }
            }
            Some(SendAction::Compact) => {
                // Compact conversation history
                if let Some(ref mut ag) = agent {
                    match ag.inner_mut().compact().await {
                        Ok(Some(result)) => {
                            let msg = format!(
                                "Compacted: {} messages -> {} remaining",
                                result.messages_compacted,
                                result.messages_remaining
                            );
                            app.status = Some(msg.clone());
                            tracing::info!("{}", msg);
                        }
                        Ok(None) => {
                            app.status = Some("No compaction needed".to_string());
                        }
                        Err(e) => {
                            let err_msg = format!("Compaction failed: {}", e);
                            app.chat.push_error(&err_msg);
                            tracing::error!("{}", err_msg);
                        }
                    }
                } else {
                    app.status = Some("No active agent to compact".to_string());
                }
            }
            Some(SendAction::McpAdd { name, command, args }) => {
                // Add a new MCP server
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
                // Remove an MCP server
                match mcp_manager.remove_server(&name).await {
                    Ok(()) => {
                        app.status = Some(format!("Removed MCP server: {}", name));
                        app.mcp_servers = mcp_manager.server_info();
                        // Reset selection if needed
                        if app.mcp_selected >= app.mcp_servers.len() && !app.mcp_servers.is_empty() {
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
                // Reconnect to an MCP server
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
            Some(SendAction::Queued) | None => {}
        }
    }

    Ok(())
}

/// Start a new session.
async fn start_new_session(
    app: &mut App,
    agent: &mut Option<MayAgent>,
    workspace_tx: WorkspaceRequestSender,
) -> anyhow::Result<()> {
    // Get may_dir first before borrowing session_manager mutably
    let may_dir = app.session_manager.may_dir().to_path_buf();

    // Use last_model from config as default for new session
    let model = app.config.last_model.clone();
    let session = app.session_manager.create_session_with_model(model.clone())?;

    // Extract values we need (clone to release the borrow)
    let sandbox_dir = session.sandbox_dir.clone();
    let session_id = session.id.clone();
    let messages: Vec<_> = session.history.messages().to_vec();

    // Now the session borrow is released, we can call init_session
    app.init_session(&session_id, &messages, model.as_deref());

    // Determine the actual model to use
    let actual_model = model.clone()
        .or_else(|| app.config.last_model.clone())
        .or_else(|| app.config.default_model_for_tier(may_agent::config::ModelTier::Balanced).map(|s| s.to_string()));

    // Create agent - fail if no model configured
    let new_agent = match &actual_model {
        Some(model) => {
            MayAgent::from_config_with_model(
                &app.config,
                sandbox_dir,
                may_dir,
                app.cwd.clone(),
                session_id,
                workspace_tx,
                model,
            ).await?
        }
        None => anyhow::bail!("No model configured"),
    };

    // Track the actual model being used
    if let Some(ref model) = actual_model {
        app.set_current_model(model);
    }

    *agent = Some(new_agent);
    Ok(())
}

/// Load an existing session.
async fn load_session(
    app: &mut App,
    agent: &mut Option<MayAgent>,
    id: &SessionId,
    workspace_tx: WorkspaceRequestSender,
) -> anyhow::Result<()> {
    // Get may_dir first before borrowing session_manager mutably
    let may_dir = app.session_manager.may_dir().to_path_buf();

    let session = app.session_manager.load_session(id)?;

    // Extract values we need (clone to release the borrow)
    let sandbox_dir = session.sandbox_dir.clone();
    let session_id = session.id.clone();
    let session_model = session.model.clone();
    let messages: Vec<_> = session.history.messages().to_vec();

    // Now the session borrow is released, we can call init_session
    app.init_session(&session_id, &messages, session_model.as_deref());

    // Determine the actual model to use
    let actual_model = session_model.or_else(|| app.config.last_model.clone())
        .or_else(|| app.config.default_model_for_tier(may_agent::config::ModelTier::Balanced).map(|s| s.to_string()));

    // Create agent - fail if no model configured
    let new_agent = match &actual_model {
        Some(model) => {
            MayAgent::from_config_with_model(
                &app.config,
                sandbox_dir,
                may_dir,
                app.cwd.clone(),
                session_id,
                workspace_tx,
                model,
            ).await?
        }
        None => anyhow::bail!("No model configured"),
    };

    // Track the actual model being used
    if let Some(ref model) = actual_model {
        app.set_current_model(model);
    }

    *agent = Some(new_agent);
    Ok(())
}

/// Switch to a different model while keeping the current session.
async fn switch_model(
    app: &mut App,
    agent: &mut Option<MayAgent>,
    workspace_tx: WorkspaceRequestSender,
    model: &str,
) -> anyhow::Result<()> {
    // Get current session info
    let session = app.session_manager.current()
        .ok_or_else(|| anyhow::anyhow!("No active session"))?;

    let sandbox_dir = session.sandbox_dir.clone();
    let session_id = session.id.clone();
    let may_dir = app.session_manager.may_dir().to_path_buf();

    // Rebuild agent with new model
    let new_agent = MayAgent::from_config_with_model(
        &app.config,
        sandbox_dir,
        may_dir,
        app.cwd.clone(),
        session_id,
        workspace_tx,
        model,
    ).await?;

    // Track the actual model being used
    app.set_current_model(model);

    *agent = Some(new_agent);
    Ok(())
}

/// Format a message with attachments into a single prompt string.
fn format_message_with_attachments(msg: &PendingMessage) -> String {
    let mut parts = vec![msg.content.clone()];

    for attachment in &msg.attachments {
        match attachment {
            Attachment::Image { path, .. } => {
                parts.push(format!("\n[Attached image: {}]", path.display()));
            }
            Attachment::FileReference { path } => {
                // File content will be read by the agent if needed
                parts.push(format!("\n[File reference: {}]", path.display()));
            }
            Attachment::PastedText { content, saved_path } => {
                let char_count = content.chars().count();
                let xml = build_pasted_text_xml(content, char_count, saved_path.as_ref());
                parts.push(format!("\n{}", xml));
            }
        }
    }

    parts.join("")
}

/// XML structure for pasted text content.
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

/// Build XML for pasted text using serde + quick-xml.
fn build_pasted_text_xml(
    content: &str,
    char_count: usize,
    saved_path: Option<&std::path::PathBuf>,
) -> String {
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

    quick_xml::se::to_string(&xml).unwrap_or_default()
}

/// Process a message by running the agent and handling the stream.
async fn process_message<B: Backend>(
    app: &mut App,
    agent: &mut MayAgent,
    msg: PendingMessage,
    events: &mut EventHandler,
    workspace_rx: &mut WorkspaceRequestReceiver,
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    // Ensure session is persisted before running agent (creates directories if needed)
    app.session_manager.ensure_persisted()?;

    // Check if we need to generate a title (first message in a new session)
    let needs_title = app
        .session_manager
        .current()
        .map(|s| s.title.is_none())
        .unwrap_or(false);

    // Capture user message for title generation
    let user_message = msg.content.clone();

    // Start processing
    app.start_processing(&msg);

    // Format message with attachments
    let prompt = format_message_with_attachments(&msg);

    // Create and pin the stream
    let stream = agent.run(&prompt);
    let mut stream = pin!(stream);

    // Capture assistant response for title generation
    let mut assistant_response = String::new();
    let mut quit_requested = false;

    // Process the stream
    'outer: loop {
        // Draw UI
        terminal.draw(|frame| ui::render(frame, app))?;

        // Prioritize agent events: drain all available agent events first
        // This ensures streaming updates are shown immediately
        loop {
            // Try to get agent event without blocking using poll
            let agent_event = futures_lite::future::poll_once(stream.next()).await;
            match agent_event {
                Some(Some(result)) => {
                    // Capture text for title generation
                    if let Ok(AgentEvent::Text(ref text)) = result {
                        assistant_response.push_str(text);
                    }
                    let ended = app.handle_agent_event(result);
                    if ended {
                        break 'outer;
                    }
                    // Continue draining agent events
                }
                Some(None) => {
                    // Stream ended
                    break 'outer;
                }
                None => {
                    // No agent event ready, break to check terminal events
                    break;
                }
            }
        }

        // Now wait for terminal, agent, or workspace request events
        let event = futures_lite::future::race(
            futures_lite::future::race(
                async {
                    events
                        .next()
                        .await
                        .map(Event::Terminal)
                        .unwrap_or(Event::AgentDone)
                },
                async {
                    stream
                        .next()
                        .await
                        .map(Event::Agent)
                        .unwrap_or(Event::AgentDone)
                },
            ),
            async {
                workspace_rx
                    .recv()
                    .await
                    .map(Event::WorkspaceRequest)
                    .unwrap_or(Event::AgentDone)
            },
        )
        .await;

        match event {
            Event::Terminal(terminal_event) => {
                // Check for interrupt keys (Ctrl+C or Esc) to stop the agent immediately
                if let AppEvent::Key(key_code, modifiers) = &terminal_event {
                    let is_ctrl_c = *key_code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL);
                    let is_esc = *key_code == KeyCode::Esc;
                    if is_ctrl_c || is_esc {
                        // Interrupt the agent
                        tracing::info!("Agent interrupted by user");
                        app.chat.push_text("\n\n[Interrupted]");
                        app.finish_processing();
                        break 'outer;
                    }
                }

                // Handle terminal event (might queue messages)
                match app.handle_event(terminal_event)? {
                    Some(SendAction::Quit) => {
                        quit_requested = true;
                        break 'outer;
                    }
                    Some(SendAction::Send(queued_msg)) => {
                        // Already processing, so queue this message
                        app.queue.enqueue(queued_msg);
                    }
                    Some(SendAction::Queued) => {
                        // Already queued, nothing to do
                    }
                    Some(SendAction::NewSession)
                    | Some(SendAction::LoadSession(_))
                    | Some(SendAction::StartCopilotAuth)
                    | Some(SendAction::SwitchModel(_))
                    | Some(SendAction::OpenCopilotAuthUrl { .. })
                    | Some(SendAction::Compact)
                    | Some(SendAction::McpAdd { .. })
                    | Some(SendAction::McpRemove { .. })
                    | Some(SendAction::McpReconnect { .. }) => {
                        // Can't do these actions while processing - ignore
                        tracing::warn!("Cannot perform this action while agent is processing");
                    }
                    None => {}
                }
            }
            Event::Agent(result) => {
                // Capture text for title generation
                if let Ok(AgentEvent::Text(ref text)) = result {
                    assistant_response.push_str(text);
                }
                let ended = app.handle_agent_event(result);
                if ended {
                    break 'outer;
                }
            }
            Event::WorkspaceRequest(request) => {
                tracing::info!(
                    path = %request.path(),
                    reason = %request.reason(),
                    "Workspace access requested"
                );
                app.mode = may_agent::tui::AppMode::WorkspaceConfirm(request);
            }
            Event::AgentDone => {
                break 'outer;
            }
        }
    }

    // Finish processing
    app.finish_processing();

    // Generate title if needed (first message in new session)
    // Spawn as background task to avoid blocking input
    if needs_title && !quit_requested && !assistant_response.is_empty() {
        let config = app.config.clone();
        let user_msg = user_message.clone();
        let assistant_resp = assistant_response.clone();
        let may_dir = app.session_manager.may_dir().to_path_buf();
        let session_id = app.session_manager.current()
            .map(|s| s.id.clone())
            .expect("session should exist");

        tokio::spawn(async move {
            use may_agent::session::SessionStore;

            if let Some(title) = generate_session_title(&config, &user_msg, &assistant_resp).await {
                // Write title directly to metadata file
                let store = SessionStore::new(may_dir.join("sessions"));

                // Read existing metadata to preserve model
                let mut metadata = store.read_metadata(session_id.as_str());
                metadata.title = Some(title);

                if let Err(e) = store.write_metadata(session_id.as_str(), &metadata) {
                    tracing::warn!("Failed to write session title: {}", e);
                }
            }
        });
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

    // Enter auth mode
    app.start_copilot_auth();

    // Draw initial state
    terminal.draw(|frame| ui::render(frame, app))?;

    // Request device code
    let device_code = match auth::request_device_code().await {
        Ok(dc) => dc,
        Err(e) => {
            app.copilot_auth_failed(e.to_string());
            // Wait for user to acknowledge
            wait_for_auth_dismiss(app, events, terminal).await?;
            return Ok(());
        }
    };

    // Update UI with device code info
    app.set_copilot_device_code(device_code.user_code.clone(), device_code.verification_uri.clone());
    terminal.draw(|frame| ui::render(frame, app))?;

    // Poll for token with interruptible loop
    let poll_interval = std::time::Duration::from_secs(device_code.interval.max(5));

    loop {
        // Wait for poll interval while checking for cancellation
        let deadline = tokio::time::Instant::now() + poll_interval;

        // Check for terminal events during the wait
        loop {
            let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
            if timeout.is_zero() {
                break;
            }

            // Poll for events with a short timeout
            let event_timeout = timeout.min(std::time::Duration::from_millis(100));
            match tokio::time::timeout(event_timeout, events.next()).await {
                Ok(Some(event)) => {
                    if let Some(action) = app.handle_event(event)? {
                        match action {
                            SendAction::Quit => return Ok(()),
                            SendAction::OpenCopilotAuthUrl { url, code } => {
                                // Copy code to clipboard
                                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                    if let Err(e) = clipboard.set_text(&code) {
                                        tracing::warn!("Failed to copy code to clipboard: {}", e);
                                    }
                                }
                                // Open URL in browser
                                if let Err(e) = open::that(&url) {
                                    tracing::warn!("Failed to open URL in browser: {}", e);
                                }
                            }
                            _ => {}
                        }
                    }
                    // Check if mode changed (user pressed Esc)
                    if !matches!(app.mode, may_agent::tui::AppMode::CopilotAuth) {
                        return Ok(());
                    }
                    terminal.draw(|frame| ui::render(frame, app))?;
                }
                Ok(None) => {
                    // Event stream closed
                    return Ok(());
                }
                Err(_) => {
                    // Timeout, continue waiting
                }
            }
        }

        // Try to get token (single poll attempt)
        match auth::try_get_token(&device_code.device_code).await {
            Ok(token) => {
                // Save the token
                if let Err(e) = save_copilot_token(&token) {
                    app.copilot_auth_failed(format!("Failed to save token: {}", e));
                    wait_for_auth_dismiss(app, events, terminal).await?;
                    return Ok(());
                }
                // Add copilot provider to config
                let instance = may_agent::config::ProviderInstance::new(may_agent::config::Provider::Copilot);
                app.config.upsert_provider("copilot", instance);
                app.config_state.load_providers(&app.config);
                if let Err(e) = app.config.save() {
                    tracing::error!("Failed to save config: {}", e);
                }
                // Success!
                app.copilot_auth_success();
                wait_for_auth_dismiss(app, events, terminal).await?;
                return Ok(());
            }
            Err(aither_copilot::CopilotError::AuthorizationPending) => {
                // User hasn't completed auth yet, keep polling
                terminal.draw(|frame| ui::render(frame, app))?;
                continue;
            }
            Err(e) => {
                // Auth failed
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
            // Check if mode changed (user pressed Enter or Esc)
            if !matches!(app.mode, may_agent::tui::AppMode::CopilotAuth) {
                break;
            }
        }
        terminal.draw(|frame| ui::render(frame, app))?;
    }

    Ok(())
}
