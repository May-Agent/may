use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant as TokioInstant;

use crate::agent::PermissionApprover;
use crate::agent::register_tool::RegisterSubagentsBroker;
use crate::ask_user::{AnswerValue, AskUserRequest, Question, QuestionAnswer};
use crate::config::MayConfig;
use crate::mcp::McpManager;
use crate::session::SessionManager;
use crate::skill::SkillManager;

use super::local_url_mapper::LocalUrlPortMapper;
use super::types::{ModelsResponse, ProviderModelsResponse, RequestLogEntry};

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContainerPrepareStage {
    Checking,
    Building,
    Ready,
    Failed,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ContainerPrepareStatus {
    pub(crate) profile_id: String,
    pub(crate) stage: ContainerPrepareStage,
    pub(crate) summary: String,
    pub(crate) recent_logs: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ContainerPrepareStatusesPayload {
    pub(crate) statuses: Vec<ContainerPrepareStatus>,
}

#[derive(Debug, Clone)]
pub(crate) struct TitleGenerationTask {
    pub(crate) session_id: String,
    pub(crate) user_message: String,
    pub(crate) assistant_response: String,
}

#[derive(Debug, Clone)]
pub(crate) struct TitleGenerationQueue {
    tx: tokio::sync::mpsc::UnboundedSender<TitleGenerationTask>,
}

impl TitleGenerationQueue {
    pub(crate) fn new() -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<TitleGenerationTask>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    pub(crate) fn enqueue(&self, task: TitleGenerationTask) {
        let _ = self.tx.send(task);
    }
}

#[derive(Debug)]
pub(crate) struct RefreshableCache<T> {
    ttl: Duration,
    inner: Mutex<Option<(TokioInstant, T)>>,
}

impl<T: Clone> RefreshableCache<T> {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(None),
        }
    }

    pub(crate) async fn get_or_refresh<F, Fut>(&self, refresh: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        {
            let guard = self.inner.lock().await;
            if let Some((updated_at, value)) = guard.as_ref()
                && updated_at.elapsed() < self.ttl
            {
                return value.clone();
            }
        }

        let value = refresh().await;
        let mut guard = self.inner.lock().await;
        *guard = Some((TokioInstant::now(), value.clone()));
        value
    }
}

