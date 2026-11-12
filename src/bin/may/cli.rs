//! The `may` command line, declared with clap and resolved into a [`CliMode`].
//!
//! `may` alone is the TUI; a trailing positional is a one-shot prompt; every
//! other first word is a subcommand. clap refuses anything else.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use aither_core::llm::model::ReasoningEffort;
use aither_sandbox::SandboxMode;
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use may_agent::browser_service::{BrowserReadDetail, BrowserScreenshotMode};
use may_agent::session::SessionLaunch;

/// Top-level CLI mode the binary resolves from argv.
#[derive(Debug, PartialEq, Eq)]
pub enum CliMode {
    Tui(TuiArgs),
    Resume(ResumeArgs),
    Ui,
    /// The API-only daemon, on this address.
    Rest(SocketAddr),
    Daemon(DaemonCommand),
    Exec(ExecArgs),
    Browser(BrowserCliArgs),
    /// The computer-agent helper: its May dir, and the file its stderr
    /// goes to (a `LaunchServices` launch has no terminal).
    ComputerAgent {
        may_dir: PathBuf,
        stderr: Option<PathBuf>,
    },
    OneShot(OneShotArgs),
    /// Print the completion script of a shell.
    Completion(Shell),
    /// Replace this binary with the newest build and restart the daemon.
    Update,
}

/// Parsed `may` / `may chat` / `may --repo` TUI invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiArgs {
    /// How the TUI starts its sessions.
    pub launch: SessionLaunch,
}

/// Parsed `may resume` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeArgs {
    /// Session to reopen in the TUI; `None` resumes the most recent one.
    pub session: Option<String>,
}

/// Parsed `--model` / `--reasoning-effort` / prompt for one-shot runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneShotArgs {
    pub prompt: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Parsed `may daemon` invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonCommand {
    /// Run the daemon in the foreground.
    Run,
    /// Install and start the login service.
    Install,
    /// Stop and remove the login service.
    Uninstall,
    /// Report whether a daemon runs and its state.
    Status,
    /// Stop the running daemon.
    Stop,
    /// Stop the running daemon and start one of this build.
    Restart,
}

/// Parsed `may exec` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecArgs {
    pub session_id: String,
    pub script: String,
    pub mode: Option<SandboxMode>,
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
    },
    Provision,
    Kill,
    Service {
        socket_path: PathBuf,
    },
}

/// May: a terminal-first AI assistant.
///
/// Without arguments it opens the TUI in the current directory. Words that
/// are not a subcommand are a one-shot prompt: the assistant's text streams
/// to stdout and process events to stderr.
#[derive(Debug, Parser)]
#[command(name = "may", version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    launch: LaunchArgs,
}

/// The TUI on a repository, or a one-shot prompt.
#[derive(Debug, Args)]
struct LaunchArgs {
    /// Open the TUI on a repository (`owner/repo` or a local path).
    #[arg(long, value_name = "OWNER/REPO|PATH", conflicts_with_all = ["model", "reasoning_effort", "prompt"])]
    repo: Option<String>,
    /// Model id for the one-shot run.
    #[arg(long, value_name = "ID", requires = "prompt")]
    model: Option<String>,
    /// Reasoning effort for the one-shot run.
    #[arg(long, value_enum, ignore_case = true, requires = "prompt")]
    reasoning_effort: Option<EffortArg>,
    /// The one-shot prompt; several words are joined with spaces.
    #[arg(value_name = "PROMPT")]
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open the TUI with repo-less chats (no directory, no repository).
    Chat,
    /// Reopen a session in the TUI.
    Resume {
        /// Session id; the most recently active session when omitted.
        session: Option<String>,
    },
    /// Start the daemon if needed and open its web UI in the browser.
    Ui(ListenArgs),
    /// Run the daemon in the foreground, API only, without the embedded web UI.
    Rest(ListenArgs),
    /// Run the daemon in the foreground, or manage it.
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonAction>,
    },
    /// Run a terminal command in a session's home environment.
    Exec(ExecCli),
    /// Inspect or maintain the managed browser runtime.
    Browser {
        #[command(subcommand)]
        command: Option<BrowserSubcommand>,
    },
    /// The computer-agent helper the daemon launches.
    #[command(hide = true)]
    ComputerAgent {
        /// The May directory; the default one when omitted.
        #[arg(long)]
        may_dir: Option<PathBuf>,
        /// File the helper's stderr is appended to.
        #[arg(long)]
        stderr: Option<PathBuf>,
    },
    /// Update May: a release build to the latest GitHub release, a dev build
    /// to the latest successful CI build of main.
    Update,
    /// Print the completion script of a shell.
    Completion {
        /// The shell to complete for.
        shell: Shell,
    },
}

