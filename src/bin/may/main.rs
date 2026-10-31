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
mod oneshot;

use std::io;
use std::io::Write as _;
use std::mem;
use std::panic;

use aither_agent::{AgentEvent, TimedEvent};
use anyhow::Context;
use askama::Template as _;

use crate::browser::run_browser;
use crate::cli::{CliMode, parse_cli_mode_from_env};
use crate::exec::run_exec;
use crate::oneshot::run_oneshot;

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
use may_agent::session::RunOutcome;
use may_agent::tui::runtime::{
    AssistantReply, LocalSessionRuntime, QueuedRunStart, ResponseCommit, RuntimeDeps,
    TitleGenContext,
};
use may_agent::tui::{App, AppEvent, EventHandler, SendAction, ui};
use may_agent::workspace_request::{self, WorkspaceRequestQueue};
use may_agent::{MayMcpServerConfig, McpManager, SkillManager};

const PANIC_MESSAGE: &str = include_str!("../../texts/panic_message.txt");

/// Header of a `/bug` or panic snapshot, rendered ahead of the ring dump.
#[derive(askama::Template)]
#[template(path = "bug_report.txt", escape = "none")]
struct BugReportHeader<'a> {
    version: &'a str,
    timestamp: &'a str,
    os: &'a str,
    arch: &'a str,
    session_id: &'a str,
    mode: &'a str,
}

/// `/bug`: dump the session context and the recent log ring to a new file
/// under `~/.may/bugs`, returning where it landed.
fn write_bug_snapshot(
    app: &App,
    log_ring: &may_agent::log_ring::LogRingHandle,
) -> anyhow::Result<std::path::PathBuf> {
    let bugs_dir = dirs::home_dir()
        .context("Failed to determine home directory")?
        .join(".may")
        .join("bugs");
    let timestamp = jiff::Timestamp::now().to_string();
    let header = BugReportHeader {
        version: env!("CARGO_PKG_VERSION"),
        timestamp: timestamp.as_str(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        session_id: app
            .session_manager
            .current()
            .map_or("none", |session| session.id.as_str()),
        mode: app.mode.name(),
    }
    .render()
    .context("Failed to render the bug report header")?;
    let path = bugs_dir.join(format!("bug-{}.log", jiff::Timestamp::now().as_second()));
    log_ring.dump_to(&path, &header)?;
    tracing::info!(path = %path.display(), "bug snapshot written");
    Ok(path)
}

/// Stack every tokio worker gets. Spawned threads default to the pthread
/// stack — 512 KiB on macOS (Linux gives ~8 MiB) — and a terminal-bound
/// command polls its whole chain on one stack: `subagent` reaches
/// `route_model` → `generate` → `respond` → hyper nested inside the IPC
/// call inside the parent run, which aborts debug builds with a stack
/// overflow. Workers get the main thread's own budget.
const WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;

fn ensure_rustls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn tokio_runtime() -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_BYTES)
        .build()
}

