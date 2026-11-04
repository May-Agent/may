//! Remote access over Tailscale.
//!
//! When enabled, the daemon asks the local `tailscale` to serve its loopback
//! TCP listener on the tailnet over HTTPS (`tailscale serve --bg`), so every
//! device on the user's tailnet reaches the web UI and API at the machine's
//! `MagicDNS` name. Tailscale holds the configuration itself; the manager only
//! adds and removes the one handler it owns and checks that it is still in
//! place.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::RemoteAccessConfig;
use crate::db::ChatDb;
use crate::ui_event_bus::UiEventBus;

use super::auth::TailnetLogin;
use super::error::{bad_request_error, internal_error};
use super::state::DaemonState;
use super::types::{
    AcceptedLogins, ApiHealth, OwnerLogin, RemoteAccessState, RemoteAccessStatusResponse,
};

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

/// Funnel ports Tailscale allows.
const FUNNEL_PORTS: [u16; 3] = [443, 8443, 10000];

/// The `tailscale funnel` handler: the webhook mount on a Funnel port,
/// forwarding to the public listener.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FunnelSpec {
    tailscale_binary: String,
    port: u16,
}

/// The public listener behind Funnel, serving the webhook router alone. It
/// stops listening when dropped.
#[derive(Debug)]
struct PublicListener {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl PublicListener {
    async fn bind(router: Arc<skyzen::routing::Router>) -> Result<Self, String> {
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .map_err(|error| format!("failed to bind the public webhook listener: {error}"))?;
        let addr = listener
            .local_addr()
            .map_err(|error| format!("the public webhook listener has no address: {error}"))?;
        let task = tokio::spawn(super::serve::serve_tcp(listener, router));
        Ok(Self { addr, task })
    }

    /// Proxy target, `http://127.0.0.1:<port>`.
    fn target(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }
}

impl Drop for PublicListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Funnel as this daemon configured it.
#[derive(Debug)]
struct FunnelRuntime {
    spec: FunnelSpec,
    listener: PublicListener,
}

#[derive(Debug)]
struct RemoteAccessRuntime {
    status: RemoteAccessStatusResponse,
    /// The handler this daemon added, while it is in place.
    serving: Option<ServeSpec>,
    /// The Funnel handler and public listener this daemon added.
    funnel: Option<FunnelRuntime>,
}

impl Default for RemoteAccessRuntime {
    fn default() -> Self {
        Self {
            status: RemoteAccessStatusResponse {
                state: RemoteAccessState::Disabled,
                enabled: false,
                public_url: None,
                funnel_url: None,
                last_error: None,
                recent_logs: Vec::new(),
                accepted: None,
            },
            serving: None,
            funnel: None,
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
    /// The daemon's remote TCP address, once it listens.
    local_addr: watch::Sender<Option<SocketAddr>>,
    /// The router of the public Funnel listener, once built.
    webhook_router: watch::Sender<Option<Arc<skyzen::routing::Router>>>,
    /// Accepts `Tailscale-User-Login` identities the tailnet is allowed to use;
    /// the actor keeps its allow-list current.
    tailscale_identity: super::auth::TailscaleServeIdentitySource,
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
        let (webhook_router, webhook_router_rx) = watch::channel(None);
        let tailscale_identity =
            super::auth::TailscaleServeIdentitySource::new(std::collections::HashSet::default());
        tokio::spawn(run_remote_access_actor(
            command_rx,
            status_tx,
            local_addr_rx,
            webhook_router_rx,
            ui_event_bus,
            tailscale_identity.clone(),
        ));
        Self {
            command_tx,
            status_rx,
            local_addr,
            webhook_router,
            tailscale_identity,
        }
    }

    /// Records the router of the public Funnel listener, which serves the
    /// webhook routes alone.
    pub(crate) fn set_webhook_router(&self, router: Arc<skyzen::routing::Router>) {
        self.webhook_router.send_replace(Some(router));
    }

    /// The identity source the remote listener's Tailcheck layer accepts.
    pub(crate) fn tailscale_identity(&self) -> super::auth::TailscaleServeIdentitySource {
        self.tailscale_identity.clone()
    }

    /// Records the TCP address the daemon serves on; remote access forwards
    /// to its port on loopback.
    pub(crate) fn set_local_addr(&self, addr: SocketAddr) {
        self.local_addr.send_replace(Some(addr));
    }

    /// The daemon's TCP address, once it listens.
    pub(crate) fn local_addr(&self) -> Option<SocketAddr> {
        *self.local_addr.borrow()
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
    state: &DaemonState,
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
    skyzen::utils::State(state): skyzen::utils::State<Arc<DaemonState>>,
) -> skyzen::Result<skyzen::utils::Json<RemoteAccessStatusResponse>> {
    Ok(skyzen::utils::Json(state.remote_access_manager.snapshot()))
}

pub(super) async fn start_remote_access(
    skyzen::utils::State(state): skyzen::utils::State<Arc<DaemonState>>,
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
    skyzen::utils::State(state): skyzen::utils::State<Arc<DaemonState>>,
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
    webhook_router: watch::Receiver<Option<Arc<skyzen::routing::Router>>>,
    ui_event_bus: UiEventBus,
    tailscale_identity: super::auth::TailscaleServeIdentitySource,
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
                        runtime.status.accepted =
                            Some(refresh_allow_list(&tailscale_identity, &config).await);
                        let router = webhook_router.borrow().clone();
                        let serve_result =
                            sync_runtime(&mut runtime, config.clone(), has_local_profile, addr).await;
                        let funnel_result =
                            sync_funnel(&mut runtime, &config, has_local_profile, router).await;
                        let result = serve_result.and(funnel_result);
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
                        let result = result.and(stop_funnel(&mut runtime).await);
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
                if let Some(funnel) = &runtime.funnel
                    && let Err(error) = check_funnel(&funnel.spec, &funnel.listener.target()).await
                {
                    // Dropping the runtime stops the public listener; the
                    // next sync sets Funnel up again.
                    runtime.funnel = None;
                    runtime.status.funnel_url = None;
                    runtime.status.state = RemoteAccessState::Error;
                    runtime.status.last_error = Some(error.clone());
                    push_log(&mut runtime.status.recent_logs, &error);
                    publish(&runtime.status, &status_tx, &ui_event_bus);
                }
            }
        }
    }
}

