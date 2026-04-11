//! May TUI - Terminal interface for May agent.
//!
//! Architecture:
//! - Main thread: Handles UI rendering and terminal events only
//! - Background tasks: Run agent processing, send events via channels
//! - Communication: All through `async_channel` (runtime-agnostic)
//!
//! Module layout:
//! - [`cli`]: argv parsing, [`cli::CliMode`] dispatch enum, and all
//!   subcommand argument structs.
//! - [`browser`]: `may browser` implementation (browser service daemon,
//!   CDP fetching, stdout JSON rendering).
//! - `main.rs` (this file): entry point, logging setup, exec subcommand,
//!   one-shot subcommand, and the long-lived TUI event loop.

mod browser;
mod cli;
mod exec;

use std::io;
use std::io::Write;
use std::panic;

use aither_agent::AgentEvent;
use anyhow::Context;

use crate::browser::run_browser;
use crate::cli::{CliMode, OneShotArgs, parse_cli_mode_from_env};
use crate::exec::run_exec;

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
use jiff::Timestamp;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::backend::CrosstermBackend;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use may_agent::agent::{AgentHandle, AgentWorker, TuiEvent};
use may_agent::ask_user::{self, AskUserQueue};
use may_agent::tui::runtime::{LocalSessionRuntime, RuntimeDeps, TitleGenContext};
use may_agent::tui::{App, AppEvent, EventHandler, PendingMessage, SendAction, ui};
use may_agent::workspace_request::{self, WorkspaceRequestQueue};
use may_agent::{
    MayMcpServerConfig, McpManager, SkillManager,
};

const PANIC_MESSAGE: &str = include_str!("../../texts/panic_message.txt");

fn ensure_rustls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = try_init_global_executor(TokioGlobal);
    ensure_rustls_provider();

    let cli_mode = parse_cli_mode_from_env();
    let is_ui_mode = matches!(cli_mode, CliMode::Ui(_));

    // Initialize logging to file (shared by both UI server and TUI modes)
    let log_dir = dirs::home_dir()
        .context("Failed to determine home directory")?
        .join(".may")
        .join("logs");
    std::fs::create_dir_all(&log_dir).context("Failed to create ~/.may/logs directory")?;
    let env_filter = EnvFilter::from_default_env()
        .add_directive("may=info".parse()?)
        .add_directive("may_agent=debug".parse()?)
        .add_directive("aither_agent=debug".parse()?)
        .add_directive("aither_sandbox=info".parse()?)
        .add_directive("aither_cloud=debug".parse()?)
        .add_directive("aither_gemini=info".parse()?)
        .add_directive("aither_claude=info".parse()?)
        .add_directive("aither_openai=info".parse()?);

    if is_ui_mode {
        let file_appender = tracing_appender::rolling::daily(log_dir, "may.log");
        tracing_subscriber::registry()
            .with(fmt::layer().with_writer(file_appender).with_ansi(false))
            .with(fmt::layer().with_writer(io::stderr).with_ansi(true))
            .with(env_filter)
            .init();
    } else {
        let file_appender = tracing_appender::rolling::daily(log_dir, "may.log");
        tracing_subscriber::registry()
            .with(fmt::layer().with_writer(file_appender).with_ansi(false))
            .with(env_filter)
            .init();
    }

    if !matches!(cli_mode, CliMode::Help(_)) {
        may_agent::configure_heel_binary().await;
    }

    match cli_mode {
        CliMode::Help(help_text) => {
            use std::io::Write as _;
            io::stdout().write_all(help_text.as_bytes())?;
            io::stdout().write_all(
                b"
",
            )?;
            return Ok(());
        }
        CliMode::Ui(ui_args) => return may_agent::ui_server::run_ui_server(&ui_args).await,
        CliMode::Browser(args) => std::process::exit(run_browser(args).await?),
        CliMode::Exec(args) => std::process::exit(run_exec(args).await?),
        CliMode::OneShot(args) => return run_oneshot(args).await,
        CliMode::Tui => {}
    }

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



#[cfg(test)]
mod tests {
    use super::parse_oneshot_selection;

    #[test]
    fn parse_oneshot_selection_accepts_single_choice() {
        let options = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let selected =
            parse_oneshot_selection("2", &options, false).expect("single selection must parse");
        assert_eq!(selected, vec!["b".to_string()]);
    }

