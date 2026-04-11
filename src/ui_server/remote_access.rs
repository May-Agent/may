use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use std::str::FromStr;
use tokio::io::{AsyncBufReadExt, BufReader};

use tokio::sync::{mpsc, oneshot, watch};

use crate::config::RemoteAccessConfig;
use crate::db::ChatDb;
use crate::ui_event_bus::UiEventBus;

use super::error::{bad_request_error, internal_error};
use super::state::UiServerState;
use super::types::{ApiHealth, RemoteAccessState, RemoteAccessStatusResponse};

const MAX_REMOTE_ACCESS_LOGS: usize = 48;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteAccessLaunchSpec {
    cloudflared_binary: String,
    tunnel_token: String,
    public_url: String,
}

#[derive(Debug)]
struct RemoteAccessRuntime {
    status: RemoteAccessStatusResponse,
    launch_spec: Option<RemoteAccessLaunchSpec>,
    last_stderr_line: Option<String>,
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
            launch_spec: None,
            last_stderr_line: None,
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

enum RemoteAccessEvent {
    Stdout(String),
    Stderr(String),
    ReaderError(String),
}

#[derive(Clone)]
pub struct RemoteAccessManager {
    command_tx: mpsc::Sender<RemoteAccessCommand>,
    status_rx: watch::Receiver<RemoteAccessStatusResponse>,
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
        tokio::spawn(run_remote_access_actor(command_rx, status_tx, ui_event_bus));
        Self {
            command_tx,
            status_rx,
        }
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
    ui_event_bus: UiEventBus,
) {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mut runtime = RemoteAccessRuntime::default();
    let mut child: Option<tokio::process::Child> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(500));

    loop {
        tokio::select! {
            Some(command) = command_rx.recv() => {
                match command {
                    RemoteAccessCommand::Sync { config, has_local_profile, response } => {
                        let result = sync_remote_access_runtime(
                            &mut runtime,
                            &mut child,
                            &event_tx,
                            &status_tx,
                            &ui_event_bus,
                            config,
                            has_local_profile,
                        ).await;
                        let _ = response.send(result);
                    }
                    RemoteAccessCommand::Stop { response } => {
                        let result = stop_remote_access_runtime(
                            &mut runtime,
                            &mut child,
                            &status_tx,
                            &ui_event_bus,
                        ).await;
                        let _ = response.send(result);
                    }
                }
            }
            Some(event) = event_rx.recv() => {
                match event {
                    RemoteAccessEvent::Stdout(line) => {
                        push_log(&mut runtime.status.recent_logs, line);
                    }
                    RemoteAccessEvent::Stderr(line) => {
                        runtime.last_stderr_line = Some(line.clone());
                        push_log(&mut runtime.status.recent_logs, line);
                    }
                    RemoteAccessEvent::ReaderError(message) => {
                        runtime.status.last_error = Some(message.clone());
                        push_log(&mut runtime.status.recent_logs, message);
                    }
                }
                publish_remote_access_status(&runtime.status, &status_tx, &ui_event_bus);
            }
            _ = tick.tick() => {
                if let Some(process) = child.as_mut() {
                    match process.try_wait() {
                        Ok(Some(status)) => {
                            let success = status.success();
                            let code = status.code().unwrap_or(-1);
                            runtime.launch_spec = None;
                            child = None;
                            runtime.status.state = if runtime.status.enabled {
                                RemoteAccessState::Error
                            } else {
                                RemoteAccessState::Disabled
                            };
                            runtime.status.last_error = Some(if success {
                                "cloudflared exited unexpectedly".to_string()
                            } else {
                                runtime.last_stderr_line.clone().unwrap_or_else(|| {
                                    format!("cloudflared exited with code {code}")
                                })
                            });
                            publish_remote_access_status(&runtime.status, &status_tx, &ui_event_bus);
                        }
                        Ok(None) => {}
                        Err(error) => {
                            runtime.launch_spec = None;
                            child = None;
                            runtime.status.state = RemoteAccessState::Error;
                            runtime.status.last_error = Some(error.to_string());
                            publish_remote_access_status(&runtime.status, &status_tx, &ui_event_bus);
                        }
                    }
                }
            }
            else => break,
        }
    }
}

