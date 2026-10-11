//! Tool-dispatch request handlers used by the chat SSE loop.
//!
//! Each `handle_*_request` maps a typed tool-call from an agent into
//! a concrete side effect against `UiServerState` (creating a task,
//! sending a notification, driving the managed browser, or registering
//! skills/subagents after user approval). All helpers in this file are
//! `pub(super)` so the chat module that wires the tool-dispatch loop
//! can invoke them without re-exporting anything.

use std::sync::Arc;

use aither_agent::ask_user;

use crate::agent::browser_tool::BrowserToolError;
use crate::notification::{NotificationPayload, NotificationService};
use crate::ui_event_bus::UiEventTopics;

use super::super::browser::ensure_browser_runtime_started;
use super::super::error_chain::error_chain_text;
use super::super::helpers::load_session_paths;
use super::super::state::UiServerState;
use crate::browser_service::persist_browser_page_capture;

pub(super) async fn handle_task_request(
    state: &Arc<UiServerState>,
    request: crate::agent::task_tool::TaskCreateRequest,
) -> Result<crate::agent::task_tool::TaskCreateResult, String> {
    let result = state
        .task_runtime
        .create_task_from_tool_request(request)
        .await;
    result
}

pub(super) async fn handle_notify_request(
    notification_service: Arc<NotificationService>,
    args: crate::agent::notify_tool::NotifyArgs,
) -> Result<crate::agent::notify_tool::NotifyResult, String> {
    let payload = NotificationPayload {
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
        .map_err(|error| format!("failed to send notification payload: {error}"))?;

    Ok(crate::agent::notify_tool::NotifyResult { sent })
}

pub(super) async fn handle_browser_request(
    state: &Arc<UiServerState>,
    request: crate::agent::browser_tool::BrowserRequest,
) -> Result<crate::agent::browser_tool::BrowserResult, BrowserToolError> {
    let session_id = request.session_id;
    let Some(browser_manager) = state.browser_manager() else {
        return Err(BrowserToolError::UnavailableInRuntime);
    };

    let browser_not_running_error = || {
        BrowserToolError::request_failed(
            "browser is not running; call browser open before browser cdp-endpoint or browser read",
        )
    };

    match request.args {
        crate::agent::browser_tool::BrowserArgs::Status => {
            Ok(crate::agent::browser_tool::BrowserResult {
                running: browser_manager.is_running(),
                cdp_endpoint: None,
                page: None,
            })
        }
        crate::agent::browser_tool::BrowserArgs::Start => {
            let browser_manager = ensure_browser_runtime_started(state.as_ref())
                .await
                .map_err(|error| BrowserToolError::request_failed(error_chain_text(&error)))?;
            state.set_browser_owner_session_id(Some(session_id.clone()));
            state.notify_ui_events(UiEventTopics::BROWSER_STATUS);
            Ok(crate::agent::browser_tool::BrowserResult {
                running: true,
                cdp_endpoint: browser_manager.cdp_endpoint(),
                page: None,
            })
        }
        crate::agent::browser_tool::BrowserArgs::Stop => {
            browser_manager
                .stop()
                .await
                .map_err(|error| BrowserToolError::request_failed(error.to_string()))?;
            state.set_browser_owner_session_id(None);
            state.notify_ui_events(UiEventTopics::BROWSER_STATUS);
            Ok(crate::agent::browser_tool::BrowserResult {
                running: false,
                cdp_endpoint: None,
                page: None,
            })
        }
        crate::agent::browser_tool::BrowserArgs::CdpEndpoint => {
            if !browser_manager.is_running() {
                return Err(browser_not_running_error());
            }
            Ok(crate::agent::browser_tool::BrowserResult {
                running: true,
                cdp_endpoint: browser_manager.cdp_endpoint(),
                page: None,
            })
        }
        crate::agent::browser_tool::BrowserArgs::Read {
            url,
            detail,
            screenshot,
            format: _,
        } => {
            let options = crate::agent::browser_tool::BrowserArgs::read_options(detail, screenshot);
            let browser_manager = ensure_browser_runtime_started(state.as_ref())
                .await
                .map_err(|error| BrowserToolError::request_failed(error_chain_text(&error)))?;
            state.set_browser_owner_session_id(Some(session_id.clone()));
            state.notify_ui_events(UiEventTopics::BROWSER_STATUS);
            let capture = browser_manager
                .read_page(url, options)
                .await
                .map_err(|error| BrowserToolError::request_failed(error_chain_text(&error)))?;
            let session_paths = load_session_paths(state, session_id.as_str())
                .await
                .map_err(|error| BrowserToolError::request_failed(error.to_string()))?;
            let page = persist_browser_page_capture(session_id.as_str(), &session_paths, capture)
                .await
                .map_err(|error| BrowserToolError::request_failed(error.to_string()))?;
            Ok(crate::agent::browser_tool::BrowserResult {
                running: true,
                cdp_endpoint: browser_manager.cdp_endpoint(),
                page: Some(page),
            })
        }
    }
}

pub(super) async fn handle_register_skills_request(
    state: &Arc<UiServerState>,
    request: crate::agent::register_tool::RegisterRequest,
) -> Result<crate::agent::register_tool::RegisterResult, String> {
    handle_register_request(
        state,
        request,
        "skill",
        "skills",
        |skills_dir, source| async move {
            let manager = crate::skill::SkillManager::from_skills_dir(skills_dir);
            let info = manager
                .register_from_local_path(&source)
                .await
                .map_err(|error| {
                    format!(
                        "failed to register skill from {}: {error}",
                        source.display()
                    )
                })?;
            Ok(vec![info.dir_name])
        },
    )
    .await
}

pub(super) async fn handle_register_subagents_request(
    state: &Arc<UiServerState>,
    request: crate::agent::register_tool::RegisterRequest,
) -> Result<crate::agent::register_tool::RegisterResult, String> {
    handle_register_request(
        state,
        request,
        "subagent definitions",
        "subagents",
        |subagents_dir, source| async move {
            let manager = crate::subagent::SubagentManager::from_subagents_dir(subagents_dir);
            let infos = manager
                .register_from_local_path(&source)
                .await
                .map_err(|error| {
                    format!(
                        "failed to register subagents from {}: {error}",
                        source.display()
                    )
                })?;
            Ok(infos.into_iter().map(|item| item.file_name).collect())
        },
    )
    .await
}

async fn handle_register_request<F, Fut>(
    state: &Arc<UiServerState>,
    request: crate::agent::register_tool::RegisterRequest,
    source_label: &str,
    target_dir_name: &str,
    register: F,
) -> Result<crate::agent::register_tool::RegisterResult, String>
where
    F: FnOnce(std::path::PathBuf, std::path::PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<String>, String>>,
{
    let source = resolve_registration_source_path(state, &request.path).await?;
    let session_root = resolve_session_root(state, request.session_id.as_str()).await?;
    let target_dir = session_root.join(target_dir_name);
    let approval_message = build_registration_approval_message(
        request.session_id.as_str(),
        &source,
        source_label,
        target_dir_name,
    );
    request_registration_approval(state, request.session_id.as_str(), &approval_message).await?;
    Ok(crate::agent::register_tool::RegisterResult {
        registered: register(target_dir, source).await?,
    })
}

fn build_registration_approval_message(
    session_id: &str,
    source: &std::path::Path,
    source_label: &str,
    target_dir_name: &str,
) -> String {
    format!(
        "Session {session_id} requests registering {source_label} from {} into this session's {target_dir_name}. Approve?",
        source.display()
    )
}

async fn resolve_session_root(
    state: &UiServerState,
    session_id: &str,
) -> Result<std::path::PathBuf, String> {
    let session_root = load_session_paths(state, session_id)
        .await
        .map_err(|error| error.to_string())?
        .root()
        .to_path_buf();
    tokio::fs::create_dir_all(&session_root)
        .await
        .map_err(|error| {
            format!(
                "failed to create session root {}: {error}",
                session_root.display()
            )
        })?;
    let session_root_canonical = tokio::fs::canonicalize(&session_root)
        .await
        .map_err(|error| {
            format!(
                "failed to resolve session {}: {error}",
                session_root.display()
            )
        })?;
    Ok(session_root_canonical)
}

async fn resolve_registration_source_path(
    state: &UiServerState,
    raw_path: &str,
) -> Result<std::path::PathBuf, String> {
    let path = raw_path.trim();
    if path.is_empty() {
        return Err("registration path must not be empty".to_string());
    }

    let candidate = if std::path::Path::new(path).is_absolute() {
        std::path::PathBuf::from(path)
    } else {
        state.cwd.join(path)
    };

    let canonical = tokio::fs::canonicalize(&candidate).await.map_err(|error| {
        format!(
            "failed to resolve registration path {}: {error}",
            candidate.display()
        )
    })?;
    let project_root = tokio::fs::canonicalize(&state.cwd).await.map_err(|error| {
        format!(
            "failed to resolve project root {}: {error}",
            state.cwd.display()
        )
    })?;

    if !canonical.starts_with(&project_root) {
        return Err(format!(
            "registration path must be inside project root {}",
            project_root.display()
        ));
    }

    Ok(canonical)
}

async fn request_registration_approval(
    state: &UiServerState,
    session_id: &str,
    question: &str,
) -> Result<(), String> {
    let (tx, rx) = async_channel::bounded(1);
    let request = ask_user::AskUserRequest::new(
        ask_user::AskUserRequestPayload {
            session_id: session_id.to_string(),
            request: ask_user::AskUserArgs {
                question: None,
                option: Vec::new(),
                multi_select: false,
                questions: vec![ask_user::Question {
                    section: "registration".to_string(),
                    question: question.to_string(),
                    option: vec!["approve".to_string(), "deny".to_string()],
                    multi_select: false,
                }],
            },
        },
        tx,
    );
    state
        .ask_user_approver
        .enqueue(request, Some(session_id.to_string()))
        .await;
    let answers = rx
        .recv()
        .await
        .map_err(|error| format!("failed to receive user approval: {error}"))?;

    let approved = answers.into_iter().any(|answer| match answer.answer {
        ask_user::AnswerValue::Selected(values) => values
            .iter()
            .any(|value| value.eq_ignore_ascii_case("approve")),
        ask_user::AnswerValue::Custom(value) => value.eq_ignore_ascii_case("approve"),
        ask_user::AnswerValue::Skipped => false,
    });

    if !approved {
        return Err("user denied registration request".to_string());
    }

    Ok(())
}