    #[test]
    fn parse_oneshot_selection_rejects_multiple_when_not_allowed() {
        let options = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert!(parse_oneshot_selection("1,2", &options, false).is_none());
    }

    #[test]
    fn parse_oneshot_selection_accepts_multi_select() {
        let options = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let selected =
            parse_oneshot_selection("1,3", &options, true).expect("multi selection must parse");
        assert_eq!(selected, vec!["a".to_string(), "c".to_string()]);
    }
}

async fn run_oneshot(args: OneShotArgs) -> anyhow::Result<()> {
    const ONESHOT_IDLE_FINISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    let (event_tx, _event_rx) = async_channel::unbounded();
    let mut app = App::new(event_tx, None).await?;
    if !app.is_configured() {
        anyhow::bail!("May is not configured");
    }

    if let Some(model) = args.model.as_ref() {
        app.config.last_model = Some(model.clone());
    }

    let (workspace_broker, workspace_queue) = workspace_request::channel();
    let (ask_user_broker, ask_user_queue) = ask_user::channel();
    let mut local_runtime = LocalSessionRuntime::new(app.session_manager.db().clone());
    local_runtime.enable_oneshot_restricted_mode();
    let tool_dispatchers = may_agent::tool_dispatch::ToolDispatchers::spawn(
        may_agent::tool_dispatch::ToolDispatchConfig {
            may_dir: app.session_manager.may_dir().to_path_buf(),
            cwd: app.cwd.clone(),
            db: app.session_manager.db().clone(),
            browser_config: app.config.browser.clone(),
            ssh_servers: app.config.ssh_servers.clone(),
            network_policy: app.config.preferences.network_policy.clone(),
            ask_user_broker: ask_user_broker.clone(),
        },
    )
    .await?;
    let runtime_deps = RuntimeDeps::new(
        local_runtime,
        workspace_broker,
        ask_user_broker,
        tool_dispatchers,
        std::sync::Arc::new(may_agent::location::LocationProvider::new()),
    )
    .with_oneshot_overrides(args.model.clone(), args.reasoning_effort);

    let mut worker: Option<AgentWorker> = None;
    runtime_deps
        .start_new_session(&mut app, &mut worker)
        .await?;
    let worker = worker
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("oneshot worker was not initialized"))?;

    let mut pending_title_gen: Option<TitleGenContext> = None;
    let mut assistant_response = String::new();
    let mut active_tool_calls = std::collections::HashSet::<String>::new();
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    runtime_deps.mark_running(&app).await?;
    let prompt = build_oneshot_prompt_with_mentions(&app, &args.prompt).await?;
    let handle = runtime_deps
        .start_agent_task(
            &mut app,
            worker,
            PendingMessage {
                content: prompt.content,
                hidden_context_xml: prompt.hidden_context_xml,
                attachments: Vec::new(),
                queued_at: Timestamp::now(),
                delivery_mode: may_agent::session::PendingUserMessageMode::Steer,
            },
            &mut pending_title_gen,
        )
        .await?;
    let mut agent_handle = Some(handle);

    loop {
        if let Some(request) = runtime_deps.pending_permission(&app).await? {
            let approved = prompt_oneshot_permission(&request.request, &mut stderr)?;
            runtime_deps
                .resolve_permission_response(&mut app, &request.id, approved)
                .await?;
            continue;
        }
        enum Event {
            AskUser(may_agent::ask_user::AskUserRequest),
            Workspace(may_agent::workspace_request::WorkspaceRequest),
            Agent(TuiEvent),
        }

        let Some(handle) = agent_handle.as_ref() else {
            break;
        };
        let event_future = futures_lite::future::or(
            async { ask_user_queue.next().await.map(Event::AskUser) },
            futures_lite::future::or(
                async { workspace_queue.next().await.map(Event::Workspace) },
                async { handle.recv().await.map(Event::Agent) },
            ),
        );
        let next = if assistant_response.is_empty() || !active_tool_calls.is_empty() {
            event_future.await
        } else {
            match tokio::time::timeout(ONESHOT_IDLE_FINISH_TIMEOUT, event_future).await {
                Ok(event) => event,
                Err(_) => {
                    if let Some(handle) = agent_handle.take() {
                        handle.cancel();
                    }
                    runtime_deps
                        .finish_agent_task(
                            &mut app,
                            &mut agent_handle,
                            &mut pending_title_gen,
                            &assistant_response,
                        )
                        .await;
                    if !assistant_response.ends_with('\n') {
                        stdout.write_all(b"\n")?;
                        stdout.flush()?;
                    }
                    return Ok(());
                }
            }
        };
        let Some(event) = next else {
            break;
        };
        match event {
            Event::AskUser(request) => {
                let session_id = app
                    .session_manager
                    .current()
                    .map(|session| session.id.clone())
                    .ok_or_else(|| anyhow::anyhow!("oneshot session missing"))?;
                let questions = request.args.request.clone().into_questions();
                let request_id = runtime_deps
                    .register_ask_user_request(&mut app, &session_id, &questions)
                    .await?;
                let answers = prompt_oneshot_questions(&questions, &mut stderr)?;
                runtime_deps
                    .resolve_ask_user_response(&mut app, &session_id, &request_id, &answers)
                    .await?;
                request.respond(answers).map_err(anyhow::Error::from)?;
            }
            Event::Workspace(request) => {
                let session_id = app
                    .session_manager
                    .current()
                    .map(|session| session.id.clone())
                    .ok_or_else(|| anyhow::anyhow!("oneshot session missing"))?;
                let request_id = runtime_deps
                    .register_workspace_request(&mut app, &session_id, &request.args.request)
                    .await?;
                let approved = prompt_oneshot_workspace(&request.args.request, &mut stderr)?;
                runtime_deps
                    .resolve_workspace_response(&mut app, &session_id, &request_id, approved)
                    .await?;
                request.respond(approved).map_err(anyhow::Error::from)?;
            }
            Event::Agent(TuiEvent::Agent(result)) => {
                if let Ok(AgentEvent::ToolCallStart { id, .. }) = &result {
                    active_tool_calls.insert(id.clone());
                }
                if let Ok(AgentEvent::ToolCallEnd { id, .. }) = &result {
                    active_tool_calls.remove(id);
                }
                if let Ok(AgentEvent::Text(ref text)) = result {
                    assistant_response.push_str(text);
                    stdout.write_all(text.as_bytes())?;
                    stdout.flush()?;
                }
                render_oneshot_event(&mut stderr, &result)?;
                let run_failed = result.is_err();
                let ended = app.handle_agent_event(result);
                if ended {
                    runtime_deps
                        .finish_agent_task(
                            &mut app,
                            &mut agent_handle,
                            &mut pending_title_gen,
                            &assistant_response,
                        )
                        .await;
                    if run_failed {
                        anyhow::bail!("oneshot agent run failed");
                    }
                    if !assistant_response.ends_with('\n') {
                        stdout.write_all(b"\n")?;
                        stdout.flush()?;
                    }
                    return Ok(());
                }
            }
            Event::Agent(TuiEvent::Complete) => {
                runtime_deps
                    .finish_agent_task(
                        &mut app,
                        &mut None,
                        &mut pending_title_gen,
                        &assistant_response,
                    )
                    .await;
                if !assistant_response.ends_with('\n') {
                    stdout.write_all(b"\n")?;
                    stdout.flush()?;
                }
                return Ok(());
            }
            Event::Agent(TuiEvent::Compacted(_)) => {}
            Event::Agent(TuiEvent::Error(error)) => {
                let _ = writeln!(stderr, "[error] {error}");
                runtime_deps
                    .finish_agent_task(
                        &mut app,
                        &mut None,
                        &mut pending_title_gen,
                        &assistant_response,
                    )
                    .await;
                anyhow::bail!(error);
            }
        }
    }

    Ok(())
}