fn main() -> anyhow::Result<()> {
    tokio_runtime()
        .context("failed to build the tokio runtime")?
        .block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    let _ = try_init_global_executor(TokioGlobal);
    ensure_rustls_provider();

    let cli_mode = parse_cli_mode_from_env();
    // The daemon's stderr is its log file; its diagnostics go there too.
    let is_server_mode = matches!(
        cli_mode,
        CliMode::Ui(_) | CliMode::Rest(_) | CliMode::Daemon(cli::DaemonCommand::Run)
    );

    // Diagnostics live in a bounded in-memory ring; `/bug` and the panic
    // hook dump it on demand under ~/.may/bugs.
    let (log_writer, log_ring) =
        may_agent::log_ring::LogRingHandle::new(may_agent::log_ring::DEFAULT_LOG_RING_CAPACITY);
    let env_filter = EnvFilter::from_default_env()
        .add_directive("may=info".parse()?)
        .add_directive("may_agent=debug".parse()?)
        .add_directive("aither_agent=debug".parse()?)
        .add_directive("aither_sandbox=info".parse()?)
        .add_directive("aither_cloud=debug".parse()?)
        .add_directive("aither_gemini=info".parse()?)
        .add_directive("aither_claude=info".parse()?)
        .add_directive("aither_openai=info".parse()?)
        // skyzen logs every endpoint error here (5xx at `error` with the cause
        // chain, 4xx at `warn`) before redacting a 5xx response body; it is
        // the only record of a 500's cause.
        .add_directive("skyzen_core=warn".parse()?);

    if is_server_mode {
        tracing_subscriber::registry()
            .with(fmt::layer().with_writer(log_writer).with_ansi(false))
            .with(
                fmt::layer()
                    .with_writer(io::stderr)
                    .with_ansi(std::io::IsTerminal::is_terminal(&io::stderr())),
            )
            .with(env_filter)
            .init();
    } else {
        tracing_subscriber::registry()
            .with(fmt::layer().with_writer(log_writer).with_ansi(false))
            .with(env_filter)
            .init();
    }

    // A panic dumps the panic message and the recent log to ~/.may/bugs
    // before the previous hook restores the terminal / prints the trace.
    let panic_ring = log_ring.clone();
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        if let Some(bugs_dir) = dirs::home_dir().map(|home| home.join(".may").join("bugs"))
            && let Err(error) = panic_ring.dump_to(
                &bugs_dir.join(format!("panic-{}.log", jiff::Timestamp::now().as_second())),
                &panic_info.to_string(),
            )
        {
            eprintln!("failed to write the panic log dump: {error}");
        }
        previous_hook(panic_info);
    }));

    if !matches!(cli_mode, CliMode::Help(_)) {
        may_agent::startup::validate_configuration().await?;
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
        CliMode::Ui(_) => return open_web_ui().await,
        CliMode::Rest(rest_args) => {
            return may_agent::daemon::run_daemon(may_agent::daemon::DaemonOptions {
                mode: may_agent::ui_server::UiServerMode::Rest,
                tcp: Some(may_agent::ui_server::parse_listen_args(&rest_args)?),
            })
            .await;
        }
        CliMode::Daemon(command) => return run_daemon_command(command).await,
        CliMode::Browser(args) => std::process::exit(run_browser(args).await?),
        CliMode::Exec(args) => std::process::exit(Box::pin(run_exec(args)).await?),
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
    let result = run_app(&mut terminal, image_support, log_ring).await;

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
    log_ring: may_agent::log_ring::LogRingHandle,
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
    let tool_distribution = std::sync::Arc::new(
        may_agent::tools::embedded_distribution()
            .context("embedded tool distribution is invalid")?,
    );
    let tool_dispatchers = may_agent::tool_dispatch::ToolDispatchers::spawn(
        may_agent::tool_dispatch::ToolDispatchConfig {
            may_dir: app.session_manager.may_dir().to_path_buf(),
            cwd: app.cwd.clone(),
            db: app.session_manager.db().clone(),
            browser_config: app.config.browser.clone(),
            network_policy: app.config.preferences.network_policy.clone(),
            ask_user_broker: ask_user_broker.clone(),
            tool_distribution: tool_distribution.clone(),
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
        tool_distribution,
        may_agent::agent::ToolEnvironmentCache::spawn(),
    );

    // Agents run in the May daemon; the TUI is its client. Starting or
    // replacing the daemon shows on the status line meanwhile.
    app.set_status("Connecting to the May daemon…");
    terminal.draw(|frame| ui::render(frame, &mut app))?;
    let paths = may_agent::daemon::DaemonPaths::user()?;
    let runtime_deps = match may_agent::daemon::lifecycle::ensure_running(&paths, |_| {}).await {
        Ok(client) => {
            app.status = None;
            app.btw_daemon = Some(client.clone());
            runtime_deps.with_daemon(client)
        }
        Err(error) => {
            return Err(anyhow::anyhow!("the May daemon is unavailable: {error}"));
        }
    };

    // Only start a session if configuration is complete
    let mut worker_build = None;
    if app.is_configured() {
        match runtime_deps.begin_new_session(&mut app, &mut worker).await {
            Ok(build) => {
                worker_build = Some(WorkerBuild {
                    purpose: WorkerBuildPurpose::Startup,
                    build,
                });
            }
            Err(e) => report_startup_failure(&mut app, &e),
        }
    }

    // Main event loop - only receives from channels, never blocks on agent processing
    main_event_loop(
        &mut app,
        &mut worker,
        worker_build,
        &mut events,
        terminal,
        runtime_deps,
        workspace_queue,
        ask_user_queue,
        &mut mcp_manager,
        log_ring,
    )
    .await
}

/// Why the event loop is building an agent worker; decides how its outcome
/// is reported.
#[derive(Debug, Clone, Copy)]
enum WorkerBuildPurpose {
    /// The session started with the TUI; failure leaves it unconfigured.
    Startup,
    /// A user-requested new or loaded session; failure ends the TUI.
    Session,
    /// A user-requested model switch; failure keeps the current worker.
    SwitchModel,
    /// A saved config change to what the agent is built from; failure keeps
    /// the current worker.
    Reconfigure,
}

/// A worker build in flight, raced by the event loop against its queues so
/// prompts raised while preparing tool environments are answered.
struct WorkerBuild {
    purpose: WorkerBuildPurpose,
    build: may_agent::tui::runtime::PendingWorkerBuild,
}

fn report_startup_failure(app: &mut may_agent::tui::App, error: &impl std::fmt::Display) {
    tracing::warn!("Initial session start failed: {}", error);
    app.mode = may_agent::tui::AppMode::Chat;
    app.set_status(format!(
        "Session start failed: {error} (open /config to fix provider/model)"
    ));
}

/// Applies a finished worker build to the app.
async fn finish_worker_build(
    app: &mut may_agent::tui::App,
    worker: &mut Option<AgentWorker>,
    finished: WorkerBuild,
    built: Result<AgentWorker, may_agent::tui::runtime::TuiRuntimeError>,
) -> anyhow::Result<()> {
    let WorkerBuild { purpose, build } = finished;
    let built = match (purpose, built) {
        (WorkerBuildPurpose::Reconfigure, Err(error)) => {
            // Settled either way: the failure is reported once, not retried
            // on every loop pass.
            app.worker_settings = build.settings().clone();
            app.chat
                .push_error(&format!("Failed to apply the saved settings: {error}"));
            return Ok(());
        }
        (_, Ok(built)) => built,
        (WorkerBuildPurpose::Startup, Err(error)) => {
            report_startup_failure(app, &error);
            return Ok(());
        }
        (WorkerBuildPurpose::Session, Err(error)) => return Err(error.into()),
        (WorkerBuildPurpose::SwitchModel, Err(error)) => {
            app.chat
                .push_error(&format!("Failed to switch model: {error}"));
            return Ok(());
        }
    };
    RuntimeDeps::install_worker(app, worker, &build, built);
    if matches!(purpose, WorkerBuildPurpose::SwitchModel) {
        let model = build.model().to_string();
        tracing::info!("Switched model to: {}", model);
        app.set_status(format!("Model: {model}"));
        if let Err(e) = app.session_manager.set_current_model(model.clone()).await {
            tracing::warn!("Failed to save model to session: {}", e);
        }
        app.config.last_model = Some(
            model
                .parse::<may_agent::ModelSelector>()
                .expect("interactive model switch must persist a valid provider::model selector"),
        );
        if let Err(e) = app.config.clone().save().await {
            tracing::warn!("Failed to save config: {}", e);
        }
    }
    Ok(())
}

/// The main event loop - handles terminal events and channel messages.
///
/// This loop NEVER directly calls `agent.run()` or `stream.next()` - all agent processing
/// happens in background tasks that communicate via channels.
#[allow(clippy::too_many_arguments)]
async fn main_event_loop<B: Backend>(
    app: &mut App,
    worker: &mut Option<AgentWorker>,
    mut worker_build: Option<WorkerBuild>,
    events: &mut EventHandler,
    terminal: &mut Terminal<B>,
    runtime_deps: RuntimeDeps,
    workspace_queue: WorkspaceRequestQueue,
    ask_user_queue: AskUserQueue,
    mcp_manager: &mut McpManager,
    log_ring: may_agent::log_ring::LogRingHandle,
) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    // Handle to a running agent task (if any)
    let mut agent_handle: Option<AgentHandle> = None;

    // Handle to a `/context` measurement (deferred behind any running turn)
    let mut context_handle: Option<AgentHandle> = None;

    // For title generation
    let mut pending_title_gen: Option<TitleGenContext> = None;
    // The busy status shows once per episode of another run holding the session.
    let mut busy_elsewhere_notified = false;
    let mut assistant_response = AssistantReply::default();
    // Daemon-held prompts already shown, so a prompt answered moments ago is
    // not reopened before the daemon clears it.
    let mut shown_prompts = std::collections::HashSet::<String>::new();

    // Sockets through which other main agents reach this TUI's session
    let agent_listeners =
        may_agent::agent_link::AgentListeners::new(tokio::runtime::Handle::current());

    // Frames are drawn when something changed: every event but the idle
    // tick redraws. A tick redraws while a turn animates its spinner, and
    // otherwise at most once a second so elapsed counters, auto-collapsing
    // tool calls and expiring notices still advance.
    let mut needs_draw = true;
    let mut last_draw = std::time::Instant::now();

    loop {
        enum Event {
            AskUser(may_agent::ask_user::AskUserRequest),
            Workspace(may_agent::workspace_request::WorkspaceRequest),
            WorkerBuilt(Box<Result<AgentWorker, may_agent::tui::runtime::TuiRuntimeError>>),
            Agent(TuiEvent),
            ForeignTurn(may_agent::agent::ForeignTurn),
            Context(Option<TuiEvent>),
            Terminal(Box<AppEvent>),
        }

        tracing::info!("=== LOOP START ===");
        let mut phases = may_agent::tui::perf::LoopPhases::default();

        // First draw: updates content_height/view_height for accurate overflow calculation.
        if needs_draw {
            let draw_start = std::time::Instant::now();
            terminal.draw(|frame| ui::render(frame, app))?;
            phases.draw = draw_start.elapsed();
            last_draw = draw_start;
            tracing::info!("after draw");
        }
        // Any path back to the top of the loop redraws unless an idle tick
        // below says otherwise.
        needs_draw = true;

        // Check for model fetch results (non-blocking)
        if let Some(ref mut rx) = app.model_fetch_rx
            && let Ok(models) = rx.try_recv()
        {
            app.handle_fetched_models(models);
            app.model_fetch_rx = None;
        }

        // Apply finished file diffs to their tool-call blocks (non-blocking)
        app.poll_file_diffs();

        // Check for clipboard paste results (non-blocking)
        if let Some(ref mut rx) = app.clipboard_rx {
            match rx.try_recv() {
                Ok(result) => {
                    app.clipboard_rx = None;
                    app.pending_attachment_jobs = app.pending_attachment_jobs.saturating_sub(1);
                    app.enqueue_clipboard_result(result);
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

        let step_start = std::time::Instant::now();
        while let Some(paste) = app.dequeue_pending_paste() {
            app.handle_pending_paste(paste).await?;
        }
        phases.step("paste_results", step_start.elapsed());

        let step_start = std::time::Instant::now();
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
        // A daemon run waits on its prompts in the daemon; they show here
        // from the session's runtime state and are answered over REST.
        if runtime_deps.daemon().is_some() && matches!(app.mode, may_agent::tui::AppMode::Chat) {
            if let Some(request) = runtime_deps.pending_ask_user(app).await?
                && shown_prompts.insert(request.id.clone())
            {
                app.show_blocking_prompt(may_agent::tui::AppMode::AskUser(
                    may_agent::tui::AskUserState::for_open_question(request),
                ));
            } else if let Some(request) = runtime_deps.pending_workspace(app).await?
                && shown_prompts.insert(request.id.clone())
            {
                app.show_blocking_prompt(may_agent::tui::AppMode::WorkspaceConfirm(
                    may_agent::tui::state::WorkspaceConfirmState {
                        request_id: request.id,
                        path: request.request.path,
                        reason: request.request.reason,
                        request: None,
                    },
                ));
            }
        }
        phases.step("pending_permission", step_start.elapsed());

        app.restore_parked_question();

        // Open questions are listed in the status bar and opened with
        // `/questions` or Ctrl+Left; a fresh question (0 -> N) pops the
        // docked panel itself, notifies, and rings the terminal bell so the
        // dock icon bounces. Later additions only update the count.
        let step_start = std::time::Instant::now();
        let open = runtime_deps.open_questions(app).await?;
        let had_none = app.open_question_count == 0;
        app.open_question_count = open.len();
        if had_none && let Some(first) = open.into_iter().next() {
            let question_text = first
                .questions
                .first()
                .map_or("A new question is waiting", |q| q.question.as_str());
            if let Some(session_id) = app
                .session_manager
                .current()
                .map(|session| session.id.clone())
            {
                runtime_deps
                    .notify_open_question(&session_id, question_text)
                    .await;
            }
            let _ = io::stdout()
                .write_all(b"\x07")
                .and_then(|()| io::stdout().flush());
            app.offer_open_question(may_agent::tui::AskUserState::for_open_question(first));
        } else if app.open_question_count == 0
            && matches!(
                app.mode,
                may_agent::tui::AppMode::AskUser(ref state) if state.request.is_none()
            )
        {
            // Every open question went away (answered or withdrawn elsewhere):
            // the docked panel closes on its own; a blocking prompt is not
            // touched.
            app.mode = may_agent::tui::AppMode::Chat;
        }

        runtime_deps
            .listen_for_agent_messages(app, &agent_listeners, events.sender())
            .await?;
        phases.step("questions+listeners", step_start.elapsed());

        // A saved change to the cache lifetime or compaction model replaces
        // the idle worker with one built from the saved config.
        if worker.is_some()
            && worker_build.is_none()
            && agent_handle.is_none()
            && app.worker_settings != may_agent::agent::AgentLiveSettings::of(&app.config)
        {
            match runtime_deps.begin_reload_worker(app) {
                Ok(build) => {
                    worker_build = Some(WorkerBuild {
                        purpose: WorkerBuildPurpose::Reconfigure,
                        build,
                    });
                }
                Err(error) => {
                    app.worker_settings = may_agent::agent::AgentLiveSettings::of(&app.config);
                    app.chat
                        .push_error(&format!("Failed to apply the saved settings: {error}"));
                }
            }
        }

        // Check for queued messages - start processing if agent is idle. The
        // run lease is taken before any message leaves the queue.
        if agent_handle.is_none()
            && let Some(w) = worker.as_ref()
        {
            let step_start = std::time::Instant::now();
            match runtime_deps.begin_queued_run(app).await? {
                QueuedRunStart::Started(msg) => {
                    busy_elsewhere_notified = false;
                    agent_handle = Some(
                        runtime_deps
                            .start_agent_task(app, w, msg, &mut pending_title_gen)
                            .await?,
                    );
                    assistant_response.clear();
                    continue;
                }
                QueuedRunStart::Busy => {
                    if !busy_elsewhere_notified {
                        busy_elsewhere_notified = true;
                        app.set_status(
                            "This session is running elsewhere; the message stays queued",
                        );
                    }
                }
                QueuedRunStart::Nothing => busy_elsewhere_notified = false,
            }
            phases.step("begin_queued_run", step_start.elapsed());
        }

        // DIAGNOSTIC: Check channel states before entering select
        tracing::info!(
            ask_user_empty = ask_user_queue.is_empty(),
            workspace_empty = workspace_queue.is_empty(),
            has_agent_handle = agent_handle.is_some(),
            "channel states before select"
        );

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
                    match worker_build.as_mut() {
                        Some(pending) => {
                            Some(Event::WorkerBuilt(Box::new(pending.build.wait().await)))
                        }
                        None => futures_lite::future::pending().await,
                    }
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
                            } else if let Some(worker) = worker.as_ref() {
                                // A turn another client started, rendered
                                // while this TUI has none of its own.
                                worker.next_foreign_turn().await.map(Event::ForeignTurn)
                            } else {
                                futures_lite::future::pending().await
                            }
                        },
                        futures_lite::future::or(
                            async {
                                if let Some(ref handle) = context_handle {
                                    // None means the report channel closed —
                                    // surface it as an event so the handle is
                                    // cleared instead of quitting the loop.
                                    Some(Event::Context(handle.recv().await))
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
                                result.map(|event| Event::Terminal(Box::new(event)))
                            },
                        ),
                    ),
                ),
            ),
        )
        .await;

        tracing::info!(
            "select completed, event type: {}",
            match &event {
                Some(Event::AskUser(_)) => "AskUser",
                Some(Event::Workspace(_)) => "Workspace",
                Some(Event::WorkerBuilt(_)) => "WorkerBuilt",
                Some(Event::Agent(_)) => "Agent",
                Some(Event::ForeignTurn(_)) => "ForeignTurn",
                Some(Event::Context(_)) => "Context",
                Some(Event::Terminal(_)) => "Terminal",
                None => "None",
            }
        );

        let Some(event) = event else {
            tracing::warn!("All channels closed, exiting");
            break;
        };
        if matches!(&event, Event::Terminal(e) if matches!(**e, AppEvent::Tick)) {
            needs_draw = app.processing || last_draw.elapsed() >= std::time::Duration::from_secs(1);
        }
        let event_kind = match &event {
            Event::Terminal(e) => e.kind(),
            Event::AskUser(_) => "ask_user",
            Event::Workspace(_) => "workspace",
            Event::WorkerBuilt(_) => "worker_built",
            Event::Agent(_) => "agent",
            Event::ForeignTurn(_) => "foreign_turn",
            Event::Context(_) => "context",
        };

        match event {
            Event::WorkerBuilt(built) => {
                let finished = worker_build
                    .take()
                    .expect("a built worker comes from the pending worker build");
                finish_worker_build(app, worker, finished, *built).await?;
            }

            Event::AskUser(request) => {
                let questions = request.args.request.clone().into_questions();
                tracing::info!(question_count = questions.len(), "User question requested");
                let session_id = app
                    .session_manager
                    .current()
                    .map(|session| session.id.clone())
                    .ok_or_else(|| anyhow::anyhow!("No active session"))?;
                match runtime_deps
                    .register_ask_user_request(&session_id, &questions)
                    .await
                {
                    Ok(request_id) => {
                        let state =
                            may_agent::tui::AskUserState::new(request_id, request, questions);
                        app.show_blocking_prompt(may_agent::tui::AppMode::AskUser(state));
                    }
                    // A dropped request fails the tool call instead of
                    // taking the process down with it.
                    Err(error) => {
                        tracing::warn!(error = %error, "failed to register ask-user request");
                        app.chat
                            .push_error(&format!("Could not open the question: {error}"));
                        drop(request);
                    }
                }
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
                match runtime_deps
                    .register_workspace_request(app, &session_id, &request.args.request)
                    .await
                {
                    Ok(request_id) => {
                        app.show_blocking_prompt(may_agent::tui::AppMode::WorkspaceConfirm(
                            may_agent::tui::state::WorkspaceConfirmState {
                                request_id,
                                path: request.args.request.path.clone(),
                                reason: request.args.request.reason.clone(),
                                request: Some(request),
                            },
                        ));
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "failed to register workspace request");
                        app.chat
                            .push_error(&format!("Could not open the workspace request: {error}"));
                        drop(request);
                    }
                }
            }

            Event::ForeignTurn(turn) => {
                let message = may_agent::tui::queue::PendingMessage {
                    content: turn.content.clone().unwrap_or_default(),
                    hidden_context_xml: None,
                    attachments: Vec::new(),
                    queued_at: jiff::Timestamp::now(),
                    delivery_mode: may_agent::session::PendingUserMessageMode::Queue,
                    display: turn.content.is_none().then(String::new),
                };
                if let Some(content) = turn.content {
                    app.session_manager
                        .record_unpersisted(may_agent::session::ChatMessage::user(content));
                }
                app.start_processing(&message);
                assistant_response.clear();
                agent_handle = Some(turn.handle);
            }

            Event::Agent(tui_event) => {
                match tui_event {
                    TuiEvent::Agent(result) => {
                        // Capture text for title generation
                        if let Ok(TimedEvent {
                            at,
                            event: AgentEvent::Text(ref text),
                        }) = result
                        {
                            assistant_response.push(text, at);
                        }
                        let ended = app.handle_agent_event(result).await;
                        if ended {
                            runtime_deps
                                .finish_agent_task(
                                    app,
                                    &mut agent_handle,
                                    &mut pending_title_gen,
                                    &assistant_response,
                                    RunOutcome::Completed,
                                )
                                .await;
                        }
                    }
                    TuiEvent::ContextReport(_)
                    | TuiEvent::ContextDump(_)
                    | TuiEvent::ContextDumpSaved { .. } => {
                        // Reports arrive on the context channel; the agent
                        // channel only carries them if the channels raced.
                    }
                    TuiEvent::Compacted(result) => match result {
                        Ok(Some(stats)) => {
                            app.set_status(format!(
                                "Compacted {} messages",
                                stats.messages_compacted
                            ));
                        }
                        Ok(None) => {
                            app.set_status("Nothing to compact");
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
                                RunOutcome::Completed,
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
                                RunOutcome::Failed,
                            )
                            .await;
                    }
                }
            }

            Event::Context(event) => {
                context_handle = None;
                match event {
                    Some(TuiEvent::ContextReport(Ok(report))) => {
                        app.chat.push_context_report(report);
                        app.scroll = usize::MAX;
                    }
                    Some(TuiEvent::ContextReport(Err(err))) => {
                        app.chat
                            .push_error(&format!("Context report failed: {err}"));
                    }
                    Some(TuiEvent::ContextDump(Ok(dump))) => {
                        let message_count = dump.messages.len();
                        let text = dump.render();
                        let dir = app.session_manager.current().map(|session| {
                            may_agent::session::SessionPaths::new(session.sandbox_dir.clone())
                                .outputs_dir()
                        });
                        match dir {
                            Some(dir) => {
                                let path = dir.join("context-dump.md");
                                let write = match async_fs::create_dir_all(&dir).await {
                                    Ok(()) => async_fs::write(&path, &text).await,
                                    Err(error) => Err(error),
                                };
                                match write {
                                    Ok(()) => {
                                        app.chat.push_notice(&format!(
                                            "Context dump written to {} — {message_count} messages",
                                            path.display(),
                                        ));
                                        app.scroll = usize::MAX;
                                    }
                                    Err(error) => {
                                        app.chat
                                            .push_error(&format!("Context dump failed: {error}"));
                                    }
                                }
                            }
                            None => app.chat.push_error("No active session for context dump"),
                        }
                    }
                    Some(TuiEvent::ContextDump(Err(err))) => {
                        app.chat.push_error(&format!("Context dump failed: {err}"));
                    }
                    Some(TuiEvent::ContextDumpSaved {
                        path,
                        message_count,
                    }) => {
                        app.chat.push_notice(&format!(
                            "Context dump written to {path} — {message_count} messages"
                        ));
                        app.scroll = usize::MAX;
                    }
                    Some(TuiEvent::Error(err)) => app.chat.push_error(&err),
                    _ => {}
                }
            }

            Event::Terminal(terminal_event) => {
                // Interrupt only from plain chat mode — while a popup or
                // dialog is open, Esc/Ctrl+C belong to that view's own
                // handler (Esc closes it instead of aborting the agent).
                if agent_handle.is_some()
                    && matches!(app.mode, may_agent::tui::AppMode::Chat)
                    && let AppEvent::Key(key_code, modifiers) = &*terminal_event
                {
                    let is_ctrl_c = *key_code == KeyCode::Char('c')
                        && modifiers.contains(KeyModifiers::CONTROL);
                    let is_esc = *key_code == KeyCode::Esc;
                    // Esc while a subagent runs stops just that run; a second
                    // Esc (or Ctrl+C) still aborts the whole turn.
                    if is_esc && app.cancel_all_subagents() {
                        continue;
                    }
                    if is_ctrl_c || is_esc {
                        tracing::info!("Agent interrupted by user");
                        app.last_esc_at = Some(std::time::Instant::now());
                        if let Some(handle) = agent_handle.take() {
                            handle.cancel();
                        }
                        app.status = None;
                        // Clean up loading/streaming state before adding interrupt message
                        app.chat.collapse_thinking();
                        app.chat.end_text();
                        app.chat.push_text("\n\n[Interrupted]");
                        app.chat.end_text();
                        runtime_deps
                            .finish_agent_task(
                                app,
                                &mut agent_handle,
                                &mut pending_title_gen,
                                &assistant_response,
                                RunOutcome::Cancelled,
                            )
                            .await;
                        continue;
                    }
                }

                if let AppEvent::SessionDeleted {
                    deleted: Some(session_id),
                    ..
                } = &*terminal_event
                {
                    runtime_deps.evict_session(session_id);
                }

                // Handle normal terminal events
                let ev_start = std::time::Instant::now();
                match app.handle_event(*terminal_event)? {
                    Some(SendAction::Quit) => break,
                    Some(SendAction::NewSession) => {
                        worker_build = Some(WorkerBuild {
                            purpose: WorkerBuildPurpose::Session,
                            build: runtime_deps.begin_new_session(app, worker).await?,
                        });
                    }
                    Some(SendAction::LoadSession(id)) => {
                        worker_build = Some(WorkerBuild {
                            purpose: WorkerBuildPurpose::Session,
                            build: runtime_deps.begin_load_session(app, worker, &id).await?,
                        });
                    }
                    Some(SendAction::ResumeImport { source, query }) => {
                        match runtime_deps
                            .begin_import_session(app, worker, source, &query)
                            .await
                        {
                            Ok(build) => {
                                worker_build = Some(WorkerBuild {
                                    purpose: WorkerBuildPurpose::Session,
                                    build,
                                });
                            }
                            Err(error) => {
                                app.chat.push_error(&format!("{error}"));
                            }
                        }
                    }
                    Some(SendAction::Rewind {
                        history_index,
                        restore_code,
                    }) => {
                        if agent_handle.is_none() {
                            worker_build = Some(WorkerBuild {
                                purpose: WorkerBuildPurpose::Session,
                                build: runtime_deps
                                    .rewind_conversation(app, worker, history_index, restore_code)
                                    .await?,
                            });
                        } else {
                            app.chat.push_notice(
                                "Wait for the current turn to finish before rewinding",
                            );
                        }
                    }
                    Some(SendAction::Send(msg)) => {
                        if agent_handle.is_none()
                            && let Some(w) = worker.as_ref()
                        {
                            if runtime_deps.begin_run(app).await? {
                                agent_handle = Some(
                                    runtime_deps
                                        .start_agent_task(app, w, msg, &mut pending_title_gen)
                                        .await?,
                                );
                                assistant_response.clear();
                            } else {
                                app.chat.push_notice(
                                    "This session is running elsewhere; the message stays queued",
                                );
                                runtime_deps.enqueue_pending_message(app, msg).await?;
                            }
                        } else if worker.is_none() && worker_build.is_some() {
                            // The session's worker is still being prepared;
                            // it picks the message up from the queue.
                            runtime_deps.enqueue_pending_message(app, msg).await?;
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
                        if let Some(request) = request {
                            let session_id = app
                                .session_manager
                                .current()
                                .map(|session| session.id.clone())
                                .ok_or_else(|| anyhow::anyhow!("No active session"))?;
                            match runtime_deps
                                .resolve_ask_user_response(&session_id, &request_id, &answers)
                                .await?
                            {
                                ResponseCommit::Committed => {
                                    request.respond(answers).unwrap_or_else(|_| {
                                        panic!(
                                            "{}",
                                            may_agent::tui::app::ERR_TOOL_RESPONSE_SEND.trim()
                                        )
                                    });
                                }
                                ResponseCommit::Lost(committed) => {
                                    request.respond(committed).unwrap_or_else(|_| {
                                        panic!(
                                            "{}",
                                            may_agent::tui::app::ERR_TOOL_RESPONSE_SEND.trim()
                                        )
                                    });
                                    app.chat.push_notice(
                                        "That prompt was already answered elsewhere; the run continues with that answer, not yours",
                                    );
                                }
                                ResponseCommit::Withdrawn => app.chat.push_notice(
                                    "That prompt was withdrawn; your answer was not sent",
                                ),
                            }
                        } else if !runtime_deps
                            .answer_open_question(&request_id, &answers)
                            .await?
                        {
                            app.chat.push_notice(
                                "That question is no longer open; your answer was not sent",
                            );
                        }
                    }
                    Some(SendAction::OpenQuestions) => {
                        let open = runtime_deps.open_questions(app).await?;
                        if open.is_empty() {
                            app.chat.push_notice("No open questions");
                        } else {
                            let index = app.question_cursor % open.len();
                            app.question_cursor = index + 1;
                            let request = open
                                .into_iter()
                                .nth(index)
                                .expect("the index is within the open questions");
                            app.mode = may_agent::tui::AppMode::AskUser(
                                may_agent::tui::AskUserState::for_open_question(request),
                            );
                        }
                    }
                    Some(SendAction::StartCopilotAuth) => {
                        run_copilot_auth(app, events, terminal).await?;
                    }
                    Some(SendAction::StartChatgptAuth) => {
                        run_chatgpt_auth(app, events, terminal).await?;
                    }
                    Some(SendAction::SwitchModel(model)) => {
                        match runtime_deps.begin_switch_model(app, &model) {
                            Ok(build) => {
                                worker_build = Some(WorkerBuild {
                                    purpose: WorkerBuildPurpose::SwitchModel,
                                    build,
                                });
                            }
                            Err(e) => {
                                app.chat.push_error(&format!("Failed to switch model: {e}"));
                            }
                        }
                    }
                    Some(SendAction::OpenChatgptAuthUrl { url, code }) => {
                        if !code.is_empty()
                            && let Ok(mut clipboard) = arboard::Clipboard::new()
                            && let Err(e) = clipboard.set_text(&code)
                        {
                            tracing::warn!("Failed to copy code to clipboard: {}", e);
                        }
                        if let Err(e) = open::that(&url) {
                            tracing::warn!("Failed to open URL in browser: {}", e);
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
                    Some(SendAction::BugReport) => match write_bug_snapshot(app, &log_ring) {
                        Ok(path) => app
                            .chat
                            .push_notice(&format!("Bug snapshot saved to {}", path.display())),
                        Err(error) => app
                            .chat
                            .push_error(&format!("Failed to save bug snapshot: {error}")),
                    },
                    Some(SendAction::Compact(focus)) => {
                        if agent_handle.is_none() {
                            if let Some(w) = worker.as_ref() {
                                app.processing = true;
                                app.set_status("Compacting conversation...");
                                agent_handle = Some(w.compact(focus));
                            }
                        } else {
                            app.set_status("Agent is busy");
                        }
                    }
                    Some(SendAction::ContextReport) => {
                        if let Some(w) = worker.as_ref() {
                            if context_handle.is_none() {
                                context_handle = Some(w.context_report());
                            }
                        } else {
                            app.chat
                                .push_notice("Agent is still starting — try again in a moment");
                        }
                    }
                    Some(SendAction::ContextDump) => {
                        if let Some(w) = worker.as_ref() {
                            if context_handle.is_none() {
                                context_handle = Some(w.context_dump());
                            }
                        } else {
                            app.chat
                                .push_notice("Agent is still starting — try again in a moment");
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
                                app.set_status(format!("Added MCP server: {name}"));
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
                                app.set_status(format!("Removed MCP server: {name}"));
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
                                app.set_status(format!("Reconnected to MCP server: {name}"));
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
                            app.set_status(format!("Added SSH server: {name}"));
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
                                app.set_status(format!("Removed SSH server: {name}"));
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
                        match runtime_deps
                            .resolve_workspace_response(&session_id, &request_id, approved)
                            .await?
                        {
                            ResponseCommit::Committed => {
                                if let Some(request) = request {
                                    request.respond(approved).unwrap_or_else(|_| {
                                        panic!(
                                            "{}",
                                            may_agent::tui::app::ERR_TOOL_RESPONSE_SEND.trim()
                                        )
                                    });
                                }
                            }
                            ResponseCommit::Lost(committed) => {
                                if let Some(request) = request {
                                    request.respond(committed).unwrap_or_else(|_| {
                                        panic!(
                                            "{}",
                                            may_agent::tui::app::ERR_TOOL_RESPONSE_SEND.trim()
                                        )
                                    });
                                }
                                app.chat.push_notice(
                                    "That request was already answered elsewhere; the run continues with that decision, not yours",
                                );
                            }
                            ResponseCommit::Withdrawn => app.chat.push_notice(
                                "That request was withdrawn; your decision was not sent",
                            ),
                        }
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
                        app.set_status(format!("Installing from {repo}..."));
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
                        app.set_status(format!("Deleting {dir_name}..."));
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
                    Some(SendAction::RunBash { call_id, command }) => {
                        let tx = events.sender();
                        let secrets = app.secrets.clone();
                        let cwd = app.cwd.clone();
                        spawn(async move {
                            let mut outcome = blocking::unblock(move || {
                                run_bash_command(&command, &cwd, &secrets)
                            })
                            .await;
                            outcome.call_id = call_id;
                            let _ = tx.send(AppEvent::BashFinished(Box::new(outcome))).await;
                        })
                        .detach();
                    }
                    Some(SendAction::StoreSecret { key, value }) => {
                        let tx = events.sender();
                        let may_dir = app.session_manager.may_dir().to_path_buf();
                        let sandbox_dir = app
                            .session_manager
                            .current()
                            .map(|session| session.sandbox_dir.clone());
                        let config = app.config.clone();
                        spawn(async move {
                            let store_dir = may_dir.clone();
                            let store_key = key.clone();
                            let result = blocking::unblock(move || {
                                may_agent::secrets::SecretStore::new(&store_dir)
                                    .set(&store_key, &value)
                                    .map_err(|error| error.to_string())
                            })
                            .await;
                            // Refresh the live session's shell env file so the
                            // agent picks the secret up on its next command.
                            if result.is_ok()
                                && let Some(dir) = sandbox_dir
                            {
                                let secrets = may_agent::secrets::Secrets::load(
                                    &may_agent::secrets::SecretStore::new(&may_dir),
                                    &config,
                                );
                                if let Err(error) = may_agent::secrets::write_env_file(
                                    &dir.join(may_agent::secrets::SECRETS_ENV_FILE),
                                    &secrets,
                                )
                                .await
                                {
                                    tracing::warn!(%error, "failed to refresh secrets env file");
                                }
                            }
                            let _ = tx.send(AppEvent::SecretStored { key, result }).await;
                        })
                        .detach();
                    }
                    None => {}
                }
                phases.event = ev_start.elapsed();
            }
        }

        // Report a stall whenever one busy iteration exceeded the jank
        // threshold; the log carries the render breakdown for repro.
        may_agent::tui::perf::report(
            &phases,
            &app.frame_stats,
            app.scroll,
            app.chat_layout.plain_len(),
            events.backlog(),
            event_kind,
        );
    }

    Ok(())
}

/// Cap a captured output stream at `MAX` bytes on a char boundary.
fn cap_stream(bytes: &[u8]) -> String {
    const MAX: usize = 64 * 1024;
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX {
        return text.into_owned();
    }
    let end = text
        .char_indices()
        .nth(MAX)
        .map_or(text.len(), |(index, _)| index);
    format!("{}\n… (output truncated)", &text[..end])
}

/// Run a bash-mode (`!`) command in may's environment: the launch cwd plus
/// keychain secrets injected as environment variables. Output is masked
/// before it returns to the TUI or the transcript.
fn run_bash_command(
    command: &str,
    cwd: &std::path::Path,
    secrets: &may_agent::secrets::Secrets,
) -> may_agent::tui::event::BashOutcome {
    let (_, command_secret) = secrets.mask(command);
    let spawn = || {
        std::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .envs(secrets.env_pairs())
            .output()
    };
    match spawn() {
        Err(error) => may_agent::tui::event::BashOutcome {
            call_id: String::new(),
            command: command.to_string(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: -1,
            had_secret: command_secret,
            error: Some(format!("failed to run bash: {error}")),
        },
        Ok(output) => {
            let (stdout, out_secret) = secrets.mask(&cap_stream(&output.stdout));
            let (stderr, err_secret) = secrets.mask(&cap_stream(&output.stderr));
            // `had_secret` tracks output only — a command-side hit already
            // warned at submit time.
            may_agent::tui::event::BashOutcome {
                call_id: String::new(),
                command: command.to_string(),
                stdout,
                stderr,
                exit_code: output.status.code().unwrap_or(-1),
                had_secret: out_secret || err_secret,
                error: None,
            }
        }
    }
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
            wait_for_auth_dismiss(app, events, terminal, may_agent::tui::AppMode::CopilotAuth)
                .await?;
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
                    wait_for_auth_dismiss(
                        app,
                        events,
                        terminal,
                        may_agent::tui::AppMode::CopilotAuth,
                    )
                    .await?;
                    return Ok(());
                }
                if let Err(e) = app.config.ensure_authenticated_provider(
                    "copilot",
                    may_agent::config::ProviderType::Copilot,
                ) {
                    app.copilot_auth_failed(e.to_string());
                    wait_for_auth_dismiss(
                        app,
                        events,
                        terminal,
                        may_agent::tui::AppMode::CopilotAuth,
                    )
                    .await?;
                    return Ok(());
                }
                app.config_state.load_providers(&app.config);
                if let Err(e) = app.config.clone().save().await {
                    tracing::error!("Failed to save config: {}", e);
                }
                app.copilot_auth_success();
                wait_for_auth_dismiss(app, events, terminal, may_agent::tui::AppMode::CopilotAuth)
                    .await?;
                return Ok(());
            }
            Err(aither_copilot::CopilotError::AuthorizationPending) => {
                terminal.draw(|frame| ui::render(frame, app))?;
            }
            Err(e) => {
                app.copilot_auth_failed(e.to_string());
                wait_for_auth_dismiss(app, events, terminal, may_agent::tui::AppMode::CopilotAuth)
                    .await?;
                return Ok(());
            }
        }
    }
}

