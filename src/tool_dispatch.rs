use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::ChatDb;
use crate::agent::browser_tool::BrowserToolError;
use crate::agent::browser_tool::{BrowserBroker, BrowserRequest, BrowserResult};
use crate::agent::notify_tool::{NotifyArgs, NotifyBroker, NotifyResult};
use crate::agent::register_tool::{RegisterBroker, RegisterRequest, RegisterResult};
use crate::agent::task_tool::TaskBroker;
use crate::ask_user::{AnswerValue, AskUserArgs, AskUserBroker, AskUserRequestPayload, Question};
use crate::browser_service::{
    BrowserServiceRequest, BrowserServiceResponse, BrowserServiceStatus,
    persist_browser_page_capture,
};
use crate::config::{BrowserConfig, SshServerConfig};
use crate::network_policy::NetworkPolicyConfig;
use crate::notification::NotificationManager;
use crate::notification_service::NotificationService;
use crate::scheduler::TaskRuntime;
use crate::session::{MayHomePaths, SessionStore};
use crate::{SkillManager, SubagentManager};

#[derive(Debug, thiserror::Error)]
pub enum ToolDispatchError {
    #[error("failed to initialize notification manager: {0}")]
    InitializeNotificationManager(crate::notification::NotificationError),
    #[error("failed to load task runtime: {0}")]
    LoadTaskRuntime(crate::db::ChatDbError),
    #[error("failed to configure task runtime: {0}")]
    ConfigureTaskRuntime(String),
}

#[derive(Debug, Clone)]
pub struct ToolDispatchers {
    pub task: TaskBroker,
    pub notify: NotifyBroker,
    pub browser: Option<BrowserBroker>,
    pub register_skills: RegisterBroker,
    pub register_subagents: RegisterBroker,
}

#[derive(Clone)]
pub struct ToolDispatchConfig {
    pub may_dir: PathBuf,
    pub cwd: PathBuf,
    pub db: ChatDb,
    pub browser_config: BrowserConfig,
    pub ssh_servers: Vec<SshServerConfig>,
    pub network_policy: NetworkPolicyConfig,
    pub ask_user_broker: AskUserBroker,
}

#[derive(Debug, Clone)]
struct BrowserServiceRuntime {
    browser_config: BrowserConfig,
    may_dir: PathBuf,
    db: ChatDb,
}

#[derive(Debug, Clone)]
struct RegistrationRuntime {
    may_dir: PathBuf,
    cwd: PathBuf,
    db: ChatDb,
    ask_user_broker: AskUserBroker,
}

impl BrowserServiceRuntime {
    fn socket_path(&self) -> PathBuf {
        crate::session::MayHomePaths::new(self.may_dir.clone())
            .browser_dir()
            .join("runtime.sock")
    }
}