pub(crate) struct UiServerState {
    pub(crate) session_manager: Mutex<SessionManager>,
    pub(crate) mcp_manager: Mutex<McpManager>,
    pub(crate) skill_manager: SkillManager,
    pub(crate) permission_approver: PermissionApprover,
    pub(crate) ask_user_approver: AskUserApprover,
    pub(crate) active_streaming_sessions: Mutex<HashSet<String>>,
    pub(crate) request_logs: Mutex<VecDeque<RequestLogEntry>>,
    pub(crate) models_cache: RefreshableCache<ModelsResponse>,
    pub(crate) provider_models_cache: RefreshableCache<ProviderModelsResponse>,
    pub(crate) config: tokio::sync::RwLock<MayConfig>,
    pub(crate) ui_events_notify: Arc<tokio::sync::Notify>,
    pub(crate) container_prepare_statuses: Mutex<HashMap<String, ContainerPrepareStatus>>,
    pub(crate) container_prepare_inflight: Mutex<HashSet<String>>,
    pub(crate) container_manager: Option<Arc<crate::container::lifecycle::ContainerManager>>,
    pub(crate) browser_manager: Option<Arc<crate::container::playwright::BrowserManager>>,
    pub(crate) notification_manager: Arc<crate::notification::NotificationManager>,
    pub(crate) browser_takeover_session: Mutex<Option<String>>,
    pub(crate) browser_takeover_alert_pending: Mutex<HashSet<String>>,
    pub(crate) memory_processor: Option<crate::memory::MemoryProcessor>,
    pub(crate) local_url_port_mapper: Arc<LocalUrlPortMapper>,
    pub(crate) title_generation_queue: TitleGenerationQueue,
    pub(crate) register_subagents_broker: RegisterSubagentsBroker,
    pub(crate) location_provider: crate::location::LocationProvider,
    pub(crate) cwd: PathBuf,
    pub(crate) may_dir: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AskUserRequestPayload {
    pub(crate) id: String,
    pub(crate) questions: Vec<Question>,
    pub(crate) session_id: Option<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AskUserPollResponse {
    pub(crate) request: Option<AskUserRequestPayload>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiQuestionAnswer {
    pub(crate) section: String,
    pub(crate) answer: serde_json::Value,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AskUserRespondRequest {
    pub(crate) id: String,
    pub(crate) answers: Vec<UiQuestionAnswer>,
}

/// Pending ask-user entry: response channel + session_id for persistence.
#[derive(Debug, Clone)]
struct AskUserPendingPayload {
    questions: Vec<Question>,
    session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AskUserApprover {
    inner: Arc<
        aither_agent::tool_request::RequestApprover<AskUserPendingPayload, Vec<QuestionAnswer>>,
    >,
}

/// Generate skipped answers for all questions in the request.
fn skipped_answers(questions: &[Question]) -> Vec<QuestionAnswer> {
    questions
        .iter()
        .map(|q| QuestionAnswer {
            section: q.section.clone(),
            answer: AnswerValue::Skipped,
        })
        .collect()
}

impl AskUserApprover {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(aither_agent::tool_request::RequestApprover::new()),
        }
    }

    pub(crate) async fn enqueue(&self, request: AskUserRequest, session_id: Option<String>) {
        let questions = request.args.clone().into_questions();
        let (_id, rx) = self.inner.enqueue(AskUserPendingPayload {
            questions: questions.clone(),
            session_id,
        });

        let answers = rx
            .recv()
            .await
            .unwrap_or_else(|_| skipped_answers(&questions));
        let _ = request.respond(answers);
    }

    pub(crate) fn respond(&self, request_id: &str, answers: Vec<QuestionAnswer>) -> bool {
        self.inner.respond(request_id, answers)
    }

    pub(crate) fn get_session_id(&self, request_id: &str) -> Option<String> {
        self.inner
            .get_payload(request_id)
            .and_then(|payload| payload.session_id)
    }

    pub(crate) fn snapshot_pending(
        &self,
        session_filter: Option<&str>,
    ) -> Option<AskUserRequestPayload> {
        self.inner
            .peek_filtered(|payload| {
                if let Some(expected) = session_filter {
                    payload.session_id.as_deref() == Some(expected)
                } else {
                    true
                }
            })
            .map(|(id, payload)| AskUserRequestPayload {
                id,
                questions: payload.questions,
                session_id: payload.session_id,
            })
    }

    pub(crate) fn listen(&self) -> event_listener::EventListener {
        self.inner.listen()
    }
}

impl UiServerState {
    pub(crate) fn notify_ui_events(&self) {
        self.ui_events_notify.notify_waiters();
    }

    pub(crate) async fn resolve_container_manager(
        &self,
        allow_managed_podman_download: bool,
    ) -> Result<Arc<crate::container::lifecycle::ContainerManager>, crate::container::ContainerError>
    {
        if let Some(manager) = self.container_manager.as_ref() {
            return Ok(manager.clone());
        }

        let runtime = crate::container::ContainerRuntime::detect_with_preference_and_options(
            aither_sandbox::RuntimePreference::default(),
            crate::container::RuntimeDetectOptions {
                allow_managed_podman_download,
            },
        )
        .await?;
        Ok(Arc::new(
            crate::container::lifecycle::ContainerManager::new(
                Arc::new(runtime),
                self.may_dir.clone(),
            ),
        ))
    }

    pub(crate) async fn try_begin_container_prepare(&self, profile_id: &str) -> bool {
        let mut inflight = self.container_prepare_inflight.lock().await;
        inflight.insert(profile_id.to_string())
    }

    pub(crate) async fn finish_container_prepare(&self, profile_id: &str) {
        self.container_prepare_inflight
            .lock()
            .await
            .remove(profile_id);
    }

    pub(crate) async fn set_container_prepare_status(&self, status: ContainerPrepareStatus) {
        self.container_prepare_statuses
            .lock()
            .await
            .insert(status.profile_id.clone(), status);
        self.notify_ui_events();
    }

    pub(crate) async fn get_container_prepare_status(
        &self,
        profile_id: &str,
    ) -> Option<ContainerPrepareStatus> {
        self.container_prepare_statuses
            .lock()
            .await
            .get(profile_id)
            .cloned()
    }

    pub(crate) async fn clear_container_prepare_status(&self, profile_id: &str) {
        self.container_prepare_statuses
            .lock()
            .await
            .remove(profile_id);
        self.notify_ui_events();
    }

    pub(crate) async fn snapshot_container_prepare_statuses(
        &self,
    ) -> ContainerPrepareStatusesPayload {
        let mut statuses = self
            .container_prepare_statuses
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        statuses.sort_by(|a, b| a.profile_id.cmp(&b.profile_id));
        ContainerPrepareStatusesPayload { statuses }
    }
}