async fn sync_remote_access_runtime(
    runtime: &mut RemoteAccessRuntime,
    child: &mut Option<tokio::process::Child>,
    event_tx: &mpsc::UnboundedSender<RemoteAccessEvent>,
    status_tx: &watch::Sender<RemoteAccessStatusResponse>,
    ui_event_bus: &UiEventBus,
    config: RemoteAccessConfig,
    has_local_profile: bool,
) -> Result<(), String> {
    runtime.status.enabled = config.enabled;
    runtime.status.public_url = config.public_url.clone();

    if !config.enabled {
        stop_remote_access_runtime(runtime, child, status_tx, ui_event_bus).await?;
        runtime.status.enabled = false;
        runtime.status.state = RemoteAccessState::Disabled;
        runtime.status.last_error = None;
        runtime.last_stderr_line = None;
        publish_remote_access_status(&runtime.status, status_tx, ui_event_bus);
        return Ok(());
    }

    let launch_spec = validate_remote_access_config(config, has_local_profile)?;
    if child.is_some()
        && runtime
            .launch_spec
            .as_ref()
            .is_some_and(|current| current == &launch_spec)
    {
        runtime.status.state = RemoteAccessState::Running;
        runtime.status.last_error = None;
        runtime.last_stderr_line = None;
        publish_remote_access_status(&runtime.status, status_tx, ui_event_bus);
        return Ok(());
    }

    stop_remote_access_runtime(runtime, child, status_tx, ui_event_bus).await?;
    runtime.status.enabled = true;
    runtime.status.public_url = Some(launch_spec.public_url.clone());
    runtime.status.state = RemoteAccessState::Starting;
    runtime.status.last_error = None;
    runtime.last_stderr_line = None;
    publish_remote_access_status(&runtime.status, status_tx, ui_event_bus);

    let mut process = tokio::process::Command::new(launch_spec.cloudflared_binary.as_str())
        .arg("tunnel")
        .arg("--no-autoupdate")
        .arg("run")
        .arg("--token")
        .arg(launch_spec.tunnel_token.as_str())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to launch cloudflared: {error}"))?;

    if let Some(stdout) = process.stdout.take() {
        spawn_remote_access_log_reader(stdout, event_tx.clone(), false);
    }
    if let Some(stderr) = process.stderr.take() {
        spawn_remote_access_log_reader(stderr, event_tx.clone(), true);
    }

    push_log(
        &mut runtime.status.recent_logs,
        format!("starting tunnel at {}", launch_spec.public_url),
    );
    runtime.status.state = RemoteAccessState::Running;
    runtime.status.last_error = None;
    runtime.launch_spec = Some(launch_spec);
    *child = Some(process);
    publish_remote_access_status(&runtime.status, status_tx, ui_event_bus);
    Ok(())
}

async fn stop_remote_access_runtime(
    runtime: &mut RemoteAccessRuntime,
    child: &mut Option<tokio::process::Child>,
    status_tx: &watch::Sender<RemoteAccessStatusResponse>,
    ui_event_bus: &UiEventBus,
) -> Result<(), String> {
    if let Some(mut process) = child.take() {
        if let Err(error) = process.start_kill() {
            return Err(format!("failed to stop cloudflared: {error}"));
        }
        let _ = process.wait().await;
    }
    runtime.launch_spec = None;
    runtime.last_stderr_line = None;
    runtime.status.state = if runtime.status.enabled {
        RemoteAccessState::Error
    } else {
        RemoteAccessState::Disabled
    };
    publish_remote_access_status(&runtime.status, status_tx, ui_event_bus);
    Ok(())
}

fn validate_remote_access_config(
    config: RemoteAccessConfig,
    has_local_profile: bool,
) -> Result<RemoteAccessLaunchSpec, String> {
    if has_local_profile && !config.allow_local_environment {
        return Err(
            "remote access is blocked while local-environment profiles exist; enable allow_local_environment to override"
                .to_string(),
        );
    }

    let token = config
        .tunnel_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "remote access requires a Cloudflare tunnel token".to_string())?;
    let public_url = config
        .public_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "remote access requires a public HTTPS URL".to_string())?;
    if !public_url.starts_with("https://") {
        return Err("remote access public_url must start with https://".to_string());
    }

    Ok(RemoteAccessLaunchSpec {
        cloudflared_binary: config.cloudflared_binary.trim().to_string(),
        tunnel_token: token.to_string(),
        public_url: public_url.to_string(),
    })
}

fn spawn_remote_access_log_reader(
    reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    event_tx: mpsc::UnboundedSender<RemoteAccessEvent>,
    stderr: bool,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let payload = if stderr {
                        RemoteAccessEvent::Stderr(line)
                    } else {
                        RemoteAccessEvent::Stdout(line)
                    };
                    if event_tx.send(payload).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = event_tx.send(RemoteAccessEvent::ReaderError(error.to_string()));
                    break;
                }
            }
        }
    });
}

fn push_log(logs: &mut Vec<String>, line: String) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let mut deque = VecDeque::from(std::mem::take(logs));
    deque.push_back(trimmed.to_string());
    while deque.len() > MAX_REMOTE_ACCESS_LOGS {
        let _ = deque.pop_front();
    }
    *logs = deque.into_iter().collect();
}

fn publish_remote_access_status(
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
            cloudflared_binary: "cloudflared".to_string(),
            tunnel_token: Some("token".to_string()),
            public_url: Some("https://may.example.com".to_string()),
            allow_local_environment: false,
        }
    }

    #[test]
    fn rejects_local_environment_without_explicit_override() {
        let error = validate_remote_access_config(base_config(), true)
            .expect_err("local profiles should be blocked by default");
        assert!(error.contains("allow_local_environment"));
    }

    #[test]
    fn rejects_non_https_public_url() {
        let mut config = base_config();
        config.allow_local_environment = true;
        config.public_url = Some("http://example.com".to_string());
        let error =
            validate_remote_access_config(config, false).expect_err("http URL must be rejected");
        assert!(error.contains("https://"));
    }
}