/// Reconciles Funnel with `config`: the public listener and the
/// `tailscale funnel` handler exist exactly while `config.funnel` is on.
async fn sync_funnel(
    runtime: &mut RemoteAccessRuntime,
    config: &RemoteAccessConfig,
    has_local_profile: bool,
    router: Option<Arc<skyzen::routing::Router>>,
) -> Result<(), String> {
    if !config.funnel {
        return stop_funnel(runtime).await;
    }
    let spec = match funnel_spec(config, has_local_profile) {
        Ok(spec) => spec,
        Err(error) => {
            stop_funnel(runtime).await?;
            return Err(error);
        }
    };
    if let Some(funnel) = &runtime.funnel
        && funnel.spec == spec
        && check_funnel(&spec, &funnel.listener.target()).await.is_ok()
    {
        return Ok(());
    }

    stop_funnel(runtime).await?;
    let router = router.ok_or_else(|| "the public webhook router is not ready yet".to_string())?;
    let listener = PublicListener::bind(router).await?;
    let target = listener.target();
    let output = tailscale(
        &spec.tailscale_binary,
        vec![
            "funnel".into(),
            "--bg".into(),
            https_flag(spec.port),
            set_path_flag(),
            target.clone(),
        ],
    )
    .await?;
    push_log(&mut runtime.status.recent_logs, &output);
    runtime.funnel = Some(FunnelRuntime {
        spec: spec.clone(),
        listener,
    });

    let host = tailnet_host(&spec.tailscale_binary).await?;
    let url = public_url(&host, spec.port);
    push_log(
        &mut runtime.status.recent_logs,
        &format!(
            "funnel exposes {} at {url} via {target}",
            crate::task_trigger::WEBHOOK_MOUNT
        ),
    );
    runtime.status.funnel_url = Some(url);
    Ok(())
}

