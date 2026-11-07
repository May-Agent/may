//! The `ask_user` and `withdraw_ask_user` commands, and the context block that
//! keeps open questions in front of the agent.
//!
//! `ask_user` persists the question and returns its request id at once; no
//! run waits for the answer. The question stays open until it is answered or
//! withdrawn, however long that takes and across restarts.

use std::borrow::Cow;
use std::sync::Arc;

use aither_agent::{Block, ContextSource};
use aither_core::llm::{Tool, ToolContext, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::secrets::{SecretScope, SecretStore};
use crate::session::{
    PersistedAskUserQuestion, PersistedAskUserRequest, PersistedSecretRequest,
    SessionRuntimeService,
};

const ASK_USER_DESCRIPTION: &str = include_str!("../texts/ask_user_description.txt");
const WITHDRAW_DESCRIPTION: &str = include_str!("../texts/withdraw_ask_user_description.txt");
/// Tag of the open-questions block in the request tail.
const OPEN_QUESTIONS_TAG: &str = "open_questions";

#[derive(Debug, thiserror::Error)]
enum AskUserToolError {
    #[error("ask_user needs a question")]
    NoQuestion,
    #[error(
        "the question is given both as the positional argument and as --question; give it once"
    )]
    QuestionTwice,
    #[error("a secret needs both a name and a scope (global or session)")]
    IncompleteSecret,
    #[error("the secret question {question:?} is refused: {source}")]
    BadSecret {
        question: String,
        #[source]
        source: crate::secrets::SecretError,
    },
    #[error("the secret question {0:?} takes only the value: it has no options")]
    SecretWithOptions(String),
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
    languages: Option<aither_agent::LanguageGuard>,
    task_events: Option<aither_agent::TaskEvents>,
}

impl AskUserTool {
    /// The command for `session_id`, persisting through `runtime`.
    #[must_use]
    pub fn new(runtime: Arc<SessionRuntimeService>, session_id: impl Into<String>) -> Self {
        Self {
            runtime,
            session_id: session_id.into(),
            languages: None,
            task_events: None,
        }
    }

    /// Record every question asked on `events`, the main agent's `tasks.md`
    /// inbox.
    #[must_use]
    pub fn with_task_events(mut self, events: aither_agent::TaskEvents) -> Self {
        self.task_events = Some(events);
        self
    }

    /// Refuse a question outside the communication language `guard` enforces.
    #[must_use]
    pub fn with_languages(mut self, guard: aither_agent::LanguageGuard) -> Self {
        self.languages = Some(guard);
        self
    }
}

/// The secret a question collects: the user is asked only for its value.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SecretSpec {
    /// Environment variable name the value is stored under and injected as
    /// into terminal commands: `[A-Za-z_][A-Za-z0-9_]*`.
    pub name: String,
    /// `global` reaches every session; `session` only this one and is deleted
    /// with it.
    pub scope: SecretScope,
}

/// A single question of an `ask_user` request.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct AskedQuestion {
    /// Short section label for navigation.
    #[serde(default)]
    pub section: String,
    /// The question text to display.
    pub question: String,
    /// Options to choose from.
    #[serde(default, alias = "options")]
    pub option: Vec<String>,
    /// Allow selecting multiple options.
    #[serde(default)]
    pub multi_select: bool,
    /// Ask for a secret value instead of a choice.
    #[serde(default)]
    pub secret: Option<SecretSpec>,
}

/// The arguments of `ask_user`: one question in the flat fields, or several
/// in `questions`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct AskUserArgs {
    /// The question to ask (for single-question mode).
    #[serde(default)]
    pub question: Option<String>,
    /// The question given as the positional argument
    /// (`ask_user "Which database?" --option PostgreSQL`).
    #[serde(default)]
    #[schemars(extend("x-positional" = true))]
    pub positional_question: Option<String>,
    /// Option to choose from (use --option flag for each, repeatable).
    #[serde(default, alias = "options", alias = "choices")]
    pub option: Vec<String>,
    /// Allow selecting multiple options (for single-question mode).
    #[serde(default)]
    pub multi_select: bool,
    /// Single-question mode: ask for a secret stored under this environment
    /// variable name; needs `secret_scope`.
    #[serde(default)]
    pub secret: Option<String>,
    /// Single-question mode: `global` or `session`.
    #[serde(default)]
    pub secret_scope: Option<SecretScope>,
    /// Multiple questions (JSON array). Overrides the single-question fields
    /// if set.
    #[serde(default)]
    pub questions: Vec<AskedQuestion>,
}