fn prompt_oneshot_questions(
    questions: &[may_agent::ask_user::Question],
    stderr: &mut impl Write,
) -> anyhow::Result<Vec<may_agent::ask_user::QuestionAnswer>> {
    let mut answers = Vec::with_capacity(questions.len());
    for question in questions {
        writeln!(stderr, "[ask_user] {}", question.question)?;
        if !question.section.is_empty() {
            writeln!(stderr, "section: {}", question.section)?;
        }
        for (index, option) in question.option.iter().enumerate() {
            writeln!(stderr, "{}. {}", index + 1, option)?;
        }
        if question.multi_select {
            writeln!(
                stderr,
                "Enter comma-separated numbers, `s` to skip, or any other text for a custom answer:"
            )?;
        } else {
            writeln!(
                stderr,
                "Enter a number, `s` to skip, or any other text for a custom answer:"
            )?;
        }
        stderr.flush()?;
        let answer = loop {
            let input = read_oneshot_line()?;
            let trimmed = input.trim();
            if trimmed.eq_ignore_ascii_case("s") || trimmed.eq_ignore_ascii_case("skip") {
                break may_agent::ask_user::AnswerValue::Skipped;
            }
            if question.option.is_empty() {
                if trimmed.is_empty() {
                    writeln!(stderr, "answer must not be empty")?;
                    stderr.flush()?;
                    continue;
                }
                break may_agent::ask_user::AnswerValue::Custom(trimmed.to_string());
            }
            if let Some(selected) =
                parse_oneshot_selection(trimmed, &question.option, question.multi_select)
            {
                break may_agent::ask_user::AnswerValue::Selected(selected);
            }
            if trimmed.is_empty() {
                writeln!(stderr, "answer must not be empty")?;
                stderr.flush()?;
                continue;
            }
            break may_agent::ask_user::AnswerValue::Custom(trimmed.to_string());
        };
        answers.push(may_agent::ask_user::QuestionAnswer {
            section: question.section.clone(),
            answer,
        });
    }
    Ok(answers)
}

