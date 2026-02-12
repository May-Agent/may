use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use async_channel::{Receiver, Sender, bounded, unbounded};
use tokio::sync::Mutex;
use tokio::time::Instant as TokioInstant;

use crate::agent::PermissionApprover;
use crate::ask_user::{AnswerValue, AskUserRequest, Question, QuestionAnswer};
use crate::config::MayConfig;
use crate::mcp::McpManager;
use crate::session::SessionManager;
use crate::skill::SkillManager;

use super::types::{ModelsResponse, RequestLogEntry};

pub(crate) struct UiServerState {
    pub(crate) session_manager: Mutex<SessionManager>,
    pub(crate) mcp_manager: Mutex<McpManager>,
    pub(crate) skill_manager: SkillManager,
    pub(crate) permission_approver: PermissionApprover,
    pub(crate) ask_user_approver: AskUserApprover,
    pub(crate) active_streaming_sessions: Mutex<HashSet<String>>,
    pub(crate) request_logs: Mutex<VecDeque<RequestLogEntry>>,
    pub(crate) models_cache: Mutex<Option<(TokioInstant, ModelsResponse)>>,
    pub(crate) config: tokio::sync::RwLock<MayConfig>,
    pub(crate) cwd: PathBuf,
    pub(crate) may_dir: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct AskUserRequestPayload {
    pub(crate) id: String,
    pub(crate) questions: Vec<Question>,
    pub(crate) session_id: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct AskUserPollResponse {
    pub(crate) request: Option<AskUserRequestPayload>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct UiQuestionAnswer {
    pub(crate) section: String,
    pub(crate) answer: serde_json::Value,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct AskUserRespondRequest {
    pub(crate) id: String,
    pub(crate) answers: Vec<UiQuestionAnswer>,
}

/// Pending ask-user entry: response channel + session_id for persistence.
#[derive(Debug)]
struct AskUserPending {
    tx: Sender<Vec<QuestionAnswer>>,
    session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AskUserApprover {
    request_tx: Sender<AskUserRequestPayload>,
    request_rx: Receiver<AskUserRequestPayload>,
    pending: Arc<Mutex<HashMap<String, AskUserPending>>>,
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
        let (request_tx, request_rx) = unbounded();
        Self {
            request_tx,
            request_rx,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) async fn enqueue(&self, request: AskUserRequest, session_id: Option<String>) {
        let id = uuid::Uuid::new_v4().to_string();
        let questions = request.args.clone().into_questions();
        let payload = AskUserRequestPayload {
            id: id.clone(),
            questions: questions.clone(),
            session_id: session_id.clone(),
        };

        let (tx, rx) = bounded(1);
        self.pending.lock().await.insert(
            id.clone(),
            AskUserPending {
                tx,
                session_id,
            },
        );

        if self.request_tx.send(payload).await.is_err() {
            let _ = self.pending.lock().await.remove(&id);
            let _ = request.respond(skipped_answers(&questions));
            return;
        }

        let answers = rx
            .recv()
            .await
            .unwrap_or_else(|_| skipped_answers(&questions));
        let _ = request.respond(answers);
    }

    pub(crate) async fn next_request(&self) -> Option<AskUserRequestPayload> {
        self.request_rx.recv().await.ok()
    }

    pub(crate) async fn respond(&self, request_id: &str, answers: Vec<QuestionAnswer>) -> bool {
        let entry = self.pending.lock().await.remove(request_id);
        if let Some(pending) = entry {
            let _ = pending.tx.send(answers).await;
            true
        } else {
            false
        }
    }

    pub(crate) async fn get_session_id(&self, request_id: &str) -> Option<String> {
        self.pending
            .lock()
            .await
            .get(request_id)
            .and_then(|p| p.session_id.clone())
    }
}