impl AskUserArgs {
    /// The questions asked, validated: a secret name is a valid environment
    /// variable name, and a secret question has no options.
    fn into_questions(self) -> Result<Vec<AskedQuestion>, AskUserToolError> {
        let questions = if self.questions.is_empty() {
            let secret = match (self.secret, self.secret_scope) {
                (Some(name), Some(scope)) => Some(SecretSpec { name, scope }),
                (None, None) => None,
                _ => return Err(AskUserToolError::IncompleteSecret),
            };
            let question = match (self.question, self.positional_question) {
                (Some(_), Some(_)) => return Err(AskUserToolError::QuestionTwice),
                (question, None) | (None, question) => question,
            };
            question
                .map(|question| AskedQuestion {
                    section: String::new(),
                    question,
                    option: self.option,
                    multi_select: self.multi_select,
                    secret,
                })
                .into_iter()
                .collect()
        } else {
            self.questions
        };
        for question in &questions {
            if let Some(secret) = &question.secret {
                SecretStore::validate_name(&secret.name).map_err(|source| {
                    AskUserToolError::BadSecret {
                        question: question.question.clone(),
                        source,
                    }
                })?;
                if !question.option.is_empty() {
                    return Err(AskUserToolError::SecretWithOptions(
                        question.question.clone(),
                    ));
                }
            }
        }
        Ok(questions)
    }
}

fn persisted_questions(questions: Vec<AskedQuestion>) -> Vec<PersistedAskUserQuestion> {
    questions
        .into_iter()
        .map(|question| {
            let secret = question.secret.map(|secret| PersistedSecretRequest {
                name: secret.name,
                scope: secret.scope,
            });
            // Answers are matched to questions by section, so a secret
            // question without one is told apart by its name.
            let section = match (&secret, question.section.is_empty()) {
                (Some(secret), true) => secret.name.clone(),
                _ => question.section,
            };
            PersistedAskUserQuestion {
                section,
                question: question.question,
                option: question.option,
                multi_select: question.multi_select && secret.is_none(),
                secret,
            }
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
        let questions = args.into_questions()?;
        if questions.is_empty() {
            return Err(AskUserToolError::NoQuestion.into());
        }
        if let Some(guard) = &self.languages {
            guard.check_question(questions.iter().flat_map(|question| {
                std::iter::once(question.question.as_str())
                    .chain(question.option.iter().map(String::as_str))
            }))?;
        }
        let request = PersistedAskUserRequest {
            id: crate::id::new(),
            questions: persisted_questions(questions),
        };
        self.runtime
            .open_question(&self.session_id, &request)
            .await
            .map_err(AskUserToolError::Persist)?;
        self.runtime.announce_user_wait(crate::session::UserWait {
            session_id: self.session_id.clone(),
            ask: crate::session::UserAsk::question(&request),
        });
        if let Some(events) = &self.task_events {
            events.record(aither_agent::TaskEvent::QuestionAsked(request.id.clone()));
        }
        Ok(ToolResult::text(
            AskedText {
                request_id: &request.id,
            }
            .render()
            .expect("ask_user result template must render"),
        ))
    }
}

/// Withdraw an open `ask_user` question by its `request_id`.
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
    task_events: Option<aither_agent::TaskEvents>,
}

impl WithdrawAskUserTool {
    /// The command for `session_id`, persisting through `runtime`.
    #[must_use]
    pub fn new(runtime: Arc<SessionRuntimeService>, session_id: impl Into<String>) -> Self {
        Self {
            runtime,
            session_id: session_id.into(),
            task_events: None,
        }
    }

