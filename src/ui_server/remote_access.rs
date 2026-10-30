//! Remote access over Tailscale.
//!
//! When enabled, the daemon asks the local `tailscale` to serve its loopback
//! TCP listener on the tailnet over HTTPS (`tailscale serve --bg`), so every
//! device on the user's tailnet reaches the web UI and API at the machine's
//! `MagicDNS` name. Tailscale holds the configuration itself; the manager only
//! adds and removes the one handler it owns and checks that it is still in
//! place.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::RemoteAccessConfig;
use crate::db::ChatDb;
use crate::ui_event_bus::UiEventBus;

use super::error::{bad_request_error, internal_error};
use super::state::UiServerState;
use super::types::{ApiHealth, RemoteAccessState, RemoteAccessStatusResponse};

const MAX_REMOTE_ACCESS_LOGS: usize = 48;
/// How often a running serve is checked against `tailscale serve status`.
const SERVE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// One `tailscale serve` handler: the tailnet HTTPS port forwarding to the
/// daemon's loopback listener.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServeSpec {
    tailscale_binary: String,
    https_port: u16,
    /// Proxy target, `http://127.0.0.1:<port>`.
    target: String,
}

#[derive(Debug)]
struct RemoteAccessRuntime {
    status: RemoteAccessStatusResponse,
    /// The handler this daemon added, while it is in place.
    serving: Option<ServeSpec>,
}

impl Default for RemoteAccessRuntime {
    fn default() -> Self {
        Self {
            status: RemoteAccessStatusResponse {
                state: RemoteAccessState::Disabled,
                enabled: false,
                public_url: None,
                last_error: None,
                recent_logs: Vec::new(),
            },
            serving: None,
        }
    }
}

enum RemoteAccessCommand {
    Sync {
        config: RemoteAccessConfig,
        has_local_profile: bool,
        response: oneshot::Sender<Result<(), String>>,
    },
    Stop {
        response: oneshot::Sender<Result<(), String>>,
    },
}

#[derive(Clone)]
pub struct RemoteAccessManager {
    command_tx: mpsc::Sender<RemoteAccessCommand>,
    status_rx: watch::Receiver<RemoteAccessStatusResponse>,
    /// The daemon's loopback TCP address, once it listens.
    local_addr: watch::Sender<Option<SocketAddr>>,
}

impl std::fmt::Debug for RemoteAccessManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteAccessManager")
            .finish_non_exhaustive()
    }
}

impl RemoteAccessManager {
    pub(crate) fn new(ui_event_bus: UiEventBus) -> Self {
        let (command_tx, command_rx) = mpsc::channel(8);
        let (status_tx, status_rx) = watch::channel(RemoteAccessRuntime::default().status);
        let (local_addr, local_addr_rx) = watch::channel(None);
        tokio::spawn(run_remote_access_actor(
            command_rx,
            status_tx,
            local_addr_rx,
            ui_event_bus,
        ));
        Self {
            command_tx,
            status_rx,
            local_addr,
        }
    }

    /// Records the TCP address the daemon serves on; remote access forwards
    /// to its port on loopback.
    pub(crate) fn set_local_addr(&self, addr: SocketAddr) {
        self.local_addr.send_replace(Some(addr));
    }

    pub(crate) fn snapshot(&self) -> RemoteAccessStatusResponse {
        self.status_rx.borrow().clone()
    }

    pub(crate) async fn sync(
        &self,
        config: RemoteAccessConfig,
        has_local_profile: bool,
    ) -> Result<(), String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(RemoteAccessCommand::Sync {
                config,
                has_local_profile,
                response: response_tx,
            })
            .await
            .map_err(|_| "remote access manager is unavailable".to_string())?;
        response_rx
            .await
            .map_err(|_| "remote access manager response dropped".to_string())?
    }

    pub(crate) async fn stop(&self) -> Result<(), String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(RemoteAccessCommand::Stop {
                response: response_tx,
            })
            .await
            .map_err(|_| "remote access manager is unavailable".to_string())?;
        response_rx
            .await
            .map_err(|_| "remote access manager response dropped".to_string())?
    }
}

pub async fn sync_remote_access_from_state(
    state: &UiServerState,
) -> Result<(), crate::db::ChatDbError> {
    let config = state.config_snapshot().remote_access.clone();
    let db = state.db();
    let has_local_profile = has_local_profile(&db).await?;
    if let Err(error) = state
        .remote_access_manager
        .sync(config, has_local_profile)
        .await
    {
        tracing::warn!(error = %error, "failed to sync remote access state");
    }
    Ok(())
}