/// Removes the Funnel handler, then stops the public listener.
async fn stop_funnel(runtime: &mut RemoteAccessRuntime) -> Result<(), String> {
    if let Some(funnel) = runtime.funnel.take() {
        let output = tailscale(
            &funnel.spec.tailscale_binary,
            vec![
                "funnel".into(),
                https_flag(funnel.spec.port),
                set_path_flag(),
                "off".into(),
            ],
        )
        .await;
        match output {
            Ok(output) => push_log(&mut runtime.status.recent_logs, &output),
            Err(error) => {
                // Funnel is still on: keep the listener so deliveries work
                // and the removal is retried.
                runtime.funnel = Some(funnel);
                return Err(error);
            }
        }
    }
    runtime.status.funnel_url = None;
    Ok(())
}

fn funnel_spec(config: &RemoteAccessConfig, has_local_profile: bool) -> Result<FunnelSpec, String> {
    if has_local_profile && !config.allow_local_environment {
        return Err(
            "Funnel is blocked while local-environment profiles exist; enable allow_local_environment to override"
                .to_string(),
        );
    }
    if !FUNNEL_PORTS.contains(&config.funnel_port) {
        return Err(format!(
            "Funnel port {} is not allowed; use 443, 8443 or 10000",
            config.funnel_port
        ));
    }
    if config.enabled && config.funnel_port == config.https_port {
        return Err(format!(
            "Funnel port {} is also the tailnet serve port; Funnel would expose every route on it, so choose another",
            config.funnel_port
        ));
    }
    Ok(FunnelSpec {
        tailscale_binary: tailscale_binary(config),
        port: config.funnel_port,
    })
}

fn set_path_flag() -> String {
    format!("--set-path={}", crate::task_trigger::WEBHOOK_MOUNT)
}

/// Checks that Tailscale still funnels the webhook mount to `target`.
async fn check_funnel(spec: &FunnelSpec, target: &str) -> Result<(), String> {
    let output = tailscale(
        &spec.tailscale_binary,
        vec!["serve".into(), "status".into(), "--json".into()],
    )
    .await?;
    if funnels(&output, spec, target) {
        Ok(())
    } else {
        Err(
            "the Tailscale funnel configuration no longer forwards the webhook routes to May"
                .to_string(),
        )
    }
}

