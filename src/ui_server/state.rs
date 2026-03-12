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
use crate::live_response::LiveResponseRegistry;
use crate::mcp::McpManager;
use crate::session::SessionManager;
use crate::session::{MessageRole, PersistedAskUserQuestion, PersistedAskUserRequest};
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
    pub(crate) notification_service: Arc<crate::notification_service::NotificationService>,
    pub(crate) browser_takeover_session: Mutex<Option<String>>,
    pub(crate) browser_takeover_alert_pending: Mutex<HashSet<String>>,
    pub(crate) memory_processor: Option<crate::memory::MemoryProcessor>,
    pub(crate) local_url_port_mapper: Arc<LocalUrlPortMapper>,
    pub(crate) title_generation_queue: TitleGenerationQueue,
    pub(crate) register_subagents_broker: RegisterSubagentsBroker,
    pub(crate) location_provider: crate::location::LocationProvider,
    pub(crate) runtime_service: Arc<super::runtime::SessionRuntimeService>,
    pub(crate) task_runtime: Arc<crate::scheduler::TaskRuntime>,
    pub(crate) cwd: PathBuf,
    pub(crate) may_dir: PathBuf,
}

impl UiServerState {
    pub(crate) async fn db(&self) -> crate::db::ChatDb {
        self.session_manager.lock().await.db().clone()
    }
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

#[derive(Debug, Clone)]
pub(crate) struct AskUserApprover {
    live_responses: Arc<LiveResponseRegistry<Vec<QuestionAnswer>>>,
    runtime_db: Option<crate::db::ChatDb>,
    ui_events_notify: Option<Arc<tokio::sync::Notify>>,
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
            live_responses: Arc::new(LiveResponseRegistry::new()),
            runtime_db: None,
            ui_events_notify: None,
        }
    }

    #[must_use]
    pub(crate) fn with_runtime_sync(
        mut self,
        db: crate::db::ChatDb,
        notify: Arc<tokio::sync::Notify>,
    ) -> Self {
        self.runtime_db = Some(db);
        self.ui_events_notify = Some(notify);
        self
    }

    fn notify_ui_events(&self) {
        if let Some(notify) = &self.ui_events_notify {
            notify.notify_waiters();
        }
    }

    async fn persist_ask_user_runtime_state(
        &self,
        session_id: &str,
        request: Option<PersistedAskUserRequest>,
    ) {
        let Some(db) = self.runtime_db.clone() else {
            return;
        };
        if let Err(error) = db.set_waiting_ask_user(session_id, request).await {
            tracing::warn!(session_id = %session_id, error = %error, "failed to persist ask-user runtime state");
            return;
        }
        self.notify_ui_events();
    }

    fn persisted_questions(questions: &[Question]) -> Vec<PersistedAskUserQuestion> {
        questions
            .iter()
            .map(|question| PersistedAskUserQuestion {
                section: question.section.clone(),
                question: question.question.clone(),
                option: question.option.clone(),
                multi_select: question.multi_select,
            })
            .collect()
    }

    pub(crate) async fn enqueue(&self, request: AskUserRequest, session_id: Option<String>) {
        let questions = request.args.clone().into_questions();
        let request_id = uuid::Uuid::new_v4().to_string();
        let rx = self.live_responses.open(request_id.clone());

        if let Some(session_id) = session_id.as_deref()
            && let Some(db) = self.runtime_db.clone()
        {
            self.persist_ask_user_runtime_state(
                session_id,
                Some(PersistedAskUserRequest {
                    id: request_id.clone(),
                    questions: Self::persisted_questions(&questions),
                }),
            )
            .await;

            let content = serde_json::to_string(&crate::session::history::AskUserRequestData {
                id: request_id.clone(),
                questions: serde_json::to_value(&questions)
                    .expect("ask_user questions must serialize"),
            })
            .expect("ask_user request content must serialize");
            if let Err(error) = db
                .append_message(
                    session_id,
                    MessageRole::AskUserRequest,
                    &content,
                    None,
                    None,
                    None,
                )
                .await
            {
                tracing::warn!(session_id, error = %error, "failed to append ask-user request history");
            }
        } else {
            self.notify_ui_events();
        }

        let answers = rx
            .recv()
            .await
            .unwrap_or_else(|_| skipped_answers(&questions));
        let _ = request.respond(answers);
    }

    pub(crate) fn respond(&self, request_id: &str, answers: Vec<QuestionAnswer>) -> bool {
        let responded = self.live_responses.resolve(request_id, answers);
        if responded {
            self.notify_ui_events();
        }
        responded
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
