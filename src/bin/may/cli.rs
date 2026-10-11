//! Argument parsing for the `may` top-level binary.
//!
//! The parser is hand-rolled (no clap dependency) so startup is fast
//! for the common TUI case and so unknown arguments are treated as the
//! beginning of a one-shot prompt rather than rejected.

use aither_core::llm::model::ReasoningEffort;
use aither_sandbox::TerminalExecutionMode;
use may_agent::browser_service::{BrowserReadDetail, BrowserReadFormat, BrowserScreenshotMode};

/// Top-level CLI mode the binary resolves from argv.
#[derive(Debug, PartialEq, Eq)]
pub enum CliMode {
    Tui,
    Ui(Vec<String>),
    Rest(Vec<String>),
    Exec(ExecArgs),
    Browser(BrowserCliArgs),
    OneShot(OneShotArgs),
    Help(&'static str),
}

/// Parsed `--model` / `--reasoning-effort` / prompt for one-shot runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneShotArgs {
    pub prompt: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Parsed `may exec` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecArgs {
    pub session_id: String,
    pub script: String,
    pub mode: Option<TerminalExecutionMode>,
    pub ssh_server_id: Option<String>,
    pub timeout: u64,
    pub raw: bool,
    pub max_lines: u32,
}

/// Parsed `may browser` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserCliArgs {
    pub command: BrowserCliCommand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserCliCommand {
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
    pub fn help_text() -> &'static str {
        "Usage: may [--model <id>] [--reasoning-effort <low|medium|high>] <prompt>\n       may ui [args...]\n       may rest [args...]\n\nOne-shot mode streams assistant text to stdout and process events to stderr."
    }
}

pub struct UiArgs;

impl UiArgs {
    pub fn help_text() -> &'static str {
        "Usage: may ui [--host <addr>] [--port <port>] [--open]\n\nRun the May web UI and API server."
    }
}

pub struct RestArgs;

impl RestArgs {
    pub fn help_text() -> &'static str {
        "Usage: may rest [--host <addr>] [--port <port>]\n\nRun the May REST API server without the embedded web UI."
    }
}

impl ExecArgs {
    pub fn help_text() -> &'static str {
        "Usage: may exec --session <id> [--ssh-server <id>] [--timeout <sec>] [--max-lines <n>] [--raw] [--] <command...>\n\nExec runs a terminal command in the session home environment. Use --ssh-server to target an attached SSH environment, and container profile sessions execute inside the configured container environment."
    }
}

impl BrowserCliArgs {
    pub fn help_text() -> &'static str {
        "Usage: may browser <status|open|close|targets|contexts|cdp-endpoint|read <url> [--detail <brief|standard|full>] [--screenshot <off|viewport|full-page>] [--format json]|provision|kill>\n\nInspect or maintain the May-managed browser runtime used for browser automation."
    }
}

/// Parse argv into a [`CliMode`].
pub fn parse_cli_mode(args: &[String]) -> CliMode {
    match args.get(1).map(String::as_str) {
        Some("--help" | "-h") => CliMode::Help(OneShotArgs::help_text()),
        Some("ui") => match args.get(2).map(String::as_str) {
            Some("--help" | "-h") => CliMode::Help(UiArgs::help_text()),
            _ => CliMode::Ui(args[2..].to_vec()),
        },
        Some("rest") => match args.get(2).map(String::as_str) {
            Some("--help" | "-h") => CliMode::Help(RestArgs::help_text()),
            _ => CliMode::Rest(args[2..].to_vec()),
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

pub fn parse_cli_mode_from_env() -> CliMode {
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

pub fn parse_reasoning_effort(value: &str) -> ReasoningEffort {
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

#[cfg(test)]
mod tests {
    use super::{
        BrowserCliArgs, BrowserCliCommand, CliMode, ExecArgs, OneShotArgs, TerminalExecutionMode,
        parse_cli_mode, parse_reasoning_effort,
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
    fn parse_cli_mode_detects_rest_subcommand() {
        assert_eq!(
            parse_cli_mode(&args(&["may", "rest", "--port", "8787"])),
            CliMode::Rest(vec!["--port".to_string(), "8787".to_string()])
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
}
