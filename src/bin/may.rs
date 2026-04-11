//! May TUI - Terminal interface for May agent.
//!
//! Architecture:
//! - Main thread: Handles UI rendering and terminal events only
//! - Background tasks: Run agent processing, send events via channels
//! - Communication: All through `async_channel` (runtime-agnostic)

use std::io;
use std::io::Write;
use std::panic;
use std::sync::Arc;

use aither_agent::AgentEvent;
use aither_core::llm::Tool;
use aither_core::llm::model::ReasoningEffort;
use aither_sandbox::{
    ContainerShellRuntime, OutputFormat, ShellRuntimeAvailability, SshServer, TerminalArgs,
    TerminalExecutionMode, TerminalResult, TerminalTool, terminal_tool_factory_channel,
};
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
use futures_lite::StreamExt;
use jiff::Timestamp;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::backend::CrosstermBackend;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use may_agent::agent::{AgentHandle, AgentWorker, TuiEvent};
use may_agent::ask_user::{self, AskUserQueue};
use may_agent::browser_service::{
    BrowserReadDetail, BrowserReadFormat, BrowserReadOptions, BrowserScreenshotMode,
    BrowserServiceRequest, BrowserServiceResponse, BrowserServiceStatus,
};
use may_agent::session::{SessionPaths, initialize_session_persistence};
use may_agent::tui::runtime::{LocalSessionRuntime, RuntimeDeps, TitleGenContext};
use may_agent::tui::{App, AppEvent, EventHandler, PendingMessage, SendAction, ui};
use may_agent::workspace_request::{self, WorkspaceRequestQueue};
use may_agent::{
    ChatDb, MayConfig, MayMcpServerConfig, McpManager, ProfileEnvironment, ProfileInfo, SessionId,
    SessionStore, SharedTerminalCommandConfig, SkillManager, TaskRuntimeKind,
    TerminalPermissionHandler, register_shared_terminal_commands,
    resolve_agent_environment_for_session, resolve_provider_runtime,
};

const PANIC_MESSAGE: &str = include_str!("../texts/panic_message.txt");