fn prompt_oneshot_workspace(
    request: &may_agent::workspace_request::RequestWorkspaceArgs,
    stderr: &mut impl Write,
) -> anyhow::Result<bool> {
    writeln!(stderr, "[workspace_request] path={}", request.path)?;
    writeln!(stderr, "reason={}", request.reason)?;
    writeln!(stderr, "Approve? [y/N]")?;
    stderr.flush()?;
    loop {
        let input = read_oneshot_line()?;
        let trimmed = input.trim();
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("n")
            || trimmed.eq_ignore_ascii_case("no")
        {
            return Ok(false);
        }
        if trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes") {
            return Ok(true);
        }
        writeln!(stderr, "Please answer `y` or `n`.")?;
        stderr.flush()?;
    }
}

#[derive(serde::Serialize)]
struct OneShotNonInteractiveAlert<'a> {
    #[serde(rename = "@type")]
    alert_type: &'a str,
    message: &'a str,
}

#[derive(Debug, Clone)]
struct OneShotPromptInput {
    content: String,
    hidden_context_xml: Option<may_agent::XmlBlock>,
}

async fn build_oneshot_prompt_with_mentions(
    app: &App,
    prompt: &str,
) -> anyhow::Result<OneShotPromptInput> {
    let mut hidden_context_parts = vec![may_agent::xml::serialize_xml_block(
        "system-reminder",
        &OneShotNonInteractiveAlert {
            alert_type: "oneshot_noninteractive",
            message: "Non-interactive oneshot run. Do not use ask_user or wait for stdin. Make a reasonable assumption when possible; if interaction is truly required, explain the blocker and stop.",
        },
    )?];
    let Some(session) = app.session_manager.current() else {
        return Ok(OneShotPromptInput {
            content: prompt.to_string(),
            hidden_context_xml: Some(may_agent::XmlBlock::parse(
                hidden_context_parts.join("\n\n"),
            )?),
        });
    };

    let db = app.session_manager.db();
    if let Ok(Some(session_mentions_xml)) =
        may_agent::prompt_mentions::build_session_mentions_xml(&db, prompt).await
    {
        hidden_context_parts.push(session_mentions_xml);
    }

    let session_paths = may_agent::session::SessionPaths::new(session.sandbox_dir.clone());
    let files_dir = session_paths.files_dir();
    if let Ok(Some(file_mentions_xml)) =
        may_agent::prompt_mentions::build_file_mentions_xml(&files_dir, prompt, 400).await
    {
        hidden_context_parts.push(file_mentions_xml);
    }

    Ok(OneShotPromptInput {
        content: prompt.to_string(),
        hidden_context_xml: Some(may_agent::XmlBlock::parse(
            hidden_context_parts.join("\n\n"),
        )?),
    })
}