pub(super) async fn get_remote_access_status(
    skyzen::utils::State(state): skyzen::utils::State<Arc<UiServerState>>,
) -> skyzen::Result<skyzen::utils::Json<RemoteAccessStatusResponse>> {
    Ok(skyzen::utils::Json(state.remote_access_manager.snapshot()))
}

pub(super) async fn start_remote_access(
    skyzen::utils::State(state): skyzen::utils::State<Arc<UiServerState>>,
) -> skyzen::Result<skyzen::utils::Json<ApiHealth>> {
    sync_remote_access_from_state(&state)
        .await
        .map_err(internal_error)?;
    let status = state.remote_access_manager.snapshot();
    if matches!(
        status.state,
        RemoteAccessState::Error | RemoteAccessState::Disabled
    ) {
        let message = status
            .last_error
            .unwrap_or_else(|| "remote access failed to start".to_string());
        return Err(bad_request_error(message));
    }
    Ok(skyzen::utils::Json(ApiHealth { ok: true }))
}

pub(super) async fn stop_remote_access(
    skyzen::utils::State(state): skyzen::utils::State<Arc<UiServerState>>,
) -> skyzen::Result<skyzen::utils::Json<ApiHealth>> {
    state
        .remote_access_manager
        .stop()
        .await
        .map_err(bad_request_error)?;
    Ok(skyzen::utils::Json(ApiHealth { ok: true }))
}

async fn has_local_profile(db: &ChatDb) -> Result<bool, crate::db::ChatDbError> {
    Ok(db.list_profiles().await?.iter().any(|profile| {
        crate::db::ProfileEnvironmentKind::from_str(profile.environment.as_str())
            .is_ok_and(|environment| environment == crate::db::ProfileEnvironmentKind::Local)
    }))
}

async fn run_remote_access_actor(
    mut command_rx: mpsc::Receiver<RemoteAccessCommand>,
    status_tx: watch::Sender<RemoteAccessStatusResponse>,
    local_addr: watch::Receiver<Option<SocketAddr>>,
    ui_event_bus: UiEventBus,
) {
    let mut runtime = RemoteAccessRuntime::default();
    let mut check = tokio::time::interval(SERVE_CHECK_INTERVAL);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            command = command_rx.recv() => {
                let Some(command) = command else { break };
                match command {
                    RemoteAccessCommand::Sync { config, has_local_profile, response } => {
                        let addr = *local_addr.borrow();
                        let result = sync_runtime(&mut runtime, config, has_local_profile, addr).await;
                        if let Err(error) = &result {
                            runtime.status.state = RemoteAccessState::Error;
                            runtime.status.last_error = Some(error.clone());
                            push_log(&mut runtime.status.recent_logs, error);
                        }
                        publish(&runtime.status, &status_tx, &ui_event_bus);
                        let _ = response.send(result);
                    }
                    RemoteAccessCommand::Stop { response } => {
                        let result = stop_runtime(&mut runtime).await;
                        if result.is_ok() {
                            runtime.status.last_error = None;
                        }
                        publish(&runtime.status, &status_tx, &ui_event_bus);
                        let _ = response.send(result);
                    }
                }
            }
            _ = check.tick() => {
                if let Some(spec) = runtime.serving.clone()
                    && let Err(error) = check_serving(&spec).await
                {
                    runtime.serving = None;
                    runtime.status.state = RemoteAccessState::Error;
                    runtime.status.last_error = Some(error.clone());
                    push_log(&mut runtime.status.recent_logs, &error);
                    publish(&runtime.status, &status_tx, &ui_event_bus);
                }
            }
        }
    }
}