impl ToolDispatchers {
    pub async fn spawn(config: ToolDispatchConfig) -> Result<Self, ToolDispatchError> {
        let ui_event_bus = crate::ui_event_bus::UiEventBus::new();
        let notification_manager = Arc::new(
            NotificationManager::new(config.may_dir.as_path(), config.db.clone())
                .await
                .map_err(ToolDispatchError::InitializeNotificationManager)?,
        );
        let notification_service = Arc::new(NotificationService::new(
            config.db.clone(),
            notification_manager,
            ui_event_bus.clone(),
        ));
        let task_runtime = Arc::new(
            TaskRuntime::load(config.db.clone(), config.may_dir.clone())
                .await
                .map_err(ToolDispatchError::LoadTaskRuntime)?
                .with_notifier(notification_service.clone())
                .with_ui_event_bus(ui_event_bus)
                .with_ssh_servers(config.ssh_servers.clone())
                .with_network_policy(config.network_policy.clone())
                .map_err(ToolDispatchError::ConfigureTaskRuntime)?,
        );
        let registration = RegistrationRuntime {
            may_dir: config.may_dir.clone(),
            cwd: config.cwd.clone(),
            db: config.db.clone(),
            ask_user_broker: config.ask_user_broker,
        };

        let (task, task_queue) = crate::agent::task_channel();
        tokio::spawn(async move {
            while let Some(request) = task_queue.next().await {
                let result = task_runtime
                    .create_task_from_tool_request(request.args.clone())
                    .await;
                let _ = request.respond(result);
            }
        });

        let (notify, notify_queue) = crate::agent::notify_channel();
        tokio::spawn(async move {
            while let Some(request) = notify_queue.next().await {
                let result =
                    handle_notify_request(&notification_service, request.args.clone()).await;
                let _ = request.respond(result);
            }
        });

        let browser = if config.browser_config.enabled {
            let browser_service = BrowserServiceRuntime {
                browser_config: config.browser_config,
                may_dir: config.may_dir,
                db: config.db.clone(),
            };
            let (broker, queue) = crate::agent::browser_channel();
            tokio::spawn(async move {
                while let Some(request) = queue.next().await {
                    let result =
                        handle_browser_request(&browser_service, request.args.clone()).await;
                    let _ = request.respond(result);
                }
            });
            Some(broker)
        } else {
            None
        };

        let (register_skills, register_skills_queue) =
            crate::agent::register_tool::skills_channel();
        let registration_for_skills = registration.clone();
        tokio::spawn(async move {
            while let Some(request) = register_skills_queue.next().await {
                let result =
                    handle_register_skills_request(&registration_for_skills, request.args.clone())
                        .await;
                let _ = request.respond(result);
            }
        });

        let (register_subagents, register_subagents_queue) =
            crate::agent::register_tool::subagents_channel();
        tokio::spawn(async move {
            while let Some(request) = register_subagents_queue.next().await {
                let result =
                    handle_register_subagents_request(&registration, request.args.clone()).await;
                let _ = request.respond(result);
            }
        });

        Ok(Self {
            task,
            notify,
            browser,
            register_skills,
            register_subagents,
        })
    }
}

async fn handle_notify_request(
    notification_service: &Arc<NotificationService>,
    args: NotifyArgs,
) -> Result<NotifyResult, String> {
    let payload = crate::notification::NotificationPayload {
        notification_id: None,
        title: args.title,
        body: args.body,
        session_id: args.session_id,
        callback_context_xml: args.callback_context_xml,
        callback_message: args.callback_message,
    };
    let sent = notification_service
        .send_payload(payload)
        .await
        .map_err(|error| error.to_string())?;
    Ok(NotifyResult { sent })
}