/// The address `may rest` serves on.
#[derive(Debug, Args)]
struct ListenArgs {
    /// Address to bind.
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,
    /// Port to bind.
    #[arg(long, default_value_t = may_agent::daemon::DEFAULT_PORT)]
    port: u16,
}

#[derive(Debug, Subcommand)]
enum DaemonAction {
    /// Install and start the login service.
    Install,
    /// Stop and remove the login service.
    Uninstall,
    /// Report whether a daemon runs and its state.
    Status,
    /// Stop the running daemon.
    Stop,
    /// Stop the running daemon and start one of this build.
    Restart,
}

#[derive(Debug, Args)]
struct ExecCli {
    /// Session whose home environment runs the command.
    #[arg(long, value_name = "ID")]
    session: String,
    /// Sandbox mode of the command.
    #[arg(long, value_enum, ignore_case = true)]
    mode: Option<ModeArg>,
    /// Attached SSH environment to run on.
    #[arg(long, value_name = "ID")]
    ssh_server: Option<String>,
    /// Seconds before the command is killed.
    #[arg(long, value_name = "SEC", default_value_t = 300)]
    timeout: u64,
    /// Lines of output kept.
    #[arg(long, value_name = "N", default_value_t = 200)]
    max_lines: u32,
    /// Print the output unprocessed.
    #[arg(long)]
    raw: bool,
    /// The command; its words are joined with spaces.
    #[arg(
        required = true,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "COMMAND"
    )]
    command: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum BrowserSubcommand {
    /// Report the browser runtime's state (the default).
    Status,
    /// Start the browser.
    Open,
    /// Stop the browser.
    Close,
    /// List the open targets.
    Targets,
    /// List the browser contexts.
    Contexts,
    /// Print the CDP endpoint.
    CdpEndpoint,
    /// Read a page.
    Read {
        url: String,
        /// How much of the page to return.
        #[arg(long, value_enum, default_value = "standard")]
        detail: DetailArg,
        /// Whether to attach a screenshot.
        #[arg(long, value_enum, default_value = "viewport")]
        screenshot: ScreenshotArg,
    },
    /// Download the browser runtime.
    Provision,
    /// Kill the browser processes.
    Kill,
    /// The browser service process the daemon launches.
    #[command(hide = true)]
    Service {
        #[arg(long)]
        socket: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EffortArg {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ModeArg {
    /// Run in the sandbox.
    Default,
    /// Run outside the sandbox.
    Unsafe,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DetailArg {
    Brief,
    Standard,
    Full,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ScreenshotArg {
    Off,
    Viewport,
    FullPage,
}

impl From<EffortArg> for ReasoningEffort {
    fn from(value: EffortArg) -> Self {
        match value {
            EffortArg::Low => Self::Low,
            EffortArg::Medium => Self::Medium,
            EffortArg::High => Self::High,
        }
    }
}

impl From<ModeArg> for SandboxMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Default => Self::Sandboxed,
            ModeArg::Unsafe => Self::Unsandboxed,
        }
    }
}

impl From<DetailArg> for BrowserReadDetail {
    fn from(value: DetailArg) -> Self {
        match value {
            DetailArg::Brief => Self::Brief,
            DetailArg::Standard => Self::Standard,
            DetailArg::Full => Self::Full,
        }
    }
}

impl From<ScreenshotArg> for BrowserScreenshotMode {
    fn from(value: ScreenshotArg) -> Self {
        match value {
            ScreenshotArg::Off => Self::Off,
            ScreenshotArg::Viewport => Self::Viewport,
            ScreenshotArg::FullPage => Self::FullPage,
        }
    }
}

impl From<BrowserSubcommand> for BrowserCliCommand {
    fn from(value: BrowserSubcommand) -> Self {
        match value {
            BrowserSubcommand::Status => Self::Status,
            BrowserSubcommand::Open => Self::Start,
            BrowserSubcommand::Close => Self::Stop,
            BrowserSubcommand::Targets => Self::Targets,
            BrowserSubcommand::Contexts => Self::Contexts,
            BrowserSubcommand::CdpEndpoint => Self::Endpoint,
            BrowserSubcommand::Read {
                url,
                detail,
                screenshot,
            } => Self::Read {
                url,
                detail: detail.into(),
                screenshot: screenshot.into(),
            },
            BrowserSubcommand::Provision => Self::Provision,
            BrowserSubcommand::Kill => Self::Kill,
            BrowserSubcommand::Service { socket } => Self::Service {
                socket_path: socket,
            },
        }
    }
}

impl From<Option<DaemonAction>> for DaemonCommand {
    fn from(value: Option<DaemonAction>) -> Self {
        match value {
            None => Self::Run,
            Some(DaemonAction::Install) => Self::Install,
            Some(DaemonAction::Uninstall) => Self::Uninstall,
            Some(DaemonAction::Status) => Self::Status,
            Some(DaemonAction::Stop) => Self::Stop,
            Some(DaemonAction::Restart) => Self::Restart,
        }
    }
}

/// The prompt words joined with spaces; blank when every word is.
fn joined(words: &[String]) -> String {
    words.join(" ").trim().to_string()
}

fn empty_error(what: &str) -> clap::Error {
    Cli::command().error(ErrorKind::InvalidValue, format!("{what} must not be empty"))
}

impl Cli {
    fn into_mode(self) -> Result<CliMode, clap::Error> {
        Ok(match self.command {
            None => {
                let LaunchArgs {
                    repo,
                    model,
                    reasoning_effort,
                    prompt,
                } = self.launch;
                if prompt.is_empty() {
                    CliMode::Tui(TuiArgs {
                        launch: repo.map_or_else(SessionLaunch::default, SessionLaunch::Repo),
                    })
                } else {
                    let prompt = joined(&prompt);
                    if prompt.is_empty() {
                        return Err(empty_error("the prompt"));
                    }
                    CliMode::OneShot(OneShotArgs {
                        prompt,
                        model,
                        reasoning_effort: reasoning_effort.map(Into::into),
                    })
                }
            }
            Some(Command::Chat) => CliMode::Tui(TuiArgs {
                launch: SessionLaunch::RepoLess,
            }),
            Some(Command::Resume { session }) => CliMode::Resume(ResumeArgs { session }),
            // `may ui` always opens the configured port; the pair is accepted
            // for compatibility and not used.
            Some(Command::Ui(_)) => CliMode::Ui,
            Some(Command::Rest(listen)) => CliMode::Rest(SocketAddr::new(listen.host, listen.port)),
            Some(Command::Daemon { action }) => CliMode::Daemon(action.into()),
            Some(Command::Exec(exec)) => {
                let script = joined(&exec.command);
                if script.is_empty() {
                    return Err(empty_error("the command"));
                }
                CliMode::Exec(ExecArgs {
                    session_id: exec.session,
                    script,
                    mode: exec.mode.map(Into::into),
                    ssh_server_id: exec.ssh_server,
                    timeout: exec.timeout,
                    raw: exec.raw,
                    max_lines: exec.max_lines,
                })
            }
            Some(Command::Browser { command }) => CliMode::Browser(BrowserCliArgs {
                command: command.map_or(BrowserCliCommand::Status, Into::into),
            }),
            Some(Command::ComputerAgent { may_dir, stderr }) => CliMode::ComputerAgent {
                may_dir: may_dir.unwrap_or_else(|| {
                    may_agent::MayConfig::may_dir()
                        .expect("the May directory must resolve for the computer agent")
                }),
                stderr,
            },
            Some(Command::Update) => CliMode::Update,
            Some(Command::Completion { shell }) => CliMode::Completion(shell),
        })
    }
}

/// Parse argv (program name first) into a [`CliMode`]. Help and version are
/// reported as the [`clap::Error`] clap uses for them.
pub fn parse_cli_mode<I, T>(args: I) -> Result<CliMode, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    Cli::try_parse_from(args)?.into_mode()
}

/// Parse the process's argv; help, version and a refusal print and exit.
pub fn parse_cli_mode_from_env() -> CliMode {
    parse_cli_mode(std::env::args_os()).unwrap_or_else(|error| error.exit())
}

/// The completion script of `shell` for the whole command line.
pub fn completion_script(shell: Shell) -> Vec<u8> {
    let mut command = Cli::command();
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut command, "may", &mut script);
    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ContextKind;

    fn parse(parts: &[&str]) -> CliMode {
        parse_cli_mode(parts).expect("the invocation parses")
    }

    fn refusal(parts: &[&str]) -> clap::Error {
        parse_cli_mode(parts).expect_err("the invocation is refused")
    }

    fn oneshot(prompt: &str) -> OneShotArgs {
        OneShotArgs {
            prompt: prompt.to_string(),
            model: None,
            reasoning_effort: None,
        }
    }

    #[test]
    fn defaults_to_tui() {
        assert_eq!(parse(&["may"]), CliMode::Tui(TuiArgs::default()));
    }

    #[test]
    fn chat_is_a_repo_less_tui() {
        assert_eq!(
            parse(&["may", "chat"]),
            CliMode::Tui(TuiArgs {
                launch: SessionLaunch::RepoLess
            })
        );
    }

    #[test]
    fn repo_starts_on_a_repository() {
        assert_eq!(
            parse(&["may", "--repo", "acme/app"]),
            CliMode::Tui(TuiArgs {
                launch: SessionLaunch::Repo("acme/app".to_string())
            })
        );
    }

    #[test]
    fn resume_takes_an_optional_session() {
        assert_eq!(
            parse(&["may", "resume"]),
            CliMode::Resume(ResumeArgs { session: None })
        );
        assert_eq!(
            parse(&["may", "resume", "calm-river-stone-lake"]),
            CliMode::Resume(ResumeArgs {
                session: Some("calm-river-stone-lake".to_string()),
            })
        );
    }

    #[test]
    fn ui_accepts_the_listen_pair() {
        assert_eq!(parse(&["may", "ui"]), CliMode::Ui);
        assert_eq!(parse(&["may", "ui", "--host", "127.0.0.1"]), CliMode::Ui);
    }

    #[test]
    fn rest_resolves_its_address() {
        assert_eq!(
            parse(&["may", "rest", "--port", "9000"]),
            CliMode::Rest("127.0.0.1:9000".parse().unwrap())
        );
        assert_eq!(
            parse(&["may", "rest", "--host=0.0.0.0", "--port=1"]),
            CliMode::Rest("0.0.0.0:1".parse().unwrap())
        );
        assert_eq!(
            parse(&["may", "rest"]),
            CliMode::Rest("127.0.0.1:8787".parse().unwrap())
        );
    }

    #[test]
    fn daemon_actions() {
        for (word, command) in [
            (None, DaemonCommand::Run),
            (Some("install"), DaemonCommand::Install),
            (Some("uninstall"), DaemonCommand::Uninstall),
            (Some("status"), DaemonCommand::Status),
            (Some("stop"), DaemonCommand::Stop),
            (Some("restart"), DaemonCommand::Restart),
        ] {
            let mut argv = vec!["may", "daemon"];
            argv.extend(word);
            assert_eq!(parse(&argv), CliMode::Daemon(command));
        }
    }

    #[test]
    fn browser_commands() {
        let browser = |command| CliMode::Browser(BrowserCliArgs { command });
        assert_eq!(
            parse(&["may", "browser"]),
            browser(BrowserCliCommand::Status)
        );
        for (word, command) in [
            ("status", BrowserCliCommand::Status),
            ("open", BrowserCliCommand::Start),
            ("close", BrowserCliCommand::Stop),
            ("targets", BrowserCliCommand::Targets),
            ("contexts", BrowserCliCommand::Contexts),
            ("cdp-endpoint", BrowserCliCommand::Endpoint),
            ("provision", BrowserCliCommand::Provision),
            ("kill", BrowserCliCommand::Kill),
        ] {
            assert_eq!(parse(&["may", "browser", word]), browser(command));
        }
        assert_eq!(
            parse(&["may", "browser", "service", "--socket", "/tmp/b.sock"]),
            browser(BrowserCliCommand::Service {
                socket_path: "/tmp/b.sock".into()
            })
        );
    }

    #[test]
    fn browser_read_flags() {
        assert_eq!(
            parse(&[
                "may",
                "browser",
                "read",
                "https://example.com",
                "--detail",
                "full",
                "--screenshot",
                "full-page",
            ]),
            CliMode::Browser(BrowserCliArgs {
                command: BrowserCliCommand::Read {
                    url: "https://example.com".to_string(),
                    detail: BrowserReadDetail::Full,
                    screenshot: BrowserScreenshotMode::FullPage,
                },
            })
        );
        assert_eq!(
            parse(&["may", "browser", "read", "https://example.com"]),
            CliMode::Browser(BrowserCliArgs {
                command: BrowserCliCommand::Read {
                    url: "https://example.com".to_string(),
                    detail: BrowserReadDetail::default(),
                    screenshot: BrowserScreenshotMode::default(),
                },
            })
        );
    }

    #[test]
    fn computer_agent_flags() {
        assert_eq!(
            parse(&[
                "may",
                "computer-agent",
                "--may-dir",
                "/tmp/may",
                "--stderr",
                "/tmp/log"
            ]),
            CliMode::ComputerAgent {
                may_dir: "/tmp/may".into(),
                stderr: Some("/tmp/log".into()),
            }
        );
    }

    #[test]
    fn prompt_words_are_joined() {
        assert_eq!(
            parse(&["may", "fix", "the", "bug"]),
            CliMode::OneShot(oneshot("fix the bug"))
        );
        assert_eq!(
            parse(&["may", "fix the bug"]),
            CliMode::OneShot(oneshot("fix the bug"))
        );
        assert_eq!(
            parse(&["may", "--", "--version"]),
            CliMode::OneShot(oneshot("--version"))
        );
    }

    #[test]
    fn oneshot_flags_in_any_position() {
        let expected = CliMode::OneShot(OneShotArgs {
            prompt: "inspect repo".to_string(),
            model: Some("gpt-5.4".to_string()),
            reasoning_effort: Some(ReasoningEffort::Low),
        });
        assert_eq!(
            parse(&[
                "may",
                "--model",
                "gpt-5.4",
                "--reasoning-effort",
                "low",
                "inspect",
                "repo",
            ]),
            expected
        );
        assert_eq!(
            parse(&[
                "may",
                "inspect repo",
                "--reasoning-effort",
                "LOW",
                "--model=gpt-5.4",
            ]),
            expected
        );
    }

    #[test]
    fn exec_flags_and_command() {
        assert_eq!(
            parse(&[
                "may",
                "exec",
                "--session",
                "session-123",
                "--mode",
                "default",
                "--timeout",
                "5",
                "--max-lines",
                "42",
                "--raw",
                "--",
                "pwd",
            ]),
            CliMode::Exec(ExecArgs {
                session_id: "session-123".to_string(),
                script: "pwd".to_string(),
                mode: Some(SandboxMode::Sandboxed),
                ssh_server_id: None,
                timeout: 5,
                raw: true,
                max_lines: 42,
            })
        );
        assert_eq!(
            parse(&[
                "may",
                "exec",
                "--session",
                "s",
                "--mode",
                "unsafe",
                "--ssh-server",
                "box",
                "ls",
                "-la",
                "/tmp",
            ]),
            CliMode::Exec(ExecArgs {
                session_id: "s".to_string(),
                script: "ls -la /tmp".to_string(),
                mode: Some(SandboxMode::Unsandboxed),
                ssh_server_id: Some("box".to_string()),
                timeout: 300,
                raw: false,
                max_lines: 200,
            })
        );
    }

    #[test]
    fn update_is_a_subcommand() {
        assert_eq!(parse(&["may", "update"]), CliMode::Update);
    }

    #[test]
    fn completion_names_the_shell() {
        assert_eq!(
            parse(&["may", "completion", "fish"]),
            CliMode::Completion(Shell::Fish)
        );
    }

    #[test]
    fn completion_script_is_not_empty() {
        let script = String::from_utf8(completion_script(Shell::Fish)).unwrap();
        assert!(script.contains("complete -c may"), "{script}");
    }

    #[test]
    fn version_is_the_package_version() {
        let error = refusal(&["may", "--version"]);
        assert_eq!(error.kind(), ErrorKind::DisplayVersion);
        assert_eq!(
            error.to_string().trim(),
            format!("may {}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(refusal(&["may", "-V"]).kind(), ErrorKind::DisplayVersion);
    }

    #[test]
    fn help_is_displayed_for_may_and_subcommands() {
        for argv in [
            &["may", "--help"][..],
            &["may", "-h"],
            &["may", "exec", "--help"],
            &["may", "browser", "read", "--help"],
        ] {
            assert_eq!(refusal(argv).kind(), ErrorKind::DisplayHelp, "{argv:?}");
        }
    }

    #[test]
    fn a_mistyped_flag_is_refused_with_a_suggestion() {
        let error = refusal(&["may", "--modle", "x"]);
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
        let suggested = error
            .get(ContextKind::SuggestedArg)
            .map(ToString::to_string);
        assert_eq!(suggested.as_deref(), Some("--model"));
        assert_eq!(
            refusal(&["may", "--bogus"]).kind(),
            ErrorKind::UnknownArgument
        );
    }

    #[test]
    fn a_mistyped_subcommand_is_refused_with_a_suggestion() {
        let error = refusal(&["may", "browser", "staus"]);
        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
        let suggested = error
            .get(ContextKind::SuggestedSubcommand)
            .map(ToString::to_string);
        assert_eq!(suggested.as_deref(), Some("status"));
    }

    #[test]
    fn invalid_values_are_refused() {
        for argv in [
            &["may", "completion", "tcsh"][..],
            &["may", "--reasoning-effort", "extreme", "hi"],
            &["may", "--model", "x"],
            &["may", "--repo", "a/b", "hi"],
            &["may", "rest", "--port", "http"],
            &["may", "exec", "--session", "s"],
            &["may", "exec", "--mode", "root", "--session", "s", "ls"],
            &["may", "browser", "read"],
            &["may", "resume", "a", "b"],
        ] {
            assert!(parse_cli_mode(argv).is_err(), "{argv:?}");
        }
    }

    #[test]
    fn a_blank_prompt_is_refused() {
        assert_eq!(refusal(&["may", " "]).kind(), ErrorKind::InvalidValue);
        assert_eq!(
            refusal(&["may", "exec", "--session", "s", ""]).kind(),
            ErrorKind::InvalidValue
        );
    }
}