/// Run the `ChatGPT` subscription OAuth flow.
///
/// Uses the PKCE browser loopback on localhost:1455. Over SSH (where the
/// redirect cannot reach this machine) or when the listener fails to bind,
/// falls back to `OpenAI`'s device-authorization flow.
async fn run_chatgpt_auth<B: Backend>(
    app: &mut App,
    events: &mut EventHandler,
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    use aither_codex::auth;

    app.start_chatgpt_auth();
    terminal.draw(|frame| ui::render(frame, app))?;

    let over_ssh =
        std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    if over_ssh {
        return run_chatgpt_device_auth(app, events, terminal).await;
    }

    let pkce = auth::Pkce::generate();
    let url = pkce.authorize_url();
    app.set_chatgpt_auth_url(url.clone());
    terminal.draw(|frame| ui::render(frame, app))?;
    if let Err(e) = open::that(&url) {
        tracing::warn!("Failed to open browser: {e}");
    }

    let listener = auth::listen_for_callback(pkce.state());
    tokio::pin!(listener);

    let code = loop {
        tokio::select! {
            result = &mut listener => match result {
                Ok(code) => break code,
                Err(e) => {
                    tracing::warn!(
                        "OAuth loopback listener failed, falling back to device flow: {e}"
                    );
                    app.start_chatgpt_auth();
                    return run_chatgpt_device_auth(app, events, terminal).await;
                }
            },
            event = events.next() => match event {
                Some(event) => {
                    if let Some(action) = app.handle_event(event)? {
                        match action {
                            SendAction::Quit => return Ok(()),
                            SendAction::OpenChatgptAuthUrl { url, .. } => {
                                if let Err(e) = open::that(&url) {
                                    tracing::warn!("Failed to open URL in browser: {e}");
                                }
                            }
                            _ => {}
                        }
                    }
                    if !matches!(app.mode, may_agent::tui::AppMode::ChatgptAuth) {
                        return Ok(());
                    }
                    terminal.draw(|frame| ui::render(frame, app))?;
                }
                None => return Ok(()),
            }
        }
    };

    let credentials = match auth::exchange_code(&code, pkce.verifier()).await {
        Ok(credentials) => credentials,
        Err(e) => {
            app.chatgpt_auth_failed(e.to_string());
            wait_for_auth_dismiss(app, events, terminal, may_agent::tui::AppMode::ChatgptAuth)
                .await?;
            return Ok(());
        }
    };
    finish_chatgpt_auth(app, events, terminal, credentials).await
}