async fn handle_browser_request(
    browser_service: &BrowserServiceRuntime,
    request: BrowserRequest,
) -> Result<BrowserResult, BrowserToolError> {
    let session_id = request.session_id;
    if !browser_service.browser_config.enabled {
        return Err(BrowserToolError::request_failed(
            "browser command is unavailable because browser automation is disabled in settings",
        ));
    }

    match request.args {
        crate::agent::BrowserArgs::Status => {
            let status = match browser_service_status(browser_service.socket_path().as_path()).await
            {
                Ok(Some(status)) => Some(status),
                Ok(None) => Some(
                    ensure_browser_service_started(
                        browser_service.may_dir.as_path(),
                        &browser_service.browser_config,
                    )
                    .await
                    .map_err(BrowserToolError::request_failed)?,
                ),
                Err(_) => Some(
                    ensure_browser_service_started(
                        browser_service.may_dir.as_path(),
                        &browser_service.browser_config,
                    )
                    .await
                    .map_err(BrowserToolError::request_failed)?,
                ),
            };
            Ok(BrowserResult {
                running: status.as_ref().is_some_and(|item| item.running),
                cdp_endpoint: status.and_then(|item| {
                    if item.running {
                        item.cdp_endpoint
                    } else {
                        None
                    }
                }),
                page: None,
            })
        }
        crate::agent::BrowserArgs::Start => {
            let status = ensure_browser_service_started(
                browser_service.may_dir.as_path(),
                &browser_service.browser_config,
            )
            .await
            .map_err(BrowserToolError::request_failed)?;
            Ok(BrowserResult {
                running: true,
                cdp_endpoint: status.cdp_endpoint,
                page: None,
            })
        }
        crate::agent::BrowserArgs::Stop => {
            stop_browser_service(
                browser_service.socket_path().as_path(),
                browser_service.browser_config.port,
            )
            .await
            .map_err(BrowserToolError::request_failed)?;
            Ok(BrowserResult {
                running: false,
                cdp_endpoint: None,
                page: None,
            })
        }
        crate::agent::BrowserArgs::CdpEndpoint => {
            let status = ensure_browser_service_started(
                browser_service.may_dir.as_path(),
                &browser_service.browser_config,
            )
            .await
            .map_err(BrowserToolError::request_failed)?;
            Ok(BrowserResult {
                running: true,
                cdp_endpoint: status.cdp_endpoint,
                page: None,
            })
        }
        crate::agent::BrowserArgs::Read { url, options } => {
            let request = BrowserServiceRequest::Read { url, options };
            let status = ensure_browser_service_started(
                browser_service.may_dir.as_path(),
                &browser_service.browser_config,
            )
            .await
            .map_err(BrowserToolError::request_failed)?;
            let response = query_browser_service(browser_service.socket_path().as_path(), &request)
                .await
                .map_err(BrowserToolError::request_failed)?;
            match response {
                Some(BrowserServiceResponse::ReadOk { payload }) => {
                    let session_paths =
                        resolve_browser_session_paths(browser_service, session_id.as_str())
                            .await
                            .map_err(BrowserToolError::request_failed)?;
                    let page =
                        persist_browser_page_capture(session_id.as_str(), &session_paths, payload)
                            .await
                            .map_err(|error| BrowserToolError::request_failed(error.to_string()))?;
                    Ok(BrowserResult {
                        running: true,
                        cdp_endpoint: status.cdp_endpoint,
                        page: Some(page),
                    })
                }
                Some(BrowserServiceResponse::Error { message }) => {
                    Err(BrowserToolError::request_failed(message))
                }
                Some(BrowserServiceResponse::Ok { .. }) => Err(BrowserToolError::request_failed(
                    "browser service returned status payload for read request",
                )),
                Some(BrowserServiceResponse::Stopped) | None => Err(
                    BrowserToolError::request_failed("browser service stopped while reading page"),
                ),
            }
        }
    }
}

async fn resolve_browser_session_paths(
    runtime: &BrowserServiceRuntime,
    session_id: &str,
) -> Result<crate::session::SessionPaths, String> {
    let session = runtime
        .db
        .get_session(session_id)
        .await
        .map_err(|error| session_error_text("failed to load session ", session_id, &error))?
        .ok_or_else(|| session_not_found_text(session_id))?;
    let store = SessionStore::new(MayHomePaths::new(runtime.may_dir.clone()));
    let session_paths = store.session_paths(session_id, session.profile_id.as_deref());
    tokio::fs::create_dir_all(session_paths.root())
        .await
        .map_err(|error| {
            path_error_text(
                "failed to create session root ",
                session_paths.root(),
                &error,
            )
        })?;
    Ok(session_paths)
}

async fn handle_register_skills_request(
    runtime: &RegistrationRuntime,
    request: RegisterRequest,
) -> Result<RegisterResult, String> {
    handle_register_request(
        runtime,
        request,
        "skill",
        "skills",
        |skills_dir, source| async move {
            let manager = SkillManager::from_skills_dir(skills_dir);
            let info = manager
                .register_from_local_path(&source)
                .await
                .map_err(|error| {
                    path_error_text("failed to register skill from ", &source, &error)
                })?;
            Ok(vec![info.dir_name])
        },
    )
    .await
}

async fn handle_register_subagents_request(
    runtime: &RegistrationRuntime,
    request: RegisterRequest,
) -> Result<RegisterResult, String> {
    handle_register_request(
        runtime,
        request,
        "subagent definitions",
        "subagents",
        |subagents_dir, source| async move {
            let manager = SubagentManager::from_subagents_dir(subagents_dir);
            let infos = manager
                .register_from_local_path(&source)
                .await
                .map_err(|error| {
                    path_error_text("failed to register subagents from ", &source, &error)
                })?;
            Ok(infos.into_iter().map(|item| item.file_name).collect())
        },
    )
    .await
}

