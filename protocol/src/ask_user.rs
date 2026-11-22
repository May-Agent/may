//! The `ask_user` wire types: the questions an agent puts to the user, the
//! answers it gets back, and the persisted request a refresh recovers.
//!
//! The channel the requests travel on and the tool that asks them live in
//! `may-agent`'s harness; the secret store the collected values land in is
//! `may-config`'s. What a card, a client or the database reads is here.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A single question within an `ask_user` request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Question {
    /// Short section label for navigation (e.g. "database", "theme").
    pub section: String,
    /// The question text to display.
    pub question: String,
    /// Options to choose from.
    #[serde(alias = "options")]
    pub option: Vec<String>,
    /// Allow selecting multiple options.
    #[serde(default)]
    pub multi_select: bool,
}

/// Ask the user one or more questions with predefined options.
///
/// Displays an interactive prompt where the user picks from options,
/// types a custom answer, or skips the question.
///
/// # Single question (simple form)
///
/// ```bash
/// ask_user "Which database?" --option PostgreSQL --option MySQL --option SQLite
/// ```
///
/// # Multiple questions (use --questions with JSON)
///
/// ```bash
/// ask_user --questions '[
///   {"section":"database","question":"Which DB?","option":["PostgreSQL","MySQL","SQLite"]},
///   {"section":"theme","question":"Color theme?","option":["Light","Dark","System"]}
/// ]'
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AskUserArgs {
    /// The question to ask (for single-question mode).
    #[serde(default)]
    pub question: Option<String>,
    /// Option to choose from (use --option flag for each, repeatable).
    #[serde(default, alias = "options", alias = "choices")]
    pub option: Vec<String>,
    /// Allow selecting multiple options (for single-question mode).
    #[serde(default)]
    pub multi_select: bool,
    /// Multiple questions (JSON array). Overrides `question/options/multi_select` if set.
    #[serde(default)]
    pub questions: Vec<Question>,
}

impl AskUserArgs {
    /// Normalize into a list of questions.
    /// If `questions` is non-empty, use that directly.
    /// Otherwise, build a single question from the flat fields.
    #[must_use]
    pub fn into_questions(self) -> Vec<Question> {
        if !self.questions.is_empty() {
            return self.questions;
        }
        if let Some(q) = self.question {
            vec![Question {
                section: String::new(),
                question: q,
                option: self.option,
                multi_select: self.multi_select,
            }]
        } else {
            Vec::new()
        }
    }
}

/// The user's answer to a single question.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuestionAnswer {
    /// Section label (matches `Question::section`).
    pub section: String,
    /// Selected option values, or custom text.
    pub answer: AnswerValue,
}

/// Possible answer values.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum AnswerValue {
    /// User selected from predefined options.
    Selected(Vec<String>),
    /// User typed a custom answer.
    Custom(String),
    /// User skipped this question.
    Skipped,
}

impl std::fmt::Display for AnswerValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Selected(options) => write!(f, "{}", options.join(", ")),
            Self::Custom(text) => f.write_str(text),
            Self::Skipped => f.write_str("(skipped)"),
        }
    }
}

/// Persisted ask-user request visible after refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersistedAskUserRequest {
    /// Request id.
    pub id: String,
    /// Questions presented to the user.
    pub questions: Vec<PersistedAskUserQuestion>,
}

/// Persisted ask-user question shape for refresh-safe UI recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersistedAskUserQuestion {
    /// Section the question is grouped under.
    pub section: String,
    /// Question text presented to the user.
    pub question: String,
    /// Selectable options for the question.
    pub option: Vec<String>,
    /// Whether multiple options may be chosen.
    pub multi_select: bool,
    /// Set when the question asks for a secret value instead of a choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<PersistedSecretRequest>,
}

/// The secret an `ask_user` question collects: its name and who may read it.
/// The value is never part of the question or its answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersistedSecretRequest {
    /// Environment variable name the secret is stored and injected under.
    pub name: String,
    /// Whether the secret is global or held by the asking session.
    pub scope: crate::secrets::SecretScope,
}