fn ensure_rustls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[derive(Debug, PartialEq, Eq)]
enum CliMode {
    Tui,
    Ui(Vec<String>),
    Exec(ExecArgs),
    Browser(BrowserCliArgs),
    OneShot(OneShotArgs),
    Help(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OneShotArgs {
    prompt: String,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExecArgs {
    session_id: String,
    script: String,
    mode: Option<TerminalExecutionMode>,
    ssh_server_id: Option<String>,
    timeout: u64,
    raw: bool,
    max_lines: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserCliArgs {
    command: BrowserCliCommand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BrowserCliCommand {
    Status,
    Start,
    Stop,
    Targets,
    Contexts,
    Endpoint,
    Read {
        url: String,
        detail: BrowserReadDetail,
        screenshot: BrowserScreenshotMode,
        format: BrowserReadFormat,
    },
    Provision,
    Kill,
    Service {
        socket_path: std::path::PathBuf,
    },
}

impl OneShotArgs {
    fn help_text() -> &'static str {
        "Usage: may [--model <id>] [--reasoning-effort <low|medium|high>] <prompt>\n       may ui [args...]\n\nOne-shot mode streams assistant text to stdout and process events to stderr."
    }
}

struct UiArgs;

impl UiArgs {
    fn help_text() -> &'static str {
        "Usage: may ui [--host <addr>] [--port <port>] [--open]\n\nRun the May web UI and API server."
    }
}

impl ExecArgs {
    fn help_text() -> &'static str {
        "Usage: may exec --session <id> [--ssh-server <id>] [--timeout <sec>] [--max-lines <n>] [--raw] [--] <command...>\n\nExec runs a terminal command in the session home environment. Remote profile sessions execute over SSH automatically, and container profile sessions execute inside the configured container environment."
    }
}

impl BrowserCliArgs {
    fn help_text() -> &'static str {
        "Usage: may browser <status|open|close|targets|contexts|cdp-endpoint|read <url> [--detail <brief|standard|full>] [--screenshot <off|viewport|full-page>] [--format json]|provision|kill>\n\nInspect or maintain the May-managed browser runtime used for browser automation."
    }
}

fn parse_cli_mode(args: &[String]) -> CliMode {
    match args.get(1).map(String::as_str) {
        Some("--help" | "-h") => CliMode::Help(OneShotArgs::help_text()),
        Some("ui") => match args.get(2).map(String::as_str) {
            Some("--help" | "-h") => CliMode::Help(UiArgs::help_text()),
            _ => CliMode::Ui(args[2..].to_vec()),
        },
        Some("browser") => match args.get(2).map(String::as_str) {
            Some("--help" | "-h") => CliMode::Help(BrowserCliArgs::help_text()),
            _ => CliMode::Browser(parse_browser_args(&args[2..])),
        },
        Some("exec") => match args.get(2).map(String::as_str) {
            Some("--help" | "-h") => CliMode::Help(ExecArgs::help_text()),
            _ => CliMode::Exec(parse_exec_args(&args[2..])),
        },
        Some(_) => CliMode::OneShot(parse_oneshot_args(&args[1..])),
        None => CliMode::Tui,
    }
}

fn parse_browser_args(args: &[String]) -> BrowserCliArgs {
    let command = match args.first().map(String::as_str).unwrap_or("status") {
        "--help" | "-h" => panic!("{}", BrowserCliArgs::help_text()),
        "status" => BrowserCliCommand::Status,
        "open" => BrowserCliCommand::Start,
        "close" => BrowserCliCommand::Stop,
        "targets" => BrowserCliCommand::Targets,
        "contexts" => BrowserCliCommand::Contexts,
        "cdp-endpoint" => BrowserCliCommand::Endpoint,
        "provision" => BrowserCliCommand::Provision,
        "kill" => BrowserCliCommand::Kill,
        "read" => {
            let url = args.get(1).cloned().unwrap_or_else(|| {
                panic!("may browser read requires <url>");
            });
            let mut detail = BrowserReadDetail::default();
            let mut screenshot = BrowserScreenshotMode::default();
            let mut format = BrowserReadFormat::Json;
            let mut index = 2;
            while index < args.len() {
                match args[index].as_str() {
                    "--detail" => {
                        let value = args.get(index + 1).unwrap_or_else(|| {
                            panic!("may browser read requires --detail <brief|standard|full>")
                        });
                        detail = match value.as_str() {
                            "brief" => BrowserReadDetail::Brief,
                            "standard" => BrowserReadDetail::Standard,
                            "full" => BrowserReadDetail::Full,
                            other => panic!(
                                "invalid may browser read detail '{}'; expected brief, standard, or full",
                                other
                            ),
                        };
                        index += 2;
                    }
                    "--screenshot" => {
                        let value = args.get(index + 1).unwrap_or_else(|| {
                            panic!(
                                "may browser read requires --screenshot <off|viewport|full-page>"
                            )
                        });
                        screenshot = match value.as_str() {
                            "off" => BrowserScreenshotMode::Off,
                            "viewport" => BrowserScreenshotMode::Viewport,
                            "full-page" => BrowserScreenshotMode::FullPage,
                            other => panic!(
                                "invalid may browser read screenshot mode '{}'; expected off, viewport, or full-page",
                                other
                            ),
                        };
                        index += 2;
                    }
                    "--format" => {
                        let value = args
                            .get(index + 1)
                            .unwrap_or_else(|| panic!("may browser read requires --format json"));
                        format = match value.as_str() {
                            "json" => BrowserReadFormat::Json,
                            other => {
                                panic!("invalid may browser read format '{}'; expected json", other)
                            }
                        };
                        index += 2;
                    }
                    other => panic!("unexpected may browser read argument '{other}'"),
                }
            }
            BrowserCliCommand::Read {
                url,
                detail,
                screenshot,
                format,
            }
        }
        "service" => {
            let mut socket_path = None;
            let mut index = 1;
            while index < args.len() {
                match args[index].as_str() {
                    "--socket" => {
                        let value = args.get(index + 1).unwrap_or_else(|| {
                            panic!("may browser service requires --socket <path>")
                        });
                        socket_path = Some(std::path::PathBuf::from(value));
                        index += 2;
                    }
                    other => panic!("unexpected may browser service argument '{other}'"),
                }
            }
            BrowserCliCommand::Service {
                socket_path: socket_path
                    .unwrap_or_else(|| panic!("may browser service requires --socket <path>")),
            }
        }
        other => panic!(
            "invalid browser command '{}'; expected one of: status, open, close, targets, contexts, cdp-endpoint, read, provision, kill",
            other
        ),
    };
    if !matches!(
        command,
        BrowserCliCommand::Service { .. } | BrowserCliCommand::Read { .. }
    ) && args.len() > 1
    {
        panic!("may browser accepts exactly one subcommand");
    }
    BrowserCliArgs { command }
}

fn parse_cli_mode_from_env() -> CliMode {
    let args: Vec<String> = std::env::args().collect();
    parse_cli_mode(&args)
}

fn parse_exec_args(args: &[String]) -> ExecArgs {
    let mut session_id = None;
    let mut script_parts = Vec::new();
    let mut mode = None;
    let mut ssh_server_id = None;
    let mut timeout = 300;
    let mut raw = false;
    let mut max_lines = 200;
    let mut literal_script = false;
    let mut index = 0;

    while index < args.len() {
        let value = &args[index];
        if literal_script {
            script_parts.push(value.clone());
            index += 1;
            continue;
        }

        match value.as_str() {
            "--help" | "-h" => panic!("{}", ExecArgs::help_text()),
            "--" => {
                literal_script = true;
                index += 1;
            }
            "--session" => {
                let next = args
                    .get(index + 1)
                    .unwrap_or_else(|| panic!("--session requires a value"));
                session_id = Some(next.clone());
                index += 2;
            }
            "--mode" => {
                let next = args
                    .get(index + 1)
                    .unwrap_or_else(|| panic!("--mode requires a value"));
                mode = Some(parse_terminal_execution_mode(next));
                index += 2;
            }
            "--ssh-server" => {
                let next = args
                    .get(index + 1)
                    .unwrap_or_else(|| panic!("--ssh-server requires a value"));
                ssh_server_id = Some(next.clone());
                index += 2;
            }
            "--timeout" => {
                let next = args
                    .get(index + 1)
                    .unwrap_or_else(|| panic!("--timeout requires a value"));
                timeout = next
                    .parse::<u64>()
                    .unwrap_or_else(|_| panic!("invalid --timeout '{}'", next));
                index += 2;
            }
            "--max-lines" => {
                let next = args
                    .get(index + 1)
                    .unwrap_or_else(|| panic!("--max-lines requires a value"));
                max_lines = next
                    .parse::<u32>()
                    .unwrap_or_else(|_| panic!("invalid --max-lines '{}'", next));
                index += 2;
            }
            "--raw" => {
                raw = true;
                index += 1;
            }
            other => {
                script_parts.push(other.to_string());
                index += 1;
            }
        }
    }

    let session_id = session_id.unwrap_or_else(|| panic!("--session is required"));
    let script = script_parts.join(" ").trim().to_string();
    if script.is_empty() {
        panic!("exec command must not be empty");
    }

    ExecArgs {
        session_id,
        script,
        mode,
        ssh_server_id,
        timeout,
        raw,
        max_lines,
    }
}

fn parse_oneshot_args(args: &[String]) -> OneShotArgs {
    let mut prompt_parts = Vec::new();
    let mut model = None;
    let mut reasoning_effort = None;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => panic!("{}", OneShotArgs::help_text()),
            "--model" => {
                let value = args
                    .get(index + 1)
                    .unwrap_or_else(|| panic!("--model requires a value"));
                model = Some(value.clone());
                index += 2;
            }
            "--reasoning-effort" => {
                let value = args
                    .get(index + 1)
                    .unwrap_or_else(|| panic!("--reasoning-effort requires a value"));
                reasoning_effort = Some(parse_reasoning_effort(value));
                index += 2;
            }
            value => {
                prompt_parts.push(value.to_string());
                index += 1;
            }
        }
    }

    let prompt = prompt_parts.join(" ").trim().to_string();
    if prompt.is_empty() {
        panic!("oneshot prompt must not be empty");
    }

    OneShotArgs {
        prompt,
        model,
        reasoning_effort,
    }
}

fn parse_reasoning_effort(value: &str) -> ReasoningEffort {
    match value.trim().to_ascii_lowercase().as_str() {
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        _ => panic!(
            "invalid --reasoning-effort '{}'; expected one of: low, medium, high",
            value
        ),
    }
}