async fn sync_runtime(
    runtime: &mut RemoteAccessRuntime,
    config: RemoteAccessConfig,
    has_local_profile: bool,
    local_addr: Option<SocketAddr>,
) -> Result<(), String> {
    runtime.status.enabled = config.enabled;
    if !config.enabled {
        stop_runtime(runtime).await?;
        runtime.status.last_error = None;
        return Ok(());
    }

    let spec = serve_spec(&config, has_local_profile, local_addr)?;
    if runtime.serving.as_ref() == Some(&spec) && check_serving(&spec).await.is_ok() {
        runtime.status.state = RemoteAccessState::Running;
        runtime.status.last_error = None;
        return Ok(());
    }

    stop_runtime(runtime).await?;
    runtime.status.enabled = true;
    runtime.status.state = RemoteAccessState::Starting;
    runtime.status.last_error = None;

    let https = https_flag(spec.https_port);
    let output = tailscale(
        &spec.tailscale_binary,
        vec!["serve".into(), "--bg".into(), https, spec.target.clone()],
    )
    .await?;
    push_log(&mut runtime.status.recent_logs, &output);
    runtime.serving = Some(spec.clone());

    let host = tailnet_host(&spec.tailscale_binary).await?;
    let url = public_url(&host, spec.https_port);
    push_log(
        &mut runtime.status.recent_logs,
        &format!("serving {} at {url}", spec.target),
    );
    runtime.status.public_url = Some(url);
    runtime.status.state = RemoteAccessState::Running;
    Ok(())
}

async fn stop_runtime(runtime: &mut RemoteAccessRuntime) -> Result<(), String> {
    if let Some(spec) = runtime.serving.take() {
        let https = https_flag(spec.https_port);
        let output = tailscale(
            &spec.tailscale_binary,
            vec!["serve".into(), https, "off".into()],
        )
        .await
        .inspect_err(|_| runtime.serving = Some(spec.clone()))?;
        push_log(&mut runtime.status.recent_logs, &output);
    }
    runtime.status.public_url = None;
    runtime.status.state = RemoteAccessState::Disabled;
    Ok(())
}

fn serve_spec(
    config: &RemoteAccessConfig,
    has_local_profile: bool,
    local_addr: Option<SocketAddr>,
) -> Result<ServeSpec, String> {
    if has_local_profile && !config.allow_local_environment {
        return Err(
            "remote access is blocked while local-environment profiles exist; enable allow_local_environment to override"
                .to_string(),
        );
    }
    let port = local_addr
        .map(|addr| addr.port())
        .ok_or_else(|| "the May daemon is not listening on TCP yet".to_string())?;
    let binary = config.tailscale_binary.trim();
    Ok(ServeSpec {
        tailscale_binary: if binary.is_empty() {
            crate::config::DEFAULT_TAILSCALE_BINARY.to_string()
        } else {
            binary.to_string()
        },
        https_port: config.https_port,
        target: format!("http://127.0.0.1:{port}"),
    })
}

fn https_flag(port: u16) -> String {
    format!("--https={port}")
}

fn public_url(host: &str, https_port: u16) -> String {
    if https_port == 443 {
        format!("https://{host}")
    } else {
        format!("https://{host}:{https_port}")
    }
}

/// Runs `tailscale <args>` and returns its trimmed output, or the reason it
/// failed.
async fn tailscale(binary: &str, args: Vec<String>) -> Result<String, String> {
    let binary = binary.to_string();
    let output = blocking::unblock({
        let binary = binary.clone();
        move || std::process::Command::new(binary).args(args).output()
    })
    .await
    .map_err(|error| format!("failed to run {binary}: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("{binary} exited with {}", output.status)
    } else {
        stderr
    })
}

#[derive(Debug, Deserialize)]
struct TailscaleStatus {
    #[serde(rename = "BackendState", default)]
    backend_state: String,
    #[serde(rename = "Self")]
    self_node: Option<TailscaleNode>,
}

#[derive(Debug, Deserialize)]
struct TailscaleNode {
    #[serde(rename = "DNSName", default)]
    dns_name: String,
}

/// The machine's `MagicDNS` name, from `tailscale status --json`.
async fn tailnet_host(binary: &str) -> Result<String, String> {
    let output = tailscale(binary, vec!["status".into(), "--json".into()]).await?;
    parse_tailnet_host(&output)
}

fn parse_tailnet_host(output: &str) -> Result<String, String> {
    let status: TailscaleStatus = serde_json::from_str(output)
        .map_err(|error| format!("unexpected `tailscale status` output: {error}"))?;
    if status.backend_state != "Running" {
        return Err(format!(
            "Tailscale is not connected (state: {})",
            status.backend_state
        ));
    }
    let host = status
        .self_node
        .map(|node| node.dns_name.trim_end_matches('.').to_string())
        .unwrap_or_default();
    if host.is_empty() {
        return Err("this machine has no MagicDNS name; enable MagicDNS for the tailnet".into());
    }
    Ok(host)
}