async fn handle_register_request<F, Fut>(
    runtime: &RegistrationRuntime,
    request: RegisterRequest,
    source_label: &str,
    target_dir_name: &str,
    register: F,
) -> Result<RegisterResult, String>
where
    F: FnOnce(PathBuf, PathBuf) -> Fut,
    Fut: Future<Output = Result<Vec<String>, String>>,
{
    let source = resolve_registration_source_path(runtime.cwd.as_path(), &request.path).await?;
    let target_dir =
        resolve_registration_target_dir(runtime, request.session_id.as_str(), target_dir_name)
            .await?;
    request_registration_approval(
        runtime,
        request.session_id.as_str(),
        source.as_path(),
        source_label,
        target_dir_name,
    )
    .await?;
    Ok(RegisterResult {
        registered: register(target_dir, source).await?,
    })
}

async fn resolve_registration_target_dir(
    runtime: &RegistrationRuntime,
    session_id: &str,
    target_dir_name: &str,
) -> Result<PathBuf, String> {
    let session = runtime
        .db
        .get_session(session_id)
        .await
        .map_err(|error| session_error_text("failed to load session ", session_id, &error))?
        .ok_or_else(|| session_not_found_text(session_id))?;
    let store = SessionStore::new(MayHomePaths::new(runtime.may_dir.clone()));
    let session_paths = store.session_paths(session_id, session.profile_id.as_deref());
    tokio::fs::create_dir_all(session_paths.root())
        .await
        .map_err(|error| {
            path_error_text(
                "failed to create session root ",
                session_paths.root(),
                &error,
            )
        })?;
    Ok(session_paths.home_root().join(target_dir_name))
}

async fn resolve_registration_source_path(cwd: &Path, raw_path: &str) -> Result<PathBuf, String> {
    let path = raw_path.trim();
    if path.is_empty() {
        return Err("registration path must not be empty".to_string());
    }

    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    let canonical = tokio::fs::canonicalize(&candidate).await.map_err(|error| {
        path_error_text("failed to resolve registration path ", &candidate, &error)
    })?;
    let project_root = tokio::fs::canonicalize(cwd)
        .await
        .map_err(|error| path_error_text("failed to resolve project root ", cwd, &error))?;

    if !canonical.starts_with(&project_root) {
        return Err(path_message(
            "registration path must be inside project root ",
            &project_root,
        ));
    }

    Ok(canonical)
}

async fn request_registration_approval(
    runtime: &RegistrationRuntime,
    session_id: &str,
    source: &Path,
    source_label: &str,
    target_dir_name: &str,
) -> Result<(), String> {
    let question =
        build_registration_approval_message(session_id, source, source_label, target_dir_name);
    let response = runtime
        .ask_user_broker
        .request(AskUserRequestPayload {
            session_id: session_id.to_string(),
            request: AskUserArgs {
                question: None,
                option: Vec::new(),
                multi_select: false,
                questions: vec![Question {
                    section: "registration".to_string(),
                    question,
                    option: vec!["approve".to_string(), "deny".to_string()],
                    multi_select: false,
                }],
            },
        })
        .await
        .map_err(|error| error.to_string())?;

    let approved = response.into_iter().any(|answer| match answer.answer {
        AnswerValue::Selected(values) => values
            .iter()
            .any(|value| value.eq_ignore_ascii_case("approve")),
        AnswerValue::Custom(value) => value.eq_ignore_ascii_case("approve"),
        AnswerValue::Skipped => false,
    });
    if approved {
        Ok(())
    } else {
        Err("user denied registration request".to_string())
    }
}