/// Device-authorization variant of the `ChatGPT` OAuth flow.
async fn run_chatgpt_device_auth<B: Backend>(
    app: &mut App,
    events: &mut EventHandler,
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    use aither_codex::auth;

    let device = match auth::request_device_code().await {
        Ok(device) => device,
        Err(e) => {
            app.chatgpt_auth_failed(e.to_string());
            wait_for_auth_dismiss(app, events, terminal, may_agent::tui::AppMode::ChatgptAuth)
                .await?;
            return Ok(());
        }
    };
    app.set_chatgpt_device_code(
        device.user_code.clone(),
        aither_codex::constant::DEVICE_VERIFICATION_URL.to_string(),
    );
    terminal.draw(|frame| ui::render(frame, app))?;

    let poll_interval = std::time::Duration::from_secs(
        device
            .interval
            .as_deref()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(5)
            .max(3),
    );

    let (code, verifier) = loop {
        let deadline = tokio::time::Instant::now() + poll_interval;

        loop {
            let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
            if timeout.is_zero() {
                break;
            }
            let event_timeout = timeout.min(std::time::Duration::from_millis(100));
            match tokio::time::timeout(event_timeout, events.next()).await {
                Ok(Some(event)) => {
                    if let Some(action) = app.handle_event(event)?
                        && matches!(action, SendAction::Quit)
                    {
                        return Ok(());
                    }
                    if !matches!(app.mode, may_agent::tui::AppMode::ChatgptAuth) {
                        return Ok(());
                    }
                    terminal.draw(|frame| ui::render(frame, app))?;
                }
                Ok(None) => return Ok(()),
                Err(_) => {}
            }
        }

        match auth::poll_device_code(&device.device_auth_id, &device.user_code).await {
            Ok(result) => break result,
            Err(aither_codex::CodexError::AuthorizationPending) => {
                terminal.draw(|frame| ui::render(frame, app))?;
            }
            Err(e) => {
                app.chatgpt_auth_failed(e.to_string());
                wait_for_auth_dismiss(app, events, terminal, may_agent::tui::AppMode::ChatgptAuth)
                    .await?;
                return Ok(());
            }
        }
    };

    let credentials = match auth::exchange_code(&code, &verifier).await {
        Ok(credentials) => credentials,
        Err(e) => {
            app.chatgpt_auth_failed(e.to_string());
            wait_for_auth_dismiss(app, events, terminal, may_agent::tui::AppMode::ChatgptAuth)
                .await?;
            return Ok(());
        }
    };
    finish_chatgpt_auth(app, events, terminal, credentials).await
}