#[derive(Debug, Default, Deserialize)]
struct ServeStatus {
    #[serde(rename = "Web", default)]
    web: std::collections::BTreeMap<String, ServeHost>,
}

#[derive(Debug, Default, Deserialize)]
struct ServeHost {
    #[serde(rename = "Handlers", default)]
    handlers: std::collections::BTreeMap<String, ServeHandler>,
}

#[derive(Debug, Default, Deserialize)]
struct ServeHandler {
    #[serde(rename = "Proxy", default)]
    proxy: String,
}

/// Checks that Tailscale still serves `spec`; someone may have reset it.
async fn check_serving(spec: &ServeSpec) -> Result<(), String> {
    let output = tailscale(
        &spec.tailscale_binary,
        vec!["serve".into(), "status".into(), "--json".into()],
    )
    .await?;
    if serves(&output, spec) {
        Ok(())
    } else {
        Err("the Tailscale serve configuration no longer forwards to May".to_string())
    }
}

fn serves(status_json: &str, spec: &ServeSpec) -> bool {
    let status: ServeStatus = serde_json::from_str(status_json).unwrap_or_default();
    let suffix = format!(":{}", spec.https_port);
    status.web.iter().any(|(host, served)| {
        host.ends_with(&suffix)
            && served
                .handlers
                .get("/")
                .is_some_and(|handler| handler.proxy.trim_end_matches('/') == spec.target)
    })
}

fn push_log(logs: &mut Vec<String>, line: &str) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let mut deque = VecDeque::from(std::mem::take(logs));
    for line in trimmed.lines() {
        deque.push_back(line.to_string());
    }
    while deque.len() > MAX_REMOTE_ACCESS_LOGS {
        let _ = deque.pop_front();
    }
    *logs = deque.into_iter().collect();
}

fn publish(
    status: &RemoteAccessStatusResponse,
    status_tx: &watch::Sender<RemoteAccessStatusResponse>,
    ui_event_bus: &UiEventBus,
) {
    status_tx.send_replace(status.clone());
    ui_event_bus.publish(crate::ui_event_bus::UiEventTopics::REMOTE_ACCESS);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> RemoteAccessConfig {
        RemoteAccessConfig {
            enabled: true,
            ..RemoteAccessConfig::default()
        }
    }

    const ADDR: Option<SocketAddr> = Some(SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        8787,
    ));

    #[test]
    fn rejects_local_environment_without_explicit_override() {
        let error = serve_spec(&base_config(), true, ADDR)
            .expect_err("local profiles should be blocked by default");
        assert!(error.contains("allow_local_environment"));
    }

    #[test]
    fn forwards_to_the_daemon_port_on_loopback() {
        let spec = serve_spec(
            &base_config(),
            false,
            Some(SocketAddr::from(([0, 0, 0, 0], 9000))),
        )
        .expect("valid spec");
        assert_eq!(spec.target, "http://127.0.0.1:9000");
        assert_eq!(spec.https_port, 443);
        assert_eq!(spec.tailscale_binary, "tailscale");
    }

    #[test]
    fn needs_the_daemon_listening() {
        let error = serve_spec(&base_config(), false, None).expect_err("no listener yet");
        assert!(error.contains("not listening"));
    }

    #[test]
    fn reads_the_magicdns_name() {
        let output = r#"{"BackendState":"Running","Self":{"DNSName":"mac.tail1234.ts.net."}}"#;
        assert_eq!(
            parse_tailnet_host(output).as_deref(),
            Ok("mac.tail1234.ts.net")
        );
        let stopped = r#"{"BackendState":"Stopped","Self":{"DNSName":""}}"#;
        assert!(parse_tailnet_host(stopped).is_err());
    }

    #[test]
    fn public_url_omits_the_default_port() {
        assert_eq!(public_url("mac.ts.net", 443), "https://mac.ts.net");
        assert_eq!(public_url("mac.ts.net", 8443), "https://mac.ts.net:8443");
    }

    #[test]
    fn recognizes_its_handler_in_serve_status() {
        let spec = serve_spec(&base_config(), false, ADDR).expect("valid spec");
        let status = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"mac.tail1234.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8787"}}}}}"#;
        assert!(serves(status, &spec));
        let other = r#"{"Web":{"mac.tail1234.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3000"}}}}}"#;
        assert!(!serves(other, &spec));
        assert!(!serves("{}", &spec));
    }
}