fn prompt_oneshot_permission(
    request: &may_agent::session::PersistedPermissionRequestPayload,
    stderr: &mut impl Write,
) -> anyhow::Result<bool> {
    writeln!(stderr, "[permission_request] mode={}", request.mode)?;
    writeln!(stderr, "script={}", request.script)?;
    writeln!(stderr, "Approve? [y/N]")?;
    stderr.flush()?;
    loop {
        let input = read_oneshot_line()?;
        let trimmed = input.trim();
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("n")
            || trimmed.eq_ignore_ascii_case("no")
        {
            return Ok(false);
        }
        if trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes") {
            return Ok(true);
        }
        writeln!(stderr, "Please answer `y` or `n`.")?;
        stderr.flush()?;
    }
}

fn read_oneshot_line() -> anyhow::Result<String> {
    let mut line = String::new();
    let bytes = io::stdin().read_line(&mut line)?;
    if bytes == 0 {
        anyhow::bail!("oneshot requires interactive stdin for this prompt, but stdin is closed");
    }
    Ok(line)
}

fn parse_oneshot_selection(
    input: &str,
    options: &[String],
    multi_select: bool,
) -> Option<Vec<String>> {
    let mut selected = Vec::new();
    for raw_part in input.split(',') {
        let trimmed = raw_part.trim();
        if trimmed.is_empty() {
            return None;
        }
        let index = trimmed.parse::<usize>().ok()?;
        let option = options.get(index.checked_sub(1)?).cloned()?;
        if !selected.iter().any(|existing| existing == &option) {
            selected.push(option);
        }
    }
    if selected.is_empty() {
        return None;
    }
    if !multi_select && selected.len() > 1 {
        return None;
    }
    Some(selected)
}