fn parse_terminal_execution_mode(value: &str) -> TerminalExecutionMode {
    match value.trim().to_ascii_lowercase().as_str() {
        "default" => TerminalExecutionMode::Default,
        "sandboxed" => TerminalExecutionMode::Sandboxed,
        "unsafe" => TerminalExecutionMode::Unsafe,
        "ssh" => TerminalExecutionMode::Ssh,
        _ => panic!(
            "invalid --mode '{}'; expected one of: default, sandboxed, unsafe, ssh",
            value
        ),
    }
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

#[derive(Debug, thiserror::Error)]
enum BrowserCliError {
    #[error("failed to resolve May home directory")]
    MissingHomeDirectory,
    #[error("failed to load May config: {0}")]
    LoadConfig(may_agent::config::ConfigError),
    #[error("failed to serialize browser output: {0}")]
    Serialize(serde_json::Error),
    #[error("failed to write stdout: {0}")]
    WriteStdout(std::io::Error),
    #[error("failed to connect to managed browser at {endpoint}: {source}")]
    ConnectBrowser {
        endpoint: String,
        source: chromiumoxide::error::CdpError,
    },
    #[error("timed out while connecting to managed browser at {0}")]
    ConnectBrowserTimeout(String),
    #[error("failed to fetch browser targets from {endpoint}: {source}")]
    FetchTargets {
        endpoint: String,
        source: chromiumoxide::error::CdpError,
    },
    #[error("timed out while fetching browser targets from {0}")]
    FetchTargetsTimeout(String),
    #[error("failed to provision managed browser runtime: {0}")]
    ProvisionBrowser(may_agent::container::browser::BrowserError),
    #[error("managed browser is not running at {0}")]
    BrowserNotRunning(String),
    #[error("failed to inspect browser listener on port {port}: {details}")]
    InspectBrowserListener { port: u16, details: String },
    #[error("failed to fetch browser debugger metadata from {endpoint}: {source}")]
    FetchBrowserVersion {
        endpoint: String,
        source: reqwest::Error,
    },
    #[error("browser debugger metadata request to {endpoint} returned HTTP {status}")]
    FetchBrowserVersionStatus { endpoint: String, status: u16 },
    #[error("browser debugger metadata at {endpoint} did not include webSocketDebuggerUrl")]
    MissingBrowserDebuggerWebSocket { endpoint: String },
    #[error("browser service error: {0}")]
    BrowserService(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum BrowserListenerState {
    NotListening,
    Managed,
    ExternalProcess,
    RemoteEndpoint,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserStatusOutput {
    enabled: bool,
    backend: may_agent::config::BrowserBackendKind,
    running: bool,
    cdp_endpoint: Option<String>,
    provisioned_executable: Option<String>,
    target_count: usize,
    listener_state: BrowserListenerState,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserTargetOutput {
    target_id: String,
    target_type: String,
    title: String,
    url: String,
    attached: bool,
    browser_context_id: Option<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserContextOutput {
    browser_context_id: String,
    target_count: usize,
    targets: Vec<BrowserTargetOutput>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserVersionMetadata {
    web_socket_debugger_url: Option<String>,
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

fn render_json_stdout<T: serde::Serialize>(value: &T) -> Result<(), BrowserCliError> {
    let text = serde_json::to_string_pretty(value).map_err(BrowserCliError::Serialize)?;
    let mut stdout = io::stdout();
    stdout
        .write_all(text.as_bytes())
        .map_err(BrowserCliError::WriteStdout)?;
    stdout
        .write_all(b"\n")
        .map_err(BrowserCliError::WriteStdout)?;
    stdout.flush().map_err(BrowserCliError::WriteStdout)
}

async fn run_browser(args: BrowserCliArgs) -> Result<i32, BrowserCliError> {
    let may_dir = MayConfig::may_dir().map_err(|_| BrowserCliError::MissingHomeDirectory)?;
    let config_path = may_dir.join("config.toml");
    let config = MayConfig::load_from(&config_path)
        .await
        .map_err(BrowserCliError::LoadConfig)?;
    let socket_path = browser_service_socket_path(&may_dir);

    match args.command {
        BrowserCliCommand::Service { socket_path } => {
            return run_browser_service(may_dir, config, socket_path)
                .await
                .map_err(BrowserCliError::BrowserService);
        }
        BrowserCliCommand::Endpoint => {
            let status = ensure_browser_service_started(may_dir.as_path(), &config.browser)
                .await
                .map_err(BrowserCliError::BrowserService)?;
            render_json_stdout(&serde_json::json!({ "cdpEndpoint": status.cdp_endpoint }))?;
        }
        BrowserCliCommand::Start => {
            let status = ensure_browser_service_started(may_dir.as_path(), &config.browser)
                .await
                .map_err(BrowserCliError::BrowserService)?;
            render_json_stdout(&serde_json::json!({
                "running": status.running,
                "cdpEndpoint": status.cdp_endpoint,
            }))?;
        }
        BrowserCliCommand::Stop => {
            stop_browser_service(&socket_path, &config.browser, &may_dir)
                .await
                .map_err(BrowserCliError::BrowserService)?;
            render_json_stdout(&serde_json::json!({
                "ok": true,
                "running": false,
                "cdpEndpoint": serde_json::Value::Null,
            }))?;
        }
        BrowserCliCommand::Provision => {
            if !config.browser.requires_managed_runtime() {
                return Err(BrowserCliError::BrowserService(
                    "browser provision is only valid for the managed local browser backend"
                        .to_string(),
                ));
            }
            let path = may_agent::container::browser::provision_managed_browser(&may_dir)
                .await
                .map_err(BrowserCliError::ProvisionBrowser)?;
            render_json_stdout(&serde_json::json!({
                "ok": true,
                "executable": path,
            }))?;
        }
        BrowserCliCommand::Kill => {
            stop_browser_service(&socket_path, &config.browser, &may_dir)
                .await
                .map_err(BrowserCliError::BrowserService)?;
            render_json_stdout(&serde_json::json!({
                "ok": true,
                "cdpEndpoint": serde_json::Value::Null,
            }))?;
        }
        BrowserCliCommand::Status => {
            let daemon_status = browser_service_status(&socket_path)
                .await
                .map_err(BrowserCliError::BrowserService)?;
            let listener_state =
                if config.browser.backend == may_agent::config::BrowserBackendKind::RemoteCdp {
                    BrowserListenerState::RemoteEndpoint
                } else if daemon_status.as_ref().is_some_and(|status| status.running) {
                    BrowserListenerState::Managed
                } else {
                    inspect_browser_listener_state(config.browser.port).await?
                };
            let targets = match daemon_status.as_ref() {
                Some(status) if status.running => Some(
                    fetch_browser_targets(status.cdp_endpoint.as_deref().ok_or_else(|| {
                        BrowserCliError::BrowserService(
                            "running browser service did not expose a CDP endpoint".to_string(),
                        )
                    })?)
                    .await?,
                ),
                _ => None,
            };
            let provisioned_executable = config
                .browser
                .requires_managed_runtime()
                .then(|| may_agent::container::browser::provisioned_browser_executable(&may_dir))
                .flatten()
                .map(|path| path.display().to_string());
            render_json_stdout(&BrowserStatusOutput {
                enabled: config.browser.enabled,
                backend: config.browser.backend,
                running: daemon_status.as_ref().is_some_and(|status| status.running),
                cdp_endpoint: daemon_status.and_then(|status| status.cdp_endpoint),
                provisioned_executable,
                target_count: targets.as_ref().map_or(0, Vec::len),
                listener_state,
            })?;
        }
        BrowserCliCommand::Targets => {
            let daemon_status = browser_service_status(&socket_path)
                .await
                .map_err(BrowserCliError::BrowserService)?;
            let endpoint = daemon_status
                .as_ref()
                .and_then(|status| status.running.then_some(status))
                .and_then(|status| status.cdp_endpoint.as_deref())
                .ok_or_else(|| BrowserCliError::BrowserNotRunning("managed browser".to_string()))?;
            let targets = require_browser_targets(endpoint).await?;
            render_json_stdout(&targets)?;
        }
        BrowserCliCommand::Contexts => {
            let daemon_status = browser_service_status(&socket_path)
                .await
                .map_err(BrowserCliError::BrowserService)?;
            let endpoint = daemon_status
                .as_ref()
                .and_then(|status| status.running.then_some(status))
                .and_then(|status| status.cdp_endpoint.as_deref())
                .ok_or_else(|| BrowserCliError::BrowserNotRunning("managed browser".to_string()))?;
            let targets = require_browser_targets(endpoint).await?;
            let mut grouped = std::collections::BTreeMap::<String, Vec<BrowserTargetOutput>>::new();
            for target in targets {
                let key = target
                    .browser_context_id
                    .clone()
                    .unwrap_or_else(|| "default".to_string());
                grouped.entry(key).or_default().push(target);
            }
            let contexts = grouped
                .into_iter()
                .map(|(browser_context_id, targets)| BrowserContextOutput {
                    target_count: targets.len(),
                    browser_context_id,
                    targets,
                })
                .collect::<Vec<_>>();
            render_json_stdout(&contexts)?;
        }
        BrowserCliCommand::Read {
            url,
            detail,
            screenshot,
            format: _,
        } => {
            let status = ensure_browser_service_started(may_dir.as_path(), &config.browser)
                .await
                .map_err(BrowserCliError::BrowserService)?;
            let response = send_browser_service_request(
                &socket_path,
                BrowserServiceRequest::Read {
                    url,
                    options: BrowserReadOptions { detail, screenshot },
                },
            )
            .await
            .map_err(BrowserCliError::BrowserService)?;
            match response {
                Some(BrowserServiceResponse::ReadOk { payload }) => {
                    render_json_stdout(&serde_json::json!({
                        "running": status.running,
                        "cdpEndpoint": status.cdp_endpoint,
                        "page": payload,
                    }))?;
                }
                Some(BrowserServiceResponse::Error { message }) => {
                    return Err(BrowserCliError::BrowserService(message));
                }
                Some(BrowserServiceResponse::Ok { .. }) => {
                    return Err(BrowserCliError::BrowserService(
                        "browser service returned status payload for read request".to_string(),
                    ));
                }
                Some(BrowserServiceResponse::Stopped) | None => {
                    return Err(BrowserCliError::BrowserService(
                        "browser service stopped while reading page".to_string(),
                    ));
                }
            }
        }
    }

    Ok(0)
}

fn browser_service_socket_path(may_dir: &std::path::Path) -> std::path::PathBuf {
    may_agent::session::MayHomePaths::new(may_dir.to_path_buf())
        .browser_dir()
        .join("runtime.sock")
}

async fn run_browser_service(
    may_dir: std::path::PathBuf,
    config: MayConfig,
    socket_path: std::path::PathBuf,
) -> Result<i32, String> {
    if !config.browser.enabled {
        return Err("browser automation is disabled in settings".to_string());
    }

    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| error.to_string())?;
    }
    match tokio::fs::remove_file(&socket_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }

    let listener =
        tokio::net::UnixListener::bind(&socket_path).map_err(|error| error.to_string())?;
    let browser_manager = Arc::new(
        may_agent::container::browser::BrowserManager::new(config.browser.clone(), may_dir.clone())
            .map_err(|error| error.to_string())?,
    );

    loop {
        let (mut stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
        let mut request_bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut request_bytes)
            .await
            .map_err(|error| error.to_string())?;
        let request = serde_json::from_slice::<BrowserServiceRequest>(&request_bytes)
            .map_err(|error| error.to_string())?;
        let (response, should_exit) = match request {
            BrowserServiceRequest::Status => (
                BrowserServiceResponse::Ok {
                    payload: BrowserServiceStatus {
                        running: browser_manager.is_running().await,
                        cdp_endpoint: browser_manager.cdp_endpoint(),
                    },
                },
                false,
            ),
            BrowserServiceRequest::EnsureStarted => match async {
                if !browser_manager.is_running().await {
                    may_agent::container::browser::prepare_browser_runtime(
                        &config.browser,
                        &may_dir,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    browser_manager
                        .start()
                        .await
                        .map_err(|error| error.to_string())?;
                }
                Ok::<BrowserServiceResponse, String>(BrowserServiceResponse::Ok {
                    payload: BrowserServiceStatus {
                        running: true,
                        cdp_endpoint: browser_manager.cdp_endpoint(),
                    },
                })
            }
            .await
            {
                Ok(response) => (response, false),
                Err(message) => (BrowserServiceResponse::Error { message }, false),
            },
            BrowserServiceRequest::Stop => match async {
                if browser_manager.is_running().await {
                    browser_manager
                        .stop()
                        .await
                        .map_err(|error| error.to_string())?;
                }
                Ok::<BrowserServiceResponse, String>(BrowserServiceResponse::Stopped)
            }
            .await
            {
                Ok(response) => (response, true),
                Err(message) => (BrowserServiceResponse::Error { message }, false),
            },
            BrowserServiceRequest::Read { url, options } => match async {
                if !browser_manager.is_running().await {
                    may_agent::container::browser::prepare_browser_runtime(
                        &config.browser,
                        &may_dir,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    browser_manager
                        .start()
                        .await
                        .map_err(|error| error.to_string())?;
                }
                let page = browser_manager
                    .read_page(url, options)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok::<BrowserServiceResponse, String>(BrowserServiceResponse::ReadOk {
                    payload: page,
                })
            }
            .await
            {
                Ok(response) => (response, false),
                Err(message) => (BrowserServiceResponse::Error { message }, false),
            },
        };
        let response_bytes = serde_json::to_vec(&response).map_err(|error| error.to_string())?;
        tokio::io::AsyncWriteExt::write_all(&mut stream, &response_bytes)
            .await
            .map_err(|error| error.to_string())?;
        if should_exit {
            break;
        }
    }

    match tokio::fs::remove_file(&socket_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    Ok(0)
}

async fn browser_service_status(
    socket_path: &std::path::Path,
) -> Result<Option<BrowserServiceStatus>, String> {
    let Some(response) =
        send_browser_service_request(socket_path, BrowserServiceRequest::Status).await?
    else {
        return Ok(None);
    };
    match response {
        BrowserServiceResponse::Ok { payload } => Ok(Some(payload)),
        BrowserServiceResponse::Stopped => Ok(None),
        BrowserServiceResponse::Error { message } => Err(message),
        BrowserServiceResponse::ReadOk { .. } => {
            Err("browser service returned page payload for status request".to_string())
        }
    }
}

async fn ensure_browser_service_started(
    may_dir: &std::path::Path,
    config: &may_agent::config::BrowserConfig,
) -> Result<BrowserServiceStatus, String> {
    if !config.enabled {
        return Err(
            "browser command is unavailable because browser automation is disabled in settings"
                .to_string(),
        );
    }

    let socket_path = browser_service_socket_path(may_dir);
    if browser_service_status(&socket_path).await?.is_none() {
        spawn_browser_service_process(&socket_path)?;
    }

    let mut last_error = None;
    for _ in 0..50 {
        match send_browser_service_request(&socket_path, BrowserServiceRequest::EnsureStarted).await
        {
            Ok(Some(BrowserServiceResponse::Ok { payload })) => return Ok(payload),
            Ok(Some(BrowserServiceResponse::Error { message })) => last_error = Some(message),
            Ok(Some(BrowserServiceResponse::Stopped)) => {
                last_error = Some("browser service stopped before startup completed".to_string());
            }
            Ok(Some(BrowserServiceResponse::ReadOk { .. })) => {
                last_error =
                    Some("browser service returned page payload for startup request".to_string());
            }
            Ok(None) => {}
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    Err(last_error.unwrap_or_else(|| "browser service did not become ready".to_string()))
}

async fn stop_browser_service(
    socket_path: &std::path::Path,
    config: &may_agent::config::BrowserConfig,
    may_dir: &std::path::Path,
) -> Result<(), String> {
    match send_browser_service_request(socket_path, BrowserServiceRequest::Stop).await? {
        Some(BrowserServiceResponse::Stopped) | None => {
            if config.requires_managed_runtime() {
                may_agent::container::browser::stop_managed_browser(config.port, &may_dir).await;
            }
            Ok(())
        }
        Some(BrowserServiceResponse::Ok { .. }) => Ok(()),
        Some(BrowserServiceResponse::Error { message }) => Err(message),
        Some(BrowserServiceResponse::ReadOk { .. }) => {
            Err("browser service returned page payload for stop request".to_string())
        }
    }
}

fn spawn_browser_service_process(socket_path: &std::path::Path) -> Result<(), String> {
    use std::process::Stdio;

    let current_exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = std::process::Command::new(current_exe);
    command
        .arg("browser")
        .arg("service")
        .arg("--socket")
        .arg(socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        command.process_group(0);
    }
    command.spawn().map_err(|error| error.to_string())?;
    Ok(())
}

async fn send_browser_service_request(
    socket_path: &std::path::Path,
    request: BrowserServiceRequest,
) -> Result<Option<BrowserServiceResponse>, String> {
    let mut stream = match tokio::net::UnixStream::connect(socket_path).await {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error.to_string()),
    };
    let payload = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    tokio::io::AsyncWriteExt::write_all(&mut stream, &payload)
        .await
        .map_err(|error| error.to_string())?;
    tokio::io::AsyncWriteExt::shutdown(&mut stream)
        .await
        .map_err(|error| error.to_string())?;
    let mut response_bytes = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response_bytes)
        .await
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&response_bytes)
        .map(Some)
        .map_err(|error| error.to_string())
}

async fn try_fetch_browser_targets(
    endpoint: &str,
) -> Result<Option<Vec<BrowserTargetOutput>>, BrowserCliError> {
    if can_connect_to_browser_endpoint(endpoint).await? {
        Ok(Some(fetch_browser_targets(endpoint).await?))
    } else {
        Ok(None)
    }
}

async fn require_browser_targets(
    endpoint: &str,
) -> Result<Vec<BrowserTargetOutput>, BrowserCliError> {
    match try_fetch_browser_targets(endpoint).await? {
        Some(targets) => Ok(targets),
        None => Err(BrowserCliError::BrowserNotRunning(endpoint.to_string())),
    }
}

async fn can_connect_to_browser_endpoint(endpoint: &str) -> Result<bool, BrowserCliError> {
    let websocket_endpoint = match resolve_browser_websocket_endpoint(endpoint).await {
        Ok(websocket_endpoint) => websocket_endpoint,
        Err(
            BrowserCliError::FetchBrowserVersion { .. }
            | BrowserCliError::FetchBrowserVersionStatus { .. }
            | BrowserCliError::MissingBrowserDebuggerWebSocket { .. },
        ) => return Ok(false),
        Err(error) => return Err(error),
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        chromiumoxide::browser::Browser::connect(websocket_endpoint),
    )
    .await
    {
        Ok(Ok((_browser, mut handler))) => {
            let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });
            handler_task.abort();
            Ok(true)
        }
        Ok(Err(_)) | Err(_) => Ok(false),
    }
}

async fn fetch_browser_targets(
    endpoint: &str,
) -> Result<Vec<BrowserTargetOutput>, BrowserCliError> {
    let websocket_endpoint = resolve_browser_websocket_endpoint(endpoint).await?;
    let (mut browser, mut handler) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        chromiumoxide::browser::Browser::connect(websocket_endpoint),
    )
    .await
    .map_err(|_| BrowserCliError::ConnectBrowserTimeout(endpoint.to_string()))?
    .map_err(|source| BrowserCliError::ConnectBrowser {
        endpoint: endpoint.to_string(),
        source,
    })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });
    let targets = tokio::time::timeout(std::time::Duration::from_secs(5), browser.fetch_targets())
        .await
        .map_err(|_| BrowserCliError::FetchTargetsTimeout(endpoint.to_string()))?
        .map_err(|source| BrowserCliError::FetchTargets {
            endpoint: endpoint.to_string(),
            source,
        })?;
    handler_task.abort();
    Ok(targets
        .into_iter()
        .map(|target| BrowserTargetOutput {
            target_id: String::from(target.target_id),
            target_type: target.r#type,
            title: target.title,
            url: target.url,
            attached: target.attached,
            browser_context_id: target.browser_context_id.map(String::from),
        })
        .collect())
}

async fn resolve_browser_websocket_endpoint(endpoint: &str) -> Result<String, BrowserCliError> {
    if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        return Ok(endpoint.to_string());
    }
    let metadata_url = format!("{}/json/version", endpoint.trim_end_matches('/'));
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("reqwest client builder must succeed")
        .get(metadata_url)
        .send()
        .await
        .map_err(|source| BrowserCliError::FetchBrowserVersion {
            endpoint: endpoint.to_string(),
            source,
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(BrowserCliError::FetchBrowserVersionStatus {
            endpoint: endpoint.to_string(),
            status: status.as_u16(),
        });
    }
    let metadata = response
        .json::<BrowserVersionMetadata>()
        .await
        .map_err(|source| BrowserCliError::FetchBrowserVersion {
            endpoint: endpoint.to_string(),
            source,
        })?;
    metadata.web_socket_debugger_url.ok_or_else(|| {
        BrowserCliError::MissingBrowserDebuggerWebSocket {
            endpoint: endpoint.to_string(),
        }
    })
}

async fn inspect_browser_listener_state(
    port: u16,
) -> Result<BrowserListenerState, BrowserCliError> {
    tokio::task::spawn_blocking(move || inspect_browser_listener_state_blocking(port))
        .await
        .expect("browser listener inspection task must not panic")
}

fn inspect_browser_listener_state_blocking(
    port: u16,
) -> Result<BrowserListenerState, BrowserCliError> {
    let output = std::process::Command::new("lsof")
        .args(["-tiTCP", &port.to_string(), "-sTCP:LISTEN"])
        .output()
        .map_err(|error| BrowserCliError::InspectBrowserListener {
            port,
            details: error.to_string(),
        })?;

    if !output.status.success() && output.stdout.is_empty() {
        return Ok(BrowserListenerState::NotListening);
    }

    let pids = String::from_utf8_lossy(&output.stdout);
    if pids.trim().is_empty() {
        return Ok(BrowserListenerState::NotListening);
    }

    for pid in pids.split_whitespace() {
        let command = std::process::Command::new("ps")
            .args(["-p", pid, "-o", "args="])
            .output()
            .map_err(|error| BrowserCliError::InspectBrowserListener {
                port,
                details: error.to_string(),
            })?;
        if !command.status.success() {
            return Err(BrowserCliError::InspectBrowserListener {
                port,
                details: format!("ps failed for pid {pid}"),
            });
        }
        if String::from_utf8_lossy(&command.stdout).contains("chromiumoxide-runner") {
            return Ok(BrowserListenerState::Managed);
        }
    }

    Ok(BrowserListenerState::ExternalProcess)
}

#[derive(Debug, thiserror::Error)]
enum ExecError {
    #[error("failed to resolve May home directory")]
    MissingHomeDirectory,
    #[error("failed to initialize session persistence: {0}")]
    InitializeSession(may_agent::session::SessionError),
    #[error("session '{0}' was not found")]
    SessionNotFound(String),
    #[error("failed to load profile '{profile_id}': {source}")]
    LoadProfile {
        profile_id: String,
        source: may_agent::db::ChatDbError,
    },
    #[error("invalid profile '{profile_id}': {source}")]
    InvalidProfile {
        profile_id: String,
        source: may_agent::db::ChatDbError,
    },
    #[error("failed to resolve agent environment: {0}")]
    ResolveEnvironment(may_agent::ResolveAgentEnvironmentError),
    #[error("failed to load MCP terminal commands: {0}")]
    LoadMcpConnections(may_agent::mcp::McpConnectionLoadError),
    #[error("failed to resolve provider runtime: {0}")]
    ResolveProviderRuntime(may_agent::agent::MayAgentError),
    #[error("failed to load May config: {0}")]
    LoadConfig(may_agent::config::ConfigError),
    #[error("failed to compile network policy: {0}")]
    CompileNetworkPolicy(String),
    #[error("failed to create terminal tool: {0}")]
    CreateTerminalTool(aither_sandbox::TerminalError),
    #[error("failed to initialize tool dispatch runtime: {0}")]
    InitializeToolDispatch(may_agent::tool_dispatch::ToolDispatchError),
    #[error("failed to configure ssh servers: {0}")]
    ConfigureSshServers(String),
    #[error("terminal execution failed: {0}")]
    TerminalCall(aither_core::Error),
    #[error("failed to parse terminal result: {0}")]
    ParseTerminalResult(serde_json::Error),
    #[error("failed to write stdout: {0}")]
    WriteStdout(std::io::Error),
    #[error("failed to write stderr: {0}")]
    WriteStderr(std::io::Error),
}

#[derive(Debug)]
struct ExecSessionContext {
    db: ChatDb,
    may_dir: std::path::PathBuf,
    session_id: SessionId,
    sandbox_dir: std::path::PathBuf,
    profile_environment: Option<ProfileEnvironment>,
    profile_model: Option<may_agent::db::entity::profiles::Model>,
    profile_container_preferences: Option<may_agent::profile::ContainerProfilePreferences>,
}

#[derive(Debug, Clone, Copy)]
struct NoopSshAuthorizer;

impl aither_sandbox::SshSessionAuthorizer for NoopSshAuthorizer {
    fn authorize_connect(
        &self,
        _target: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + '_>>
    {
        Box::pin(async { Ok(true) })
    }

    fn authorize_heel_install(
        &self,
        _target: &str,
        _details: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + '_>>
    {
        Box::pin(async { Ok(true) })
    }
}

async fn run_exec(args: ExecArgs) -> Result<i32, ExecError> {
    let context = load_exec_session_context(&args.session_id).await?;
    let config_path = context.may_dir.join("config.toml");
    let may_config = MayConfig::load_from(&config_path)
        .await
        .map_err(ExecError::LoadConfig)?;

    let environment = resolve_agent_environment_for_session(
        context.profile_environment.clone(),
        context.profile_model.as_ref(),
        context.profile_container_preferences.clone(),
        &context.may_dir,
        &context.sandbox_dir,
        &context.session_id,
    )
    .await
    .map_err(ExecError::ResolveEnvironment)?;

    let model = select_exec_model(&may_config);
    let shell_mode = select_exec_mode(&args, &environment);
    let terminal_tool =
        build_exec_terminal_tool(&context, &environment, &may_config, model).await?;

    let output = terminal_tool
        .call(TerminalArgs {
            description: "execute the requested shell command".to_string(),
            script: args.script,
            mode: shell_mode,
            ssh_server_id: args.ssh_server_id,
            expect: OutputFormat::Text,
            resolution: aither_sandbox::MediaResolution::Auto,
            timeout: args.timeout,
            max_lines: args.max_lines,
            raw: args.raw,
        })
        .await
        .map_err(ExecError::TerminalCall)?;

    if output.is_error() {
        return Err(ExecError::TerminalCall(anyhow::anyhow!(
            output
                .error_message()
                .unwrap_or("terminal tool returned an error")
                .to_string()
        )));
    }
    let payload = output.render_for_model().map_err(ExecError::TerminalCall)?;
    let result: TerminalResult =
        serde_json::from_str(&payload).map_err(ExecError::ParseTerminalResult)?;
    render_exec_result(&result)?;
    Ok(result.exit_code)
}

async fn load_exec_session_context(session_id: &str) -> Result<ExecSessionContext, ExecError> {
    let home_dir = dirs::home_dir().ok_or(ExecError::MissingHomeDirectory)?;
    let may_dir = home_dir.join(".may");
    let (store, db, _default_profile_id) = initialize_session_persistence(&may_dir)
        .await
        .map_err(ExecError::InitializeSession)?;
    let session_row = db
        .get_session(session_id)
        .await
        .map_err(|error| ExecError::InitializeSession(error.into()))?
        .ok_or_else(|| ExecError::SessionNotFound(session_id.to_string()))?;
    let profile_id = session_row.profile_id.clone();
    let profile_model = match profile_id.as_deref() {
        Some(profile_id) => Some(
            db.get_profile(profile_id)
                .await
                .map_err(|source| ExecError::LoadProfile {
                    profile_id: profile_id.to_string(),
                    source,
                })?
                .ok_or_else(|| ExecError::LoadProfile {
                    profile_id: profile_id.to_string(),
                    source: may_agent::db::ChatDbError::InvalidProfileEnvironment(
                        "profile is missing".to_string(),
                    ),
                })?,
        ),
        None => None,
    };

    let profile_preferences = match profile_model.as_ref() {
        Some(model) => Some(db.load_profile_preferences(model).await.map_err(|source| {
            ExecError::LoadProfile {
                profile_id: profile_id
                    .clone()
                    .expect("profile id present when model exists"),
                source,
            }
        })?),
        None => None,
    };
    let profile_environment = match (profile_model.clone(), profile_preferences.as_ref()) {
        (Some(model), Some(preferences)) => Some(
            ProfileInfo::from_db_parts(model, preferences.clone())
                .map_err(|source| ExecError::InvalidProfile {
                    profile_id: profile_id
                        .clone()
                        .expect("profile id present when model exists"),
                    source,
                })?
                .environment
                .into_kind(),
        ),
        (None, None) => None,
        _ => panic!("profile preferences must exist exactly when profile model exists"),
    };

    ensure_exec_session_layout(
        &store,
        &profile_id,
        profile_environment.as_ref(),
        session_id,
    )
    .await?;

    Ok(ExecSessionContext {
        db,
        may_dir,
        session_id: SessionId::from_string(session_id),
        sandbox_dir: store.sandbox_dir(session_id, profile_id.as_deref()),
        profile_environment,
        profile_model,
        profile_container_preferences: profile_preferences
            .and_then(|preferences| preferences)
            .and_then(|preferences| preferences.container),
    })
}

async fn ensure_exec_session_layout(
    store: &SessionStore,
    profile_id: &Option<String>,
    profile_environment: Option<&ProfileEnvironment>,
    session_id: &str,
) -> Result<(), ExecError> {
    let layout = match profile_environment {
        Some(ProfileEnvironment::Container { .. }) => {
            may_agent::session::SandboxResourceLayout::ContainerMountPoints
        }
        _ => may_agent::session::SandboxResourceLayout::Symlinked,
    };

    if !store.exists(session_id, profile_id.as_deref()) {
        store
            .create_session_dirs(session_id, profile_id.as_deref(), layout)
            .await
            .map_err(|error| ExecError::InitializeSession(error.into()))?;
    }

    if matches!(
        profile_environment,
        Some(ProfileEnvironment::Container { .. })
    ) {
        store
            .reconcile_container_mount_points(session_id, profile_id.as_deref())
            .await
            .map_err(|error| ExecError::InitializeSession(error.into()))?;
    }

    Ok(())
}

async fn build_exec_terminal_tool(
    context: &ExecSessionContext,
    environment: &may_agent::AgentEnvironment,
    may_config: &MayConfig,
    model: &str,
) -> Result<
    TerminalTool<
        TerminalPermissionHandler,
        executor_core::DefaultExecutor,
        aither_sandbox::Configured,
    >,
    ExecError,
> {
    let mut shell_sessions = aither_sandbox::ShellSessionRegistry::new(match environment {
        may_agent::AgentEnvironment::Container { .. } => ShellRuntimeAvailability {
            local: false,
            container: true,
            ssh: false,
        },
        may_agent::AgentEnvironment::Remote { .. } => ShellRuntimeAvailability {
            local: false,
            container: false,
            ssh: true,
        },
        may_agent::AgentEnvironment::Local => ShellRuntimeAvailability {
            local: true,
            container: false,
            ssh: !may_config.ssh_servers.is_empty(),
        },
    });

    let ssh_servers = match environment {
        may_agent::AgentEnvironment::Remote { ssh_server } => vec![SshServer {
            name: ssh_server.name.clone(),
            target: ssh_server.target.clone(),
        }],
        _ => may_config
            .ssh_servers
            .iter()
            .map(|server| SshServer {
                name: server.name.clone(),
                target: server.target.clone(),
            })
            .collect(),
    };
    if !ssh_servers.is_empty() {
        shell_sessions = shell_sessions
            .with_ssh_servers(ssh_servers)
            .map_err(ExecError::ConfigureSshServers)?
            .with_ssh_authorizer(Arc::new(NoopSshAuthorizer));
    }

    let runtime_service = std::sync::Arc::new(may_agent::session::SessionRuntimeService::new(
        context.db.clone(),
        may_agent::ui_event_bus::UiEventBus::new(),
    ));
    let network_policy = may_config
        .preferences
        .network_policy
        .compile()
        .map_err(ExecError::CompileNetworkPolicy)?;

    let terminal_tool: TerminalTool<TerminalPermissionHandler, executor_core::DefaultExecutor> =
        TerminalTool::new_exact(
            &context.sandbox_dir,
            TerminalPermissionHandler::for_exec(
                context.session_id.as_str().to_string(),
                runtime_service,
                network_policy,
                context
                    .profile_model
                    .as_ref()
                    .map(|profile| profile.id.clone()),
            ),
            executor_core::DefaultExecutor,
        )
        .await
        .map_err(ExecError::CreateTerminalTool)?;

    let terminal_tool = match environment {
        may_agent::AgentEnvironment::Container {
            container_id,
            container_ipc_host,
            container_exec,
            ..
        } => terminal_tool
            .with_shell_sessions(shell_sessions)
            .with_container_runtime(ContainerShellRuntime::new(
                container_id.clone(),
                container_ipc_host.clone(),
                container_exec.clone(),
            )),
        _ => terminal_tool.with_shell_sessions(shell_sessions),
    };

    let resolved_provider = resolve_provider_runtime(may_config, model, 0)
        .await
        .map_err(ExecError::ResolveProviderRuntime)?;
    let (terminal_tool_factory, terminal_tool_factory_receiver) = terminal_tool_factory_channel();
    let tool_dispatchers = may_agent::tool_dispatch::ToolDispatchers::spawn(
        may_agent::tool_dispatch::ToolDispatchConfig {
            may_dir: context.may_dir.clone(),
            cwd: context.sandbox_dir.clone(),
            db: context.db.clone(),
            browser_config: may_config.browser.clone(),
            ssh_servers: may_config.ssh_servers.clone(),
            network_policy: may_config.preferences.network_policy.clone(),
            ask_user_broker: may_agent::ask_user::channel().0,
        },
    )
    .await
    .map_err(ExecError::InitializeToolDispatch)?;
    let registry = build_exec_registry(
        context,
        may_config,
        resolved_provider.llm,
        resolved_provider.search_embedder,
        &tool_dispatchers,
        terminal_tool_factory,
        environment,
    )
    .await?;
    let terminal_tool = terminal_tool.with_registry(registry);
    terminal_tool.start_factory_service(terminal_tool_factory_receiver);
    Ok(terminal_tool)
}

async fn build_exec_registry<LLM>(
    context: &ExecSessionContext,
    _may_config: &MayConfig,
    llm: LLM,
    search_embedder: may_agent::embedding_provider::EmbeddingModelResolution,
    tool_dispatchers: &may_agent::tool_dispatch::ToolDispatchers,
    terminal_tool_factory: aither_sandbox::TerminalToolFactory,
    environment: &may_agent::AgentEnvironment,
) -> Result<Arc<aither_sandbox::ToolRegistry>, ExecError>
where
    LLM: aither_core::LanguageModel + Clone + 'static,
{
    let mut builder = aither_sandbox::ToolRegistryBuilder::new();
    register_shared_terminal_commands(
        &mut builder,
        SharedTerminalCommandConfig {
            llm,
            session_id: context.session_id.as_str().to_string(),
            may_dir: context.may_dir.clone(),
            sandbox_dir: context.sandbox_dir.clone(),
            search_embedder,
            task_runtime: match environment {
                may_agent::AgentEnvironment::Container { .. } => TaskRuntimeKind::Container,
                may_agent::AgentEnvironment::Remote { .. } => TaskRuntimeKind::Remote,
                may_agent::AgentEnvironment::Local => TaskRuntimeKind::Local,
            },
            task_broker: Some(tool_dispatchers.task.clone()),
            notify_broker: Some(tool_dispatchers.notify.clone()),
            browser_broker: tool_dispatchers.browser.clone(),
            register_skills_broker: None,
            register_subagents_broker: None,
            workspace_request_broker: None,
            terminal_tool_factory: Some(terminal_tool_factory),
        },
    )
    .await
    .map_err(ExecError::LoadMcpConnections)?;
    Ok(Arc::new(builder.build(
        SessionPaths::new(context.sandbox_dir.clone()).outputs_dir(),
    )))
}

fn select_exec_model(config: &MayConfig) -> &str {
    config
        .last_model
        .as_deref()
        .or_else(|| config.models.advanced.first().map(String::as_str))
        .unwrap_or_else(|| panic!("may exec requires at least one configured model"))
}

fn select_exec_mode(
    args: &ExecArgs,
    environment: &may_agent::AgentEnvironment,
) -> TerminalExecutionMode {
    if let Some(mode) = args.mode {
        return mode;
    }
    if args.ssh_server_id.is_some() {
        return TerminalExecutionMode::Ssh;
    }
    match environment {
        may_agent::AgentEnvironment::Remote { .. } => TerminalExecutionMode::Ssh,
        _ => TerminalExecutionMode::Default,
    }
}

fn render_exec_result(result: &TerminalResult) -> Result<(), ExecError> {
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();

    let stdout_text = result.stdout.to_string();
    if !stdout_text.is_empty() {
        stdout
            .write_all(stdout_text.as_bytes())
            .map_err(ExecError::WriteStdout)?;
        if !stdout_text.ends_with('\n') {
            stdout.write_all(b"\n").map_err(ExecError::WriteStdout)?;
        }
        stdout.flush().map_err(ExecError::WriteStdout)?;
    }

    if let Some(stderr_entry) = &result.stderr {
        let stderr_text = stderr_entry.to_string();
        if !stderr_text.is_empty() {
            stderr
                .write_all(stderr_text.as_bytes())
                .map_err(ExecError::WriteStderr)?;
            if !stderr_text.ends_with('\n') {
                stderr.write_all(b"\n").map_err(ExecError::WriteStderr)?;
            }
        }
    }

    if let Some(task_id) = &result.task_id {
        let status = result.status.as_deref().unwrap_or("running");
        stderr
            .write_all(format!("background task: {task_id} ({status})\n").as_bytes())
            .map_err(ExecError::WriteStderr)?;
    }
    stderr.flush().map_err(ExecError::WriteStderr)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        BrowserCliArgs, BrowserCliCommand, CliMode, ExecArgs, OneShotArgs, TerminalExecutionMode,
        parse_cli_mode, parse_oneshot_selection, parse_reasoning_effort,
    };
    use aither_core::llm::model::ReasoningEffort;
    use may_agent::browser_service::{BrowserReadDetail, BrowserReadFormat, BrowserScreenshotMode};

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    #[test]
    fn parse_cli_mode_defaults_to_tui() {
        assert_eq!(parse_cli_mode(&args(&["may"])), CliMode::Tui);
    }

    #[test]
    fn parse_cli_mode_detects_ui_subcommand() {
        assert_eq!(
            parse_cli_mode(&args(&["may", "ui", "--host", "127.0.0.1"])),
            CliMode::Ui(vec!["--host".to_string(), "127.0.0.1".to_string()])
        );
    }

    #[test]
    fn parse_cli_mode_detects_browser_subcommand() {
        assert_eq!(
            parse_cli_mode(&args(&["may", "browser", "contexts"])),
            CliMode::Browser(BrowserCliArgs {
                command: BrowserCliCommand::Contexts,
            })
        );
    }

    #[test]
    fn parse_cli_mode_detects_browser_open_command() {
        assert_eq!(
            parse_cli_mode(&args(&["may", "browser", "open"])),
            CliMode::Browser(BrowserCliArgs {
                command: BrowserCliCommand::Start,
            })
        );
    }

    #[test]
    fn parse_cli_mode_detects_browser_read_flags() {
        assert_eq!(
            parse_cli_mode(&args(&[
                "may",
                "browser",
                "read",
                "https://example.com",
                "--detail",
                "full",
                "--screenshot",
                "full-page",
                "--format",
                "json",
            ])),
            CliMode::Browser(BrowserCliArgs {
                command: BrowserCliCommand::Read {
                    url: "https://example.com".to_string(),
                    detail: BrowserReadDetail::Full,
                    screenshot: BrowserScreenshotMode::FullPage,
                    format: BrowserReadFormat::Json,
                },
            })
        );
    }

    #[test]
    fn parse_cli_mode_joins_prompt_into_oneshot() {
        assert_eq!(
            parse_cli_mode(&args(&["may", "fix", "the", "bug"])),
            CliMode::OneShot(OneShotArgs {
                prompt: "fix the bug".to_string(),
                model: None,
                reasoning_effort: None,
            })
        );
    }

    #[test]
    fn parse_cli_mode_detects_exec_subcommand() {
        assert_eq!(
            parse_cli_mode(&args(&[
                "may",
                "exec",
                "--session",
                "session-123",
                "--mode",
                "sandboxed",
                "--timeout",
                "5",
                "--max-lines",
                "42",
                "--raw",
                "--",
                "pwd",
            ])),
            CliMode::Exec(ExecArgs {
                session_id: "session-123".to_string(),
                script: "pwd".to_string(),
                mode: Some(TerminalExecutionMode::Sandboxed),
                ssh_server_id: None,
                timeout: 5,
                raw: true,
                max_lines: 42,
            })
        );
    }

    #[test]
    fn parse_cli_mode_accepts_oneshot_flags() {
        assert_eq!(
            parse_cli_mode(&args(&[
                "may",
                "--model",
                "gpt-5.4",
                "--reasoning-effort",
                "low",
                "inspect",
                "repo",
            ])),
            CliMode::OneShot(OneShotArgs {
                prompt: "inspect repo".to_string(),
                model: Some("gpt-5.4".to_string()),
                reasoning_effort: Some(ReasoningEffort::Low),
            })
        );
    }

    #[test]
    fn parse_reasoning_effort_accepts_low_medium_high() {
        assert_eq!(parse_reasoning_effort("low"), ReasoningEffort::Low);
        assert_eq!(parse_reasoning_effort("medium"), ReasoningEffort::Medium);
        assert_eq!(parse_reasoning_effort("high"), ReasoningEffort::High);
    }

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
