//! Provenance of issue changes: which session, agent and tool call made each
//! change may made to an issue, recorded in the history database.
//!
//! The `issue` command writes a record as part of every successful change.
//! An issue or comment with no record was not written by may on this machine.

use aither_core::llm::{AgentIdentity, ToolContext};
use askama::Template;

use crate::db::{ChatDb, IssueProvenanceRecord, NewIssueProvenance, UserMessageAt};

use super::{IssueError, IssueStore};

/// What an issue change did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueAction {
    /// The issue was created.
    Create,
    /// Title and/or body changed.
    Edit,
    /// A comment was added.
    Comment,
    /// The issue was closed.
    Close,
    /// The issue was reopened.
    Reopen,
    /// Labels were added or removed.
    Label,
    /// A relation to another issue was added.
    RelationAdd,
    /// A relation to another issue was removed.
    RelationRemove,
}

impl IssueAction {
    /// Persisted form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Edit => "edit",
            Self::Comment => "comment",
            Self::Close => "close",
            Self::Reopen => "reopen",
            Self::Label => "label",
            Self::RelationAdd => "relation_add",
            Self::RelationRemove => "relation_remove",
        }
    }
}

/// The agent and tool call behind an `issue` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOrigin {
    agent: String,
    tool_call_id: Option<String>,
}

impl CallOrigin {
    /// Who made the call: `main`, `subagent:<name>` or [`Self::EXEC_AGENT`].
    #[must_use]
    pub fn agent(&self) -> &str {
        &self.agent
    }

    /// The model tool call behind the invocation, when it is one.
    #[must_use]
    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }

    /// Agent label recorded for an invocation that is no model tool call,
    /// such as a `may exec` script.
    pub const EXEC_AGENT: &'static str = "exec";

    /// The origin the dispatcher attached to `cx`.
    #[must_use]
    pub fn from_context(cx: &ToolContext) -> Self {
        cx.identity().map_or_else(
            || Self {
                agent: Self::EXEC_AGENT.to_string(),
                tool_call_id: None,
            },
            |identity| Self {
                agent: match identity.agent() {
                    AgentIdentity::Main => "main".to_string(),
                    AgentIdentity::Subagent(subagent) => format!("subagent:{}", subagent.name()),
                },
                tool_call_id: Some(identity.call().as_str().to_string()),
            },
        )
    }
}

/// Issue provenance of one session.
#[derive(Debug, Clone)]
pub struct IssueProvenance {
    db: ChatDb,
    session_id: String,
}

/// One recorded change with what explains it.
#[derive(Debug, Clone)]
pub struct ProvenanceEntry {
    /// The record.
    pub record: IssueProvenanceRecord,
    /// The tool call that made the change, when the session's history holds it.
    pub call: Option<ResolvedCall>,
    /// User messages that preceded the change, newest first.
    pub messages: Vec<UserMessageAt>,
}

/// A tool call as the session's history stored it.
#[derive(Debug, Clone)]
pub struct ResolvedCall {
    /// Tool name.
    pub name: String,
    /// The script the call ran, or its arguments when it has none.
    pub script: String,
}

/// Who wrote an issue and its comments, according to the database.
#[derive(Debug, Clone)]
pub struct IssueMarks {
    /// The session that created the issue, when may created it.
    pub created_in: Option<String>,
    /// Ids of the comments may wrote.
    pub comment_ids: Vec<i64>,
}

#[derive(Template)]
#[template(path = "issue_provenance.txt", escape = "none")]
struct ProvenanceText<'a> {
    number: i64,
    entries: &'a [ProvenanceEntry],
}

impl IssueProvenance {
    /// Provenance written to `db` on behalf of `session_id`.
    #[must_use]
    pub const fn new(db: ChatDb, session_id: String) -> Self {
        Self { db, session_id }
    }

    /// Record a successful change.
    ///
    /// # Errors
    /// Returns the storage error; the change itself has already happened.
    pub async fn record<S: IssueStore>(
        &self,
        store: &S,
        origin: &CallOrigin,
        number: i64,
        comment_id: Option<i64>,
        action: IssueAction,
        detail: Option<&str>,
    ) -> Result<(), IssueError> {
        let backend = store.info().name;
        let repository = store.search_key();
        self.db
            .record_issue_provenance(NewIssueProvenance {
                backend,
                repository: &repository,
                issue_number: number,
                comment_id,
                action: action.as_str(),
                detail,
                session_id: &self.session_id,
                agent: &origin.agent,
                tool_call_id: origin.tool_call_id.as_deref(),
            })
            .await?;
        Ok(())
    }

    /// Who wrote issue `number` and its comments.
    ///
    /// # Errors
    /// Returns the storage error.
    pub async fn marks<S: IssueStore>(
        &self,
        store: &S,
        number: i64,
    ) -> Result<IssueMarks, IssueError> {
        let records = self.records(store, number).await?;
        Ok(IssueMarks {
            created_in: records
                .iter()
                .find(|record| record.action == IssueAction::Create.as_str())
                .map(|record| record.session_id.clone()),
            comment_ids: records
                .iter()
                .filter(|record| record.action == IssueAction::Comment.as_str())
                .filter_map(|record| record.comment_id)
                .collect(),
        })
    }

    /// The records of issue `number`, each resolved to its tool call and the
    /// last `messages` user messages of its session before it.
    ///
    /// # Errors
    /// Returns the storage error.
    pub async fn entries<S: IssueStore>(
        &self,
        store: &S,
        number: i64,
        messages: usize,
    ) -> Result<Vec<ProvenanceEntry>, IssueError> {
        let mut entries = Vec::new();
        for record in self.records(store, number).await? {
            let call = match record.tool_call_id.as_deref() {
                Some(call_id) => self
                    .db
                    .session_tool_call(&record.session_id, call_id)
                    .await?
                    .map(|(name, arguments)| ResolvedCall {
                        script: arguments
                            .get("script")
                            .and_then(serde_json::Value::as_str)
                            .map_or_else(|| arguments.to_string(), str::to_string),
                        name,
                    }),
                None => None,
            };
            let messages = if messages == 0 {
                Vec::new()
            } else {
                self.db
                    .user_messages_before(&record.session_id, &record.recorded_at, messages)
                    .await?
            };
            entries.push(ProvenanceEntry {
                record,
                call,
                messages,
            });
        }
        Ok(entries)
    }

    /// What `issue provenance` shows the model.
    ///
    /// # Errors
    /// Returns the storage error.
    pub async fn render<S: IssueStore>(
        &self,
        store: &S,
        number: i64,
        messages: usize,
    ) -> Result<String, IssueError> {
        let entries = self.entries(store, number, messages).await?;
        ProvenanceText {
            number,
            entries: &entries,
        }
        .render()
        .map_err(|error| IssueError::Invalid(error.to_string()))
    }

    async fn records<S: IssueStore>(
        &self,
        store: &S,
        number: i64,
    ) -> Result<Vec<IssueProvenanceRecord>, IssueError> {
        Ok(self
            .db
            .issue_provenance(store.info().name, &store.search_key(), number)
            .await?)
    }
}