fn render_oneshot_event(
    stderr: &mut impl Write,
    result: &Result<AgentEvent, aither_agent::AgentError>,
) -> anyhow::Result<()> {
    match result {
        Ok(AgentEvent::RunStart { .. }) => {
            writeln!(stderr, "Running...")?;
        }
        Ok(AgentEvent::Reasoning(text)) => {
            if !text.trim().is_empty() {
                writeln!(stderr, "Thinking: {text}")?;
            }
        }
        Ok(AgentEvent::ToolCallStart { name, .. }) => {
            writeln!(stderr, "Tool: {name}")?;
        }
        Ok(AgentEvent::ToolCallEnd { name, result, .. }) => {
            if result.is_error() {
                let error = result.error_message().unwrap_or("tool failed");
                writeln!(stderr, "Tool failed: {name}: {error}")?;
            } else {
                writeln!(stderr, "Tool finished: {name}")?;
            }
        }
        Ok(AgentEvent::TurnComplete { .. }) => {}
        Ok(AgentEvent::Checkpoint { .. }) => {}
        Ok(AgentEvent::BackgroundTaskStarted {
            task_id,
            output_file,
            ..
        }) => {
            writeln!(
                stderr,
                "Background task started: {task_id} (output: {output_file})"
            )?;
        }
        Ok(AgentEvent::BackgroundTaskCompleted { task_id, .. }) => {
            writeln!(stderr, "Background task completed: {task_id}")?;
        }
        Ok(AgentEvent::TerminalInputNeeded { task_id, notice }) => {
            writeln!(
                stderr,
                "Terminal input needed{}: {}",
                task_id
                    .as_deref()
                    .map(|id| {
                        let mut text = String::from(" for ");
                        text.push_str(id);
                        text
                    })
                    .unwrap_or_default(),
                notice,
            )?;
        }
        Ok(AgentEvent::RunPaused { reason, .. }) => {
            writeln!(stderr, "Paused: {reason:?}")?;
        }
        Ok(AgentEvent::RunResumed { reason, .. }) => {
            writeln!(stderr, "Resumed: {reason:?}")?;
        }
        Ok(AgentEvent::SkillActivated { name, .. }) => {
            writeln!(stderr, "Skill activated: {name}")?;
        }
        Ok(AgentEvent::Usage(usage)) => {
            writeln!(
                stderr,
                "Usage: prompt={:?} completion={:?} total={:?}",
                usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
            )?;
        }
        Ok(AgentEvent::Complete { .. }) => {}
        Ok(AgentEvent::Text(_)) => {}
        Ok(AgentEvent::Error(error)) => {
            writeln!(stderr, "Error: {error}")?;
        }
        Err(error) => {
            writeln!(stderr, "Error: {error}")?;
        }
    }
    stderr.flush()?;
    Ok(())
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

    // Spawn background auto-archive task
    let _auto_archive = may_agent::tasks::spawn_auto_archive(app.session_manager.db().clone());

    // Create workspace request channel (runtime-agnostic)
    let (workspace_broker, workspace_queue) = workspace_request::channel();

    // Create ask user channel (runtime-agnostic)
    let (ask_user_broker, ask_user_queue) = ask_user::channel();
    tracing::info!("=== MAY STARTUP - channels created ===");

    // Load MCP configuration and connect to servers
    let mut mcp_manager = McpManager::load()
        .await
        .context("failed to load MCP config")?;

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

    let local_runtime = LocalSessionRuntime::new(app.session_manager.db().clone());
    let tool_dispatchers = may_agent::tool_dispatch::ToolDispatchers::spawn(
        may_agent::tool_dispatch::ToolDispatchConfig {
            may_dir: app.session_manager.may_dir().to_path_buf(),
            cwd: app.cwd.clone(),
            db: app.session_manager.db().clone(),
            browser_config: app.config.browser.clone(),
            ssh_servers: app.config.ssh_servers.clone(),
            network_policy: app.config.preferences.network_policy.clone(),
            ask_user_broker: ask_user_broker.clone(),
        },
    )
    .await
    .context("failed to initialize tool dispatch runtime")?;
    let runtime_deps = RuntimeDeps::new(
        local_runtime,
        workspace_broker.clone(),
        ask_user_broker.clone(),
        tool_dispatchers,
        std::sync::Arc::new(may_agent::location::LocationProvider::new()),
    );

    // Only start a session if configuration is complete
    if app.is_configured()
        && let Err(e) = runtime_deps.start_new_session(&mut app, &mut worker).await
    {
        tracing::warn!("Initial session start failed: {}", e);
        app.mode = may_agent::tui::AppMode::Chat;
        app.status = Some(format!(
            "Session start failed: {e} (open /config to fix provider/model)"
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
/// This loop NEVER directly calls `agent.run()` or `stream.next()` - all agent processing
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
            && let Ok(models) = rx.try_recv()
        {
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

        if matches!(app.mode, may_agent::tui::AppMode::Chat)
            && let Some(request) = runtime_deps.pending_permission(app).await?
        {
            app.mode = may_agent::tui::AppMode::PermissionConfirm(
                may_agent::tui::state::PermissionConfirmState {
                    request_id: request.id,
                    request: request.request,
                },
            );
        }

        // Check for queued messages - start processing if agent is idle
        if agent_handle.is_none()
            && let Some(msg) = runtime_deps.take_next_dispatchable(app).await?
            && let Some(w) = worker.as_ref()
        {
            runtime_deps.mark_running(app).await?;
            agent_handle = Some(
                runtime_deps
                    .start_agent_task(app, w, msg, &mut pending_title_gen)
                    .await?,
            );
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
                let questions = request.args.request.clone().into_questions();
                tracing::info!(question_count = questions.len(), "User question requested");
                let session_id = app
                    .session_manager
                    .current()
                    .map(|session| session.id.clone())
                    .ok_or_else(|| anyhow::anyhow!("No active session"))?;
                let request_id = runtime_deps
                    .register_ask_user_request(app, &session_id, &questions)
                    .await?;
                let state = may_agent::tui::AskUserState::new(request_id, request, questions);
                app.mode = may_agent::tui::AppMode::AskUser(state);
            }

            Event::Workspace(request) => {
                tracing::info!(
                    path = %request.args.request.path,
                    reason = %request.args.request.reason,
                    "Workspace access requested"
                );
                let session_id = app
                    .session_manager
                    .current()
                    .map(|session| session.id.clone())
                    .ok_or_else(|| anyhow::anyhow!("No active session"))?;
                let request_id = runtime_deps
                    .register_workspace_request(app, &session_id, &request.args.request)
                    .await?;
                app.mode = may_agent::tui::AppMode::WorkspaceConfirm(
                    may_agent::tui::state::WorkspaceConfirmState {
                        request_id,
                        request,
                    },
                );
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
                    && let AppEvent::Key(key_code, modifiers) = &terminal_event
                {
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
                        runtime_deps
                            .finish_agent_task(
                                app,
                                &mut agent_handle,
                                &mut pending_title_gen,
                                &assistant_response,
                            )
                            .await;
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
                        if agent_handle.is_none()
                            && let Some(w) = worker.as_ref()
                        {
                            runtime_deps.mark_running(app).await?;
                            agent_handle = Some(
                                runtime_deps
                                    .start_agent_task(app, w, msg, &mut pending_title_gen)
                                    .await?,
                            );
                            assistant_response.clear();
                        }
                    }
                    Some(SendAction::EnqueuePending(msg)) => {
                        runtime_deps.enqueue_pending_message(app, msg).await?;
                    }
                    Some(SendAction::RespondAskUser {
                        request_id,
                        request,
                        answers,
                    }) => {
                        let session_id = app
                            .session_manager
                            .current()
                            .map(|session| session.id.clone())
                            .ok_or_else(|| anyhow::anyhow!("No active session"))?;
                        runtime_deps
                            .resolve_ask_user_response(app, &session_id, &request_id, &answers)
                            .await?;
                        request.respond(answers).unwrap_or_else(|_| {
                            panic!("{}", may_agent::tui::app::ERR_TOOL_RESPONSE_SEND.trim())
                        });
                    }
                    Some(SendAction::StartCopilotAuth) => {
                        run_copilot_auth(app, events, terminal).await?;
                    }
                    Some(SendAction::SwitchModel(model)) => {
                        if let Err(e) = runtime_deps.switch_model(app, worker, &model).await {
                            let err_msg = format!("Failed to switch model: {e}");
                            app.chat.push_error(&err_msg);
                        } else {
                            tracing::info!("Switched model to: {}", model);
                            app.status = Some(format!("Model: {model}"));
                            if let Err(e) =
                                app.session_manager.set_current_model(model.clone()).await
                            {
                                tracing::warn!("Failed to save model to session: {}", e);
                            }
                            app.config.last_model = Some(model);
                            if let Err(e) = app.config.clone().save().await {
                                tracing::warn!("Failed to save config: {}", e);
                            }
                        }
                    }
                    Some(SendAction::OpenCopilotAuthUrl { url, code }) => {
                        if let Ok(mut clipboard) = arboard::Clipboard::new()
                            && let Err(e) = clipboard.set_text(&code)
                        {
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
                                app.status = Some(format!("Added MCP server: {name}"));
                                app.mcp_servers = mcp_manager.server_info();
                                tracing::info!("Added MCP server: {}", name);
                            }
                            Err(e) => {
                                let err_msg = format!("Failed to add MCP server: {e}");
                                app.chat.push_error(&err_msg);
                                tracing::error!("{}", err_msg);
                            }
                        }
                    }
                    Some(SendAction::McpRemove { name }) => {
                        match mcp_manager.remove_server(&name).await {
                            Ok(()) => {
                                app.status = Some(format!("Removed MCP server: {name}"));
                                app.mcp_servers = mcp_manager.server_info();
                                if app.mcp_selected >= app.mcp_servers.len()
                                    && !app.mcp_servers.is_empty()
                                {
                                    app.mcp_selected = app.mcp_servers.len() - 1;
                                }
                                tracing::info!("Removed MCP server: {}", name);
                            }
                            Err(e) => {
                                let err_msg = format!("Failed to remove MCP server: {e}");
                                app.chat.push_error(&err_msg);
                                tracing::error!("{}", err_msg);
                            }
                        }
                    }
                    Some(SendAction::McpReconnect { name }) => {
                        match mcp_manager.reconnect(&name).await {
                            Ok(()) => {
                                app.status = Some(format!("Reconnected to MCP server: {name}"));
                                app.mcp_servers = mcp_manager.server_info();
                                tracing::info!("Reconnected to MCP server: {}", name);
                            }
                            Err(e) => {
                                let err_msg = format!("Failed to reconnect to MCP server: {e}");
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
                        if let Err(e) = app.config.clone().save().await {
                            let err_msg = format!("Failed to save SSH server: {e}");
                            app.chat.push_error(&err_msg);
                        } else {
                            app.status = Some(format!("Added SSH server: {name}"));
                            if app.ssh_selected + 1 < app.config.ssh_servers.len() {
                                app.ssh_selected = app.config.ssh_servers.len() - 1;
                            }
                        }
                    }
                    Some(SendAction::SshRemove { name }) => {
                        if app.config.remove_ssh_server(&name) {
                            if let Err(e) = app.config.clone().save().await {
                                let err_msg = format!("Failed to save SSH config: {e}");
                                app.chat.push_error(&err_msg);
                            } else {
                                app.status = Some(format!("Removed SSH server: {name}"));
                                if app.ssh_selected >= app.config.ssh_servers.len()
                                    && !app.config.ssh_servers.is_empty()
                                {
                                    app.ssh_selected = app.config.ssh_servers.len() - 1;
                                }
                            }
                        }
                    }
                    Some(SendAction::RespondWorkspace {
                        request_id,
                        request,
                        approved,
                    }) => {
                        let session_id = app
                            .session_manager
                            .current()
                            .map(|session| session.id.clone())
                            .ok_or_else(|| anyhow::anyhow!("No active session"))?;
                        runtime_deps
                            .resolve_workspace_response(app, &session_id, &request_id, approved)
                            .await?;
                        request.respond(approved).unwrap_or_else(|_| {
                            panic!("{}", may_agent::tui::app::ERR_TOOL_RESPONSE_SEND.trim())
                        });
                    }
                    Some(SendAction::RespondPermission {
                        request_id,
                        approved,
                    }) => {
                        runtime_deps
                            .resolve_permission_response(app, &request_id, approved)
                            .await?;
                    }
                    Some(SendAction::SkillInstall { repo }) => {
                        let tx = events.sender();
                        let may_dir = app.session_manager.may_dir().to_path_buf();
                        let profile_id = app
                            .session_manager
                            .current_profile_id()
                            .map(ToOwned::to_owned)
                            .expect("session manager must always have a current profile");
                        app.status = Some(format!("Installing from {repo}..."));
                        spawn(async move {
                            let home_paths = may_agent::MayHomePaths::new(may_dir);
                            let manager = SkillManager::from_profile_home(
                                &home_paths.profile_home(profile_id.as_str()),
                            );
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
                                    final_result.map_err(may_agent::tui::error::TuiError::from),
                                ))
                                .await;
                        })
                        .detach();
                    }
                    Some(SendAction::SkillDelete { dir_name }) => {
                        let tx = events.sender();
                        let may_dir = app.session_manager.may_dir().to_path_buf();
                        let profile_id = app
                            .session_manager
                            .current_profile_id()
                            .map(ToOwned::to_owned)
                            .expect("session manager must always have a current profile");
                        app.status = Some(format!("Deleting {dir_name}..."));
                        spawn(async move {
                            let home_paths = may_agent::MayHomePaths::new(may_dir);
                            let manager = SkillManager::from_profile_home(
                                &home_paths.profile_home(profile_id.as_str()),
                            );
                            let result = manager.delete_skill(&dir_name).await;
                            // Refresh skill list after delete
                            let final_result = match result {
                                Ok(()) => manager.list_skills().await,
                                Err(e) => Err(e),
                            };
                            let _ = tx
                                .send(AppEvent::SkillDeleted(
                                    final_result.map_err(may_agent::tui::error::TuiError::from),
                                ))
                                .await;
                        })
                        .detach();
                    }
                    None => {}
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
                                    && let Err(e) = clipboard.set_text(&code)
                                {
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
                    app.copilot_auth_failed(format!("Failed to save token: {e}"));
                    wait_for_auth_dismiss(app, events, terminal).await?;
                    return Ok(());
                }
                let instance = may_agent::config::ProviderInstance::new(
                    may_agent::config::ProviderType::Copilot,
                );
                app.config.upsert_provider("copilot", instance);
                app.config_state.load_providers(&app.config);
                if let Err(e) = app.config.clone().save().await {
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