/// Persist credentials and register the `chatgpt` provider after OAuth success.
async fn finish_chatgpt_auth<B: Backend>(
    app: &mut App,
    events: &mut EventHandler,
    terminal: &mut Terminal<B>,
    credentials: aither_codex::auth::CodexCredentials,
) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    if let Err(e) = may_agent::chatgpt::save_credentials(&credentials).await {
        app.chatgpt_auth_failed(format!("Failed to save credentials: {e}"));
        wait_for_auth_dismiss(app, events, terminal, may_agent::tui::AppMode::ChatgptAuth).await?;
        return Ok(());
    }
    if let Err(e) = app
        .config
        .ensure_authenticated_provider("chatgpt", may_agent::config::ProviderType::Chatgpt)
    {
        app.chatgpt_auth_failed(e.to_string());
        wait_for_auth_dismiss(app, events, terminal, may_agent::tui::AppMode::ChatgptAuth).await?;
        return Ok(());
    }
    app.config_state.load_providers(&app.config);
    if let Err(e) = app.config.clone().save().await {
        tracing::error!("Failed to save config: {}", e);
    }
    app.chatgpt_auth_success();
    wait_for_auth_dismiss(app, events, terminal, may_agent::tui::AppMode::ChatgptAuth).await?;
    Ok(())
}