fn build_registration_approval_message(
    session_id: &str,
    source: &Path,
    source_label: &str,
    target_dir_name: &str,
) -> String {
    let source_display = source.display().to_string();
    join_text(&[
        "Session ",
        session_id,
        " requests registering ",
        source_label,
        " from ",
        source_display.as_str(),
        " into this session's ",
        target_dir_name,
        ". Approve?",
    ])
}

fn join_text(parts: &[&str]) -> String {
    let capacity = parts.iter().map(|part| part.len()).sum();
    let mut output = String::with_capacity(capacity);
    for part in parts {
        output.push_str(part);
    }
    output
}

fn path_message(prefix: &str, path: &Path) -> String {
    let path_text = path.display().to_string();
    join_text(&[prefix, path_text.as_str()])
}

fn path_error_text(prefix: &str, path: &Path, error: &impl std::fmt::Display) -> String {
    let path_text = path.display().to_string();
    let error_text = error.to_string();
    join_text(&[prefix, path_text.as_str(), ": ", error_text.as_str()])
}

fn session_error_text(prefix: &str, session_id: &str, error: &impl std::fmt::Display) -> String {
    let error_text = error.to_string();
    join_text(&[prefix, session_id, ": ", error_text.as_str()])
}

fn session_not_found_text(session_id: &str) -> String {
    join_text(&["session not found: ", session_id])
}

async fn browser_service_status(
    socket_path: &Path,
) -> Result<Option<BrowserServiceStatus>, String> {
    let request = BrowserServiceRequest::Status;
    match query_browser_service(socket_path, &request).await? {
        Some(BrowserServiceResponse::Ok { payload }) => Ok(Some(payload)),
        Some(BrowserServiceResponse::Stopped) => Ok(None),
        Some(BrowserServiceResponse::Error { message }) => Err(message),
        Some(BrowserServiceResponse::ReadOk { .. }) => {
            Err("browser service returned page payload for status request".to_string())
        }
        None => Ok(None),
    }
}

async fn ensure_browser_service_started(
    may_dir: &Path,
    _browser_config: &BrowserConfig,
) -> Result<BrowserServiceStatus, String> {
    let request = BrowserServiceRequest::EnsureStarted;
    let socket_path = crate::session::MayHomePaths::new(may_dir.to_path_buf())
        .browser_dir()
        .join("runtime.sock");

    if let Some(response) = query_browser_service(&socket_path, &request).await? {
        return match response {
            BrowserServiceResponse::Ok { payload } if payload.running => Ok(payload),
            BrowserServiceResponse::Ok { .. } | BrowserServiceResponse::Stopped => {
                Err("browser service returned a stopped state".to_string())
            }
            BrowserServiceResponse::Error { message } => Err(message),
            BrowserServiceResponse::ReadOk { .. } => {
                Err("browser service returned page payload for startup request".to_string())
            }
        };
    }

    let current_exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = tokio::process::Command::new(current_exe);
    command
        .arg("browser")
        .arg("service")
        .arg("--socket")
        .arg(socket_path.as_os_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command.spawn().map_err(|error| error.to_string())?;

    let mut last_error = None;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        match query_browser_service(&socket_path, &request).await {
            Ok(Some(BrowserServiceResponse::Ok { payload })) if payload.running => {
                return Ok(payload);
            }
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
            Ok(Some(BrowserServiceResponse::Ok { .. })) => {}
        }
    }

    Err(last_error.unwrap_or_else(|| "browser service did not become ready".to_string()))
}

async fn stop_browser_service(socket_path: &Path, _port: u16) -> Result<(), String> {
    let request = BrowserServiceRequest::Stop;
    match query_browser_service(socket_path, &request).await? {
        Some(BrowserServiceResponse::Ok { .. }) | Some(BrowserServiceResponse::Stopped) | None => {
            Ok(())
        }
        Some(BrowserServiceResponse::Error { message }) => Err(message),
        Some(BrowserServiceResponse::ReadOk { .. }) => {
            Err("browser service returned page payload for stop request".to_string())
        }
    }
}

async fn query_browser_service(
    socket_path: &Path,
    request: &BrowserServiceRequest,
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