    /// Record every question withdrawn on `events`, the main agent's
    /// `tasks.md` inbox.
    #[must_use]
    pub fn with_task_events(mut self, events: aither_agent::TaskEvents) -> Self {
        self.task_events = Some(events);
        self
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
        if let Some(events) = &self.task_events {
            events.record(aither_agent::TaskEvent::QuestionWithdrawn(
                args.request_id.clone(),
            ));
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

/// Resolves `BLOCKING <what>` payloads in `tasks.md` against the session's
/// `ask_user` history: a mark naming a still-pending question holds; one
/// naming a request the session issued that is answered or withdrawn does
/// not — the item is runnable work again. A payload naming no request the
/// session issued is free text and trusted.
#[derive(Clone)]
pub struct AskUserBlockerResolver {
    runtime: Arc<SessionRuntimeService>,
    session_id: String,
}

impl AskUserBlockerResolver {
    /// The resolver for `session_id`, reading through `runtime`.
    #[must_use]
    pub fn new(runtime: Arc<SessionRuntimeService>, session_id: impl Into<String>) -> Self {
        Self {
            runtime,
            session_id: session_id.into(),
        }
    }
}

impl aither_agent::TaskBlockerResolver for AskUserBlockerResolver {
    async fn is_open(&self, mark: &str) -> Result<bool, aither_agent::TaskBlockerError> {
        let unavailable =
            |error: crate::db::ChatDbError| aither_agent::TaskBlockerError::Unavailable {
                mark: mark.to_string(),
                reason: error.to_string(),
            };
        let db = self.runtime.db();
        // A payload no question of this session carries is free text the
        // harness cannot check, and holds.
        if !db
            .session_ask_user_request_exists(&self.session_id, mark)
            .await
            .map_err(unavailable)?
        {
            return Ok(true);
        }
        Ok(db
            .pending_ask_user_request(mark)
            .await
            .map_err(unavailable)?
            .is_some_and(|pending| pending.session_id == self.session_id))
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aither_sandbox::ToolRegistryBuilder;

    use super::{ASK_USER_DESCRIPTION, AskUserTool};

    /// The `ask_user` command on a fresh session's database, and that database.
    async fn ask_user_registry(
        dir: &tempfile::TempDir,
    ) -> (aither_sandbox::ToolRegistry, crate::db::ChatDb) {
        let db = crate::db::ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("chat db opens");
        db.create_session("session", None, None)
            .await
            .expect("session is created");
        let runtime = Arc::new(crate::session::SessionRuntimeService::new(
            db.clone(),
            crate::ui_event_bus::UiEventBus::new(),
        ));
        let mut builder = ToolRegistryBuilder::new();
        builder.configure_tool(AskUserTool::new(runtime, "session"));
        (builder.build(dir.path().join("outputs")), db)
    }

    /// The single-question form takes the question as its positional
    /// argument, and the call opens that question with its options.
    #[tokio::test]
    async fn the_positional_question_opens_a_question() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (registry, db) = ask_user_registry(&dir).await;
        let words = shell_words::split(r#""Which database?" --option PostgreSQL --option SQLite"#)
            .expect("shell syntax");
        let reply = registry.query_tool_handler("ask_user", &words).await;
        assert!(reply.ok, "{reply:?}");

        let open = db
            .open_ask_user_questions("session")
            .await
            .expect("open questions load");
        assert_eq!(open.len(), 1, "{open:?}");
        let [question] = open[0].questions.as_slice() else {
            panic!("one question: {open:?}");
        };
        assert_eq!(question.question, "Which database?");
        assert_eq!(question.option, ["PostgreSQL", "SQLite"]);

        let twice = shell_words::split(r#""Which database?" --question "Which DB?""#)
            .expect("shell syntax");
        let reply = registry.query_tool_handler("ask_user", &twice).await;
        assert!(!reply.ok, "a question given twice is refused: {reply:?}");
    }

    /// Every `ask_user ...` line of the description is a call the command
    /// line accepts, parsed against the command's real argument schema.
    #[tokio::test]
    async fn every_example_in_the_description_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (registry, _db) = ask_user_registry(&dir).await;

        let examples: Vec<&str> = ASK_USER_DESCRIPTION
            .lines()
            .filter(|line| line.starts_with("ask_user "))
            .collect();
        assert_eq!(examples.len(), 3, "single, several and secret questions");
        for example in examples {
            let words = shell_words::split(example).expect("the example is shell syntax");
            let reply = registry.query_tool_handler("ask_user", &words[1..]).await;
            assert!(reply.ok, "{example}: {reply:?}");
        }
    }
}
