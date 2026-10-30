//! The `ask_user` and `withdraw_ask_user` commands, and the context block that
//! keeps open questions in front of the agent.
//!
//! `ask_user` persists the question and returns its request id at once; no
//! run waits for the answer. The question stays open until it is answered or
//! withdrawn, however long that takes and across restarts.

use std::borrow::Cow;
use std::sync::Arc;

use aither_agent::ask_user::{AskUserArgs, Question};
use aither_agent::{Block, ContextSource};
use aither_core::llm::{Tool, ToolContext, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::session::{PersistedAskUserQuestion, PersistedAskUserRequest, SessionRuntimeService};

const ASK_USER_DESCRIPTION: &str = include_str!("../texts/ask_user_description.txt");
const WITHDRAW_DESCRIPTION: &str = include_str!("../texts/withdraw_ask_user_description.txt");
/// Tag of the open-questions block in the request tail.
const OPEN_QUESTIONS_TAG: &str = "open_questions";

#[derive(Debug, thiserror::Error)]
enum AskUserToolError {
    #[error("ask_user needs a question")]
    NoQuestion,
    #[error("failed to persist the question: {0}")]
    Persist(#[source] crate::db::ChatDbError),
    #[error("question {0} is not open: it was already answered or withdrawn")]
    NotOpen(String),
    #[error("failed to withdraw the question: {0}")]
    Withdraw(#[source] crate::db::ChatDbError),
}

#[derive(Template)]
#[template(path = "ask_user_asked.txt", escape = "none")]
struct AskedText<'a> {
    request_id: &'a str,
}

#[derive(Template)]
#[template(path = "ask_user_withdrawn.txt", escape = "none")]
struct WithdrawnText<'a> {
    request_id: &'a str,
}

#[derive(Template)]
#[template(path = "context_open_questions.txt", escape = "none")]
struct OpenQuestionsText<'a> {
    requests: &'a [PersistedAskUserRequest],
}

/// Asks the user a question without waiting for the answer.
#[derive(Clone)]
pub struct AskUserTool {
    runtime: Arc<SessionRuntimeService>,
    session_id: String,
}

impl AskUserTool {
    /// The command for `session_id`, persisting through `runtime`.
    #[must_use]
    pub fn new(runtime: Arc<SessionRuntimeService>, session_id: impl Into<String>) -> Self {
        Self {
            runtime,
            session_id: session_id.into(),
        }
    }
}

fn persisted_questions(questions: Vec<Question>) -> Vec<PersistedAskUserQuestion> {
    questions
        .into_iter()
        .map(|question| PersistedAskUserQuestion {
            section: question.section,
            question: question.question,
            option: question.option,
            multi_select: question.multi_select,
        })
        .collect()
}

impl Tool for AskUserTool {
    fn name(&self) -> Cow<'static, str> {
        "ask_user".into()
    }

    fn description(&self) -> Cow<'static, str> {
        ASK_USER_DESCRIPTION.trim().into()
    }

    type Arguments = AskUserArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> aither_core::Result<Self::Res> {
        let questions = args.into_questions();
        if questions.is_empty() {
            return Err(AskUserToolError::NoQuestion.into());
        }
        let request = PersistedAskUserRequest {
            id: uuid::Uuid::new_v4().to_string(),
            questions: persisted_questions(questions),
        };
        self.runtime
            .open_question(&self.session_id, &request)
            .await
            .map_err(AskUserToolError::Persist)?;
        Ok(ToolResult::text(
            AskedText {
                request_id: &request.id,
            }
            .render()
            .expect("ask_user result template must render"),
        ))
    }
}

/// Arguments of `withdraw_ask_user`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WithdrawAskUserArgs {
    /// The request id `ask_user` returned.
    pub request_id: String,
}

/// Withdraws a question that is still open.
#[derive(Clone)]
pub struct WithdrawAskUserTool {
    runtime: Arc<SessionRuntimeService>,
    session_id: String,
}

impl WithdrawAskUserTool {
    /// The command for `session_id`, persisting through `runtime`.
    #[must_use]
    pub fn new(runtime: Arc<SessionRuntimeService>, session_id: impl Into<String>) -> Self {
        Self {
            runtime,
            session_id: session_id.into(),
        }
    }
}

impl Tool for WithdrawAskUserTool {
    fn name(&self) -> Cow<'static, str> {
        "withdraw_ask_user".into()
    }

    fn description(&self) -> Cow<'static, str> {
        WITHDRAW_DESCRIPTION.trim().into()
    }

    type Arguments = WithdrawAskUserArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> aither_core::Result<Self::Res> {
        let withdrawn = self
            .runtime
            .withdraw_question(&self.session_id, &args.request_id)
            .await
            .map_err(AskUserToolError::Withdraw)?;
        if !withdrawn {
            return Err(AskUserToolError::NotOpen(args.request_id).into());
        }
        Ok(ToolResult::text(
            WithdrawnText {
                request_id: &args.request_id,
            }
            .render()
            .expect("withdraw_ask_user result template must render"),
        ))
    }
}

/// Lists the session's open questions in every request, so they are present
/// after compaction like `tasks.md`: this is session state, rebuilt from the
/// database each time rather than raised once as an alert.
pub struct OpenQuestionsContext {
    runtime: Arc<SessionRuntimeService>,
    session_id: String,
}

impl OpenQuestionsContext {
    /// The context of `session_id`, read through `runtime`.
    #[must_use]
    pub fn new(runtime: Arc<SessionRuntimeService>, session_id: impl Into<String>) -> Self {
        Self {
            runtime,
            session_id: session_id.into(),
        }
    }
}

impl ContextSource for OpenQuestionsContext {
    async fn render(&self) -> Option<Block> {
        let open = match self
            .runtime
            .db()
            .open_ask_user_questions(&self.session_id)
            .await
        {
            Ok(open) => open,
            Err(error) => {
                tracing::error!(session_id = %self.session_id, %error, "failed to load open questions for the request context");
                return None;
            }
        };
        if open.is_empty() {
            return None;
        }
        Some(Block::new(
            OPEN_QUESTIONS_TAG,
            OpenQuestionsText { requests: &open }
                .render()
                .expect("open questions template must render"),
        ))
    }
}
