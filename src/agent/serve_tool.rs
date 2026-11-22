//! The `serve` IPC command: a session-scoped service — a background job
//! with no time bound that dies with the session — optionally exposed to
//! the user through a daemon tunnel.

use std::borrow::Cow;

use aither_core::llm::{Tool, ToolResult};
use aither_sandbox::ServiceArgs;
use askama::Template;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agent::tunnel_tool::{TunnelBroker, TunnelRequest};
use crate::ipc::SessionShell;

/// How long `serve` waits for `ready_regex` before reporting anyway.
const READY_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Start a session-scoped service: a detached job with no timeout, killed
/// with the session. `-- <command>` runs it verbatim.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ServeArgs {
    /// Expose the service through a tunnel: a port number, or `auto` to have
    /// one allocated and published as `PORT`/`SERVE_PORT` in the service's
    /// environment.
    #[serde(default)]
    pub port: Option<String>,
    /// Human-readable name of the service.
    #[serde(default)]
    pub description: Option<String>,
    /// Return once the service's output matches this regular expression, or
    /// after a short wait.
    #[serde(default)]
    pub ready_regex: Option<String>,
    /// The service command, verbatim.
    #[schemars(extend("x-positional" = true))]
    pub command: Vec<String>,
}

/// What the model reads after `serve` started a service.
#[derive(Debug, Template)]
#[template(path = "serve_started.txt", escape = "none")]
struct ServeStarted {
    pid: u32,
    tunnel: Option<ServeTunnel>,
    /// `None` for no `--ready-regex`, else whether the pattern matched.
    ready: Option<bool>,
}

#[derive(Debug)]
struct ServeTunnel {
    id: String,
    local_url: String,
}

#[derive(Debug, thiserror::Error)]
enum ServeToolError {
    #[error("serve needs a session shell, which this runtime has none")]
    NoShell,
    #[error("serve needs a command to run")]
    NoCommand,
    #[error("invalid --port {0:?}: a port number or `auto`")]
    BadPort(String),
    #[error("--port needs the tunnel command, which this runtime has none")]
    NoTunnel,
    #[error("tunnel request was rejected: {0}")]
    Rejected(String),
    #[error("the service exited before matching --ready-regex: {0}")]
    ExitedEarly(String),
    #[error("invalid --ready-regex: {0}")]
    BadRegex(#[from] regex::Error),
    #[error("service spawn failed: {0}")]
    Spawn(#[from] anyhow::Error),
}

/// The port `--port` resolves to.
enum Port {
    /// `auto`: allocate one and publish it in the service's environment.
    Allocate,
    /// A port the caller chose.
    Given(u16),
}

fn port(port: Option<&str>) -> Result<Option<Port>, ServeToolError> {
    match port.map(str::trim) {
        None => Ok(None),
        Some("auto") => Ok(Some(Port::Allocate)),
        Some(text) => text
            .parse::<u16>()
            .map(|port| Some(Port::Given(port)))
            .map_err(|_| ServeToolError::BadPort(text.to_string())),
    }
}

/// A free loopback port, released for the service to take.
fn allocate_port() -> std::io::Result<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()
        .map(|address| address.port())
}

/// `command` as a shell script, each word escaped and `exec`-ed so the
/// service is the job's root process, with `PORT`/`SERVE_PORT` exported
/// when a port is exposed.
fn script(command: &[String], port: Option<u16>) -> String {
    let words = command
        .iter()
        .map(|word| shell_escape::escape(Cow::Borrowed(word.as_str())))
        .collect::<Vec<_>>()
        .join(" ");
    port.map_or_else(
        || format!("exec {words}"),
        |port| format!("PORT={port} SERVE_PORT={port} exec {words}"),
    )
}

/// The `serve` tool: serving files from the session's shell.
#[derive(Clone)]
pub struct ServeTool {
    shell: Option<SessionShell>,
    session_id: String,
    tunnel: Option<TunnelBroker>,
}

impl std::fmt::Debug for ServeTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeTool")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl ServeTool {
    /// A serve tool for `session_id` on `shell`, tunneling via `tunnel`.
    #[must_use]
    pub const fn new(
        shell: Option<SessionShell>,
        session_id: String,
        tunnel: Option<TunnelBroker>,
    ) -> Self {
        Self {
            shell,
            session_id,
            tunnel,
        }
    }

    /// Waits until the job's output matches `pattern`, it exits, or
    /// [`READY_WAIT`] passes; `false` is a wait that timed out.
    async fn wait_ready(
        &self,
        job: &aither_sandbox::Job,
        pattern: &str,
    ) -> Result<bool, ServeToolError> {
        let regex = regex::Regex::new(pattern)?;
        let deadline = std::time::Instant::now() + READY_WAIT;
        let mut cursor = 0;
        let mut tail = String::new();
        while std::time::Instant::now() < deadline {
            let read = job
                .read(cursor, 1 << 20)
                .await
                .map_err(anyhow::Error::msg)?;
            match read {
                aither_sandbox::JobRead::Delta(delta) => {
                    cursor = delta.cursor;
                    tail.push_str(&String::from_utf8_lossy(&delta.bytes));
                    let keep = tail.len().saturating_sub(1 << 16);
                    if keep > 0 {
                        tail.drain(..keep);
                    }
                    if regex.is_match(&tail) {
                        return Ok(true);
                    }
                    if !matches!(delta.status, aither_sandbox::JobStatus::Running) {
                        return Err(ServeToolError::ExitedEarly(tail));
                    }
                }
                aither_sandbox::JobRead::Unchanged { .. } => {
                    async_io::Timer::after(std::time::Duration::from_millis(200)).await;
                }
            }
        }
        Ok(false)
    }
}

impl Tool for ServeTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("serve")
    }

    type Arguments = ServeArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        if args.command.is_empty() {
            return Err(ServeToolError::NoCommand.into());
        }
        let shell = self.shell.clone().ok_or(ServeToolError::NoShell)?;
        let port = match port(args.port.as_deref())? {
            Some(Port::Allocate) => Some(allocate_port().map_err(anyhow::Error::from)?),
            Some(Port::Given(port)) => Some(port),
            None => None,
        };
        let description = args
            .description
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| args.command.join(" "));
        let job = shell
            .run_service(ServiceArgs {
                description,
                script: script(&args.command, port),
                interpreter: aither_sandbox::Interpreter::default(),
                sandbox: aither_sandbox::SandboxMode::default(),
                location: None,
                resolution: aither_sandbox::MediaResolution::default(),
                max_lines: None,
                raw: false,
            })
            .await
            .map_err(ServeToolError::Spawn)?;

        let tunnel = if let Some(port) = port {
            let broker = self.tunnel.clone().ok_or(ServeToolError::NoTunnel)?;
            let result = broker
                .request(TunnelRequest {
                    session_id: self.session_id.clone(),
                    url: format!("http://localhost:{port}"),
                })
                .await?
                .map_err(ServeToolError::Rejected)?;
            Some(ServeTunnel {
                id: result.id,
                local_url: result.local_url,
            })
        } else {
            None
        };

        let ready = if let Some(pattern) = args.ready_regex.as_deref() {
            Some(self.wait_ready(&job, pattern).await?)
        } else {
            None
        };

        let text = ServeStarted {
            pid: job.pid(),
            tunnel,
            ready,
        }
        .render()
        .expect("serve started template renders");
        Ok(ToolResult::text(text))
    }
}
