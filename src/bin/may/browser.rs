//! `may browser` subcommand implementation.
//!
//! Manages the lifecycle of the may-owned headless browser service and
//! exposes structured JSON over stdout for each operation. The service
//! itself is a UNIX-socket daemon (`run_browser_service`) that lives
//! outside the agent process so that multiple may sessions can share
//! one browser runtime.

use std::io::{self, Write};
use std::sync::Arc;

use futures_lite::StreamExt;

use may_agent::MayConfig;
use may_agent::browser_service::{
    BrowserReadOptions, BrowserServiceRequest, BrowserServiceResponse, BrowserServiceStatus,
};

use super::cli::{BrowserCliArgs, BrowserCliCommand};

/// Top-level errors surfaced by the `may browser` subcommand.
#[derive(Debug, thiserror::Error)]
pub enum BrowserCliError {
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

pub async fn run_browser(args: BrowserCliArgs) -> Result<i32, BrowserCliError> {
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
        may_agent::container::browser::BrowserManager::new(
            config.browser.clone(),
            may_dir.clone(),
            may_agent::container::browser::BrowserLifecyclePublisher::noop(),
        )
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
                        running: browser_manager.is_running(),
                        cdp_endpoint: browser_manager.cdp_endpoint(),
                    },
                },
                false,
            ),
            BrowserServiceRequest::EnsureStarted => match async {
                if !browser_manager.is_running() {
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
                if browser_manager.is_running() {
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
                if !browser_manager.is_running() {
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
                    payload: Box::new(page),
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
                may_agent::container::browser::stop_managed_browser(config.port, may_dir).await;
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
    try_fetch_browser_targets(endpoint)
        .await?
        .ok_or_else(|| BrowserCliError::BrowserNotRunning(endpoint.to_string()))
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