/// Wait for the user to dismiss the auth result screen.
async fn wait_for_auth_dismiss<B: Backend>(
    app: &mut App,
    events: &mut EventHandler,
    terminal: &mut Terminal<B>,
    auth_mode: may_agent::tui::AppMode,
) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    terminal.draw(|frame| ui::render(frame, app))?;

    loop {
        if let Some(event) = events.next().await {
            app.handle_event(event)?;
            if mem::discriminant(&app.mode) != mem::discriminant(&auth_mode) {
                break;
            }
        }
        terminal.draw(|frame| ui::render(frame, app))?;
    }

    Ok(())
}

/// `may ui`: make sure the daemon runs, then open its web UI.
async fn open_web_ui() -> anyhow::Result<()> {
    if !cfg!(feature = "ui") {
        anyhow::bail!(
            "`may ui` requires the `ui` cargo feature; use `may rest` for the REST API server"
        );
    }
    let paths = may_agent::daemon::DaemonPaths::user()?;
    may_agent::daemon::lifecycle::ensure_running(&paths, |line| eprintln!("{line}")).await?;
    let config = may_agent::config::MayConfig::load().await?;
    let port = config
        .daemon
        .port
        .unwrap_or(may_agent::ui_server::DEFAULT_PORT);
    let url = format!("http://127.0.0.1:{port}");
    eprintln!("May web UI: {url}");
    if let Err(error) = open::that(&url) {
        eprintln!("Open {url} in a browser ({error}).");
    }
    Ok(())
}