fn funnels(status_json: &str, spec: &FunnelSpec, target: &str) -> bool {
    let status: ServeStatus = serde_json::from_str(status_json).unwrap_or_default();
    let suffix = format!(":{}", spec.port);
    status.web.iter().any(|(host, served)| {
        host.ends_with(&suffix)
            && status.allow_funnel.get(host).copied().unwrap_or(false)
            && served
                .handlers
                .get(crate::task_trigger::WEBHOOK_MOUNT)
                .is_some_and(|handler| handler.proxy.trim_end_matches('/') == target)
    })
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
    Ok(ServeSpec {
        tailscale_binary: tailscale_binary(config),
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

/// The configured `tailscale` binary, or the default when none is set.
fn tailscale_binary(config: &RemoteAccessConfig) -> String {
    let binary = config.tailscale_binary.trim();
    if binary.is_empty() {
        crate::config::DEFAULT_TAILSCALE_BINARY.to_string()
    } else {
        binary.to_string()
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
    #[serde(rename = "User", default)]
    user: std::collections::BTreeMap<u64, TailscaleUser>,
}

#[derive(Debug, Deserialize)]
struct TailscaleNode {
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(rename = "UserID", default)]
    user_id: u64,
}

/// `tailscale whois --json`: the node a tailnet address belongs to and the
/// user owning it.
#[derive(Debug, Deserialize)]
struct TailscaleWhois {
    #[serde(rename = "Node")]
    node: WhoisNode,
    #[serde(rename = "UserProfile")]
    user_profile: Option<WhoisUserProfile>,
}

#[derive(Debug, Deserialize)]
struct WhoisNode {
    /// Present only on tagged nodes, which have no user identity.
    #[serde(rename = "Tags", default)]
    tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct WhoisUserProfile {
    #[serde(rename = "LoginName", default)]
    login_name: String,
}

/// The user login in `tailscale whois --json` output; a tagged node has none.
pub(super) fn parse_whois_login(output: &str) -> Result<Option<TailnetLogin>, String> {
    let whois: TailscaleWhois = serde_json::from_str(output)
        .map_err(|error| format!("unexpected `tailscale whois` output: {error}"))?;
    if !whois.node.tags.is_empty() {
        return Ok(None);
    }
    Ok(whois
        .user_profile
        .map(|profile| profile.login_name)
        .filter(|login| !login.is_empty())
        .map(TailnetLogin))
}

/// Resolves a peer address to its user login through the CLI's `WhoIs`
/// (`tailscale whois --json <ip>`). A failure or a peer without a user
/// identity yields `None`, so the connection is never attributed.
pub(super) async fn whois_login(binary: &str, peer: IpAddr) -> Option<TailnetLogin> {
    let args = vec!["whois".into(), "--json".into(), peer.to_string()];
    let output = match tailscale(binary, args).await {
        Ok(output) => output,
        Err(error) => {
            tracing::warn!(%peer, %error, "tailnet WhoIs failed; peer rejected");
            return None;
        }
    };
    match parse_whois_login(&output) {
        Ok(Some(login)) => Some(login),
        Ok(None) => {
            tracing::warn!(%peer, "tailnet peer is a tagged node without a user; rejected");
            None
        }
        Err(error) => {
            tracing::warn!(%peer, %error, "tailnet WhoIs unreadable; peer rejected");
            None
        }
    }
}

/// The login of a direct-bind connection's peer, using the `tailscale`
/// binary configured now.
pub(super) async fn resolve_tailnet_login(
    state: &DaemonState,
    peer: IpAddr,
) -> Option<TailnetLogin> {
    let binary = tailscale_binary(&state.config_snapshot().remote_access);
    whois_login(&binary, peer.to_canonical()).await
}

#[derive(Debug, Default, Deserialize)]
struct TailscaleUser {
    #[serde(rename = "LoginName", default)]
    login_name: String,
}

/// The machine's `MagicDNS` name, from `tailscale status --json`.
async fn tailnet_host(binary: &str) -> Result<String, String> {
    let output = tailscale(binary, vec!["status".into(), "--json".into()]).await?;
    parse_tailnet_host(&output)
}

/// The logins the Tailscale identity sources accept: the login owning this
/// node plus the configured allow-list.
async fn refresh_allow_list(
    identity: &super::auth::TailscaleServeIdentitySource,
    config: &RemoteAccessConfig,
) -> AcceptedLogins {
    let accepted = accepted_logins(
        node_owner_login(&tailscale_binary(config)).await,
        &config.allowed_logins,
    );
    identity.set_allowed(accepted.allow_list());
    accepted
}

/// The node owner (when resolved) together with the listed logins. An owner
/// that cannot be resolved is warned about and leaves only the listed logins.
fn accepted_logins(owner: Result<String, String>, listed: &[String]) -> AcceptedLogins {
    let owner = match owner {
        Ok(login) => OwnerLogin::Resolved { login },
        Err(reason) => {
            tracing::warn!(%reason, "Tailscale node owner unresolved; only listed logins accepted");
            OwnerLogin::Unresolved { reason }
        }
    };
    AcceptedLogins {
        owner,
        listed: listed.to_vec(),
    }
}

/// The login that owns this node: `tailscale status --json`'s `Self.UserID`
/// looked up in the `User` map.
async fn node_owner_login(binary: &str) -> Result<String, String> {
    let output = tailscale(binary, vec!["status".into(), "--json".into()]).await?;
    let status: TailscaleStatus = serde_json::from_str(&output)
        .map_err(|error| format!("unexpected `tailscale status` output: {error}"))?;
    if status.backend_state != "Running" {
        return Err(format!(
            "Tailscale is not connected (state: {})",
            status.backend_state
        ));
    }
    let user_id = status
        .self_node
        .map(|node| node.user_id)
        .filter(|&id| id != 0)
        .ok_or_else(|| "this node reports no owner".to_string())?;
    status
        .user
        .get(&user_id)
        .map(|user| user.login_name.clone())
        .filter(|login| !login.is_empty())
        .ok_or_else(|| "this node's owner has no login name".to_string())
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
    /// `host:port` entries Funnel exposes to the internet.
    #[serde(rename = "AllowFunnel", default)]
    allow_funnel: std::collections::BTreeMap<String, bool>,
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

    fn set(logins: &[&str]) -> std::collections::HashSet<String> {
        logins.iter().map(ToString::to_string).collect()
    }

    fn listed() -> Vec<String> {
        vec!["a@example.com".into(), "b@example.com".into()]
    }

    #[test]
    fn owner_and_list_are_united() {
        let accepted = accepted_logins(Ok("me@example.com".into()), &listed());
        assert_eq!(
            accepted.allow_list(),
            set(&["me@example.com", "a@example.com", "b@example.com"])
        );
    }

    #[test]
    fn owner_alone_when_nothing_is_listed() {
        let accepted = accepted_logins(Ok("me@example.com".into()), &[]);
        assert_eq!(accepted.allow_list(), set(&["me@example.com"]));
    }

    #[test]
    fn unresolved_owner_leaves_only_the_list() {
        let accepted = accepted_logins(Err("not connected".into()), &listed());
        assert_eq!(
            accepted.allow_list(),
            set(&["a@example.com", "b@example.com"])
        );
        assert_eq!(
            accepted.owner,
            OwnerLogin::Unresolved {
                reason: "not connected".into()
            }
        );
    }

    #[test]
    fn unresolved_owner_and_empty_list_accept_nobody() {
        let accepted = accepted_logins(Err("not connected".into()), &[]);
        assert!(accepted.allow_list().is_empty());
        let json = serde_json::to_value(&accepted).expect("serializes");
        assert_eq!(json["owner"]["state"], "unresolved");
        assert_eq!(json["owner"]["reason"], "not connected");
        assert_eq!(json["listed"], serde_json::json!([]));
    }

    #[test]
    fn refresh_applies_the_union_to_the_identity_source() {
        let identity = super::super::auth::TailscaleServeIdentitySource::new(set(&[]));
        let config = RemoteAccessConfig {
            allowed_logins: listed(),
            // No such binary: the owner stays unresolved, the list still applies.
            tailscale_binary: "/nonexistent/tailscale".into(),
            ..RemoteAccessConfig::default()
        };
        let accepted = futures_lite::future::block_on(refresh_allow_list(&identity, &config));
        assert!(matches!(accepted.owner, OwnerLogin::Unresolved { .. }));
        assert_eq!(accepted.listed, listed());
    }

    fn funnel_config(port: u16) -> RemoteAccessConfig {
        RemoteAccessConfig {
            funnel: true,
            funnel_port: port,
            ..RemoteAccessConfig::default()
        }
    }

    #[test]
    fn funnel_accepts_only_the_ports_tailscale_allows() {
        for port in [443, 8443, 10000] {
            assert_eq!(
                funnel_spec(&funnel_config(port), false)
                    .expect("allowed")
                    .port,
                port
            );
        }
        let error = funnel_spec(&funnel_config(9000), false).expect_err("not a funnel port");
        assert!(error.contains("443, 8443 or 10000"));
    }

    #[test]
    fn funnel_never_shares_the_serve_port() {
        let mut config = funnel_config(443);
        config.enabled = true;
        let error = funnel_spec(&config, false).expect_err("same port as serve");
        assert!(error.contains("serve port"));
        config.funnel_port = 8443;
        assert!(funnel_spec(&config, false).is_ok());
    }

    #[test]
    fn funnel_is_blocked_with_local_profiles_unless_overridden() {
        let mut config = funnel_config(443);
        assert!(funnel_spec(&config, true).is_err());
        config.allow_local_environment = true;
        assert!(funnel_spec(&config, true).is_ok());
    }

    #[test]
    fn recognizes_its_funnel_handler_in_serve_status() {
        let spec = funnel_spec(&funnel_config(8443), false).expect("valid spec");
        let target = "http://127.0.0.1:5555";
        let on = r#"{"Web":{"mac.ts.net:8443":{"Handlers":{"/api/tasks/webhook":{"Proxy":"http://127.0.0.1:5555"}}}},"AllowFunnel":{"mac.ts.net:8443":true}}"#;
        assert!(funnels(on, &spec, target));
        // The handler without the Funnel flag is tailnet-only.
        let private = r#"{"Web":{"mac.ts.net:8443":{"Handlers":{"/api/tasks/webhook":{"Proxy":"http://127.0.0.1:5555"}}}}}"#;
        assert!(!funnels(private, &spec, target));
        let other_target = r#"{"Web":{"mac.ts.net:8443":{"Handlers":{"/api/tasks/webhook":{"Proxy":"http://127.0.0.1:1"}}}},"AllowFunnel":{"mac.ts.net:8443":true}}"#;
        assert!(!funnels(other_target, &spec, target));
        assert!(!funnels("{}", &spec, target));
    }
}