/// `may daemon [install|uninstall|status|stop]`.
async fn run_daemon_command(command: cli::DaemonCommand) -> anyhow::Result<()> {
    use may_agent::daemon::{DaemonOptions, DaemonPaths, lifecycle};

    let paths = DaemonPaths::user()?;
    match command {
        cli::DaemonCommand::Run => may_agent::daemon::run_daemon(DaemonOptions::new()).await,
        cli::DaemonCommand::Install => {
            let path = lifecycle::install(&paths)?;
            eprintln!(
                "Installed the May daemon as a login service: {}",
                path.display()
            );
            Ok(())
        }
        cli::DaemonCommand::Uninstall => {
            match lifecycle::uninstall()? {
                Some(path) => eprintln!("Removed the May daemon login service: {}", path.display()),
                None => eprintln!("The May daemon is not installed as a login service."),
            }
            Ok(())
        }
        cli::DaemonCommand::Stop => {
            if lifecycle::stop(&paths).await? {
                eprintln!("Stopped the May daemon.");
            } else {
                eprintln!("The May daemon is not running.");
            }
            Ok(())
        }
        cli::DaemonCommand::Status => {
            let client = may_agent::client::DaemonClient::new(paths.socket());
            match client.health().await {
                Ok(health) => {
                    println!("May daemon running (pid {}).", health.pid);
                    println!(
                        "Login service: {}.",
                        if lifecycle::installed() {
                            "installed"
                        } else {
                            "not installed"
                        }
                    );
                    println!(
                        "Search index: {} change(s) pending{}.",
                        health.search_index.pending,
                        health
                            .search_index
                            .last_error
                            .map(|error| format!("; last error: {error}"))
                            .unwrap_or_default()
                    );
                }
                Err(_) => println!("May daemon not running."),
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Terminal-bound tool chains poll deep enough that the pthread default
    /// (512 KiB on macOS) aborts them; workers must carry the configured stack.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn tokio_workers_carry_the_configured_stack() {
        let measured = tokio_runtime().expect("runtime builds").block_on(async {
            tokio::spawn(async { worker_stack_bytes() })
                .await
                .expect("worker task joins")
        });
        assert!(
            measured >= WORKER_STACK_BYTES,
            "tokio worker stack {measured} below configured {WORKER_STACK_BYTES}"
        );
    }

    #[cfg(target_os = "macos")]
    fn worker_stack_bytes() -> usize {
        // SAFETY: reads this thread's own stack size; always safe to call.
        unsafe { libc::pthread_get_stacksize_np(libc::pthread_self()) }
    }

    #[cfg(target_os = "linux")]
    fn worker_stack_bytes() -> usize {
        // SAFETY: attr is written by pthread_getattr_np, read, then destroyed.
        unsafe {
            let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
            assert_eq!(
                libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()),
                0
            );
            let attr = attr.assume_init();
            let mut size = 0usize;
            assert_eq!(
                libc::pthread_attr_getstacksize(&raw const attr, &raw mut size),
                0
            );
            let mut attr = attr;
            libc::pthread_attr_destroy(&raw mut attr);
            size
        }
    }
}
