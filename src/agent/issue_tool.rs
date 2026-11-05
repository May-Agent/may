//! `issue` terminal command: one CLI over the may and GitHub issue backends.

use std::borrow::Cow;
use std::fmt::Write as _;

use aither_core::llm::{Tool, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::issue::provenance::{CallOrigin, IssueAction, IssueMarks, IssueProvenance};
use crate::issue::search::{IssueHit, IssueSearch};
use crate::issue::{
    IssueDetail, IssueError, IssueListState, IssueRelationKind, IssueState, IssueStore,
    IssueSummary,
};
use crate::repo_search::RepoSearchCollections;

/// Issue search as the `issue` command runs it.
pub type SessionIssueSearch = IssueSearch<RepoSearchCollections>;

const fn default_search_limit() -> usize {
    10
}

const fn default_search_state() -> IssueListState {
    IssueListState::All
}

const fn default_provenance_messages() -> usize {
    3
}

const fn default_list_limit() -> i64 {
    50
}

const fn default_list_state() -> IssueListState {
    IssueListState::Open
}

/// Track work as issues: create, view, list, edit, comment, close, label,
/// blocked-by and sub-issue on the session's issue backend.
///
/// Subcommands:
/// - `issue create <title> [--body text] [--labels name ...]`
/// - `issue view <number>`
/// - `issue list [--state open|closed|all] [--labels name ...] [--limit n]`
/// - `issue search <query> [--state open|closed|all] [--limit n]` — issues
///   whose title, body or comments match the described topic
/// - `issue provenance <number> [--messages n]` — the changes may made to the
///   issue, with the session, agent, tool call and preceding user messages
/// - `issue edit <number> [--title t] [--body b] [--state open|closed]`
/// - `issue comment <number> <body>`
/// - `issue close <number>` (reopen with `edit --state open`)
/// - `issue label <number> [--add name ...] [--remove name ...]`
/// - `issue blocked-by <number> [--add n ...] [--remove n ...]` — the issue
///   is blocked by the added issue numbers
/// - `issue sub-issue <number> [--add n ...] [--remove n ...]` — the added
///   issue numbers become sub-issues
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum IssueArgs {
    /// Create an issue.
    Create {
        /// Issue title.
        title: String,
        /// Issue body (markdown).
        #[serde(default)]
        body: Option<String>,
        /// Labels to apply.
        #[serde(default)]
        labels: Vec<String>,
    },
    /// View one issue with its body, comments and relations.
    View {
        /// Issue number.
        number: i64,
    },
    /// List issues, newest first.
    List {
        /// State filter: `open`, `closed` or `all`.
        #[serde(default = "default_list_state")]
        state: IssueListState,
        /// Only issues carrying every listed label.
        #[serde(default)]
        labels: Vec<String>,
        /// Maximum issues returned.
        #[serde(default = "default_list_limit")]
        limit: i64,
    },
    /// List the changes may made to an issue: session, agent, tool call and
    /// the user messages that explain each.
    Provenance {
        /// Issue number.
        number: i64,
        /// User messages shown per record.
        #[serde(default = "default_provenance_messages")]
        messages: usize,
    },
    /// Find issues by what they are about, in natural language.
    Search {
        /// What the issues are about.
        query: String,
        /// State filter: `open`, `closed` or `all` (default).
        #[serde(default = "default_search_state")]
        state: IssueListState,
        /// Maximum issues returned.
        #[serde(default = "default_search_limit")]
        limit: usize,
    },
    /// Edit an issue's title, body and/or state.
    Edit {
        /// Issue number.
        number: i64,
        /// New title.
        #[serde(default)]
        title: Option<String>,
        /// New body.
        #[serde(default)]
        body: Option<String>,
        /// New state (`open` reopens, `closed` closes).
        #[serde(default)]
        state: Option<IssueState>,
    },
    /// Comment on an issue.
    Comment {
        /// Issue number.
        number: i64,
        /// Comment body.
        body: String,
    },
    /// Close an issue.
    Close {
        /// Issue number.
        number: i64,
    },
    /// Add and/or remove labels on an issue.
    Label {
        /// Issue number.
        number: i64,
        /// Labels to add (created on the backend when missing).
        #[serde(default)]
        add: Vec<String>,
        /// Labels to remove.
        #[serde(default)]
        remove: Vec<String>,
    },
    /// Mark the issue as blocked by other issues, or unblock it.
    #[serde(rename = "blocked-by")]
    BlockedBy {
        /// Issue number.
        number: i64,
        /// Issue numbers blocking this issue.
        #[serde(default)]
        add: Vec<i64>,
        /// Issue numbers no longer blocking this issue.
        #[serde(default)]
        remove: Vec<i64>,
    },
    /// Attach or detach sub-issues.
    #[serde(rename = "sub-issue")]
    SubIssue {
        /// Issue number of the parent issue.
        number: i64,
        /// Issue numbers to attach as sub-issues.
        #[serde(default)]
        add: Vec<i64>,
        /// Issue numbers to detach.
        #[serde(default)]
        remove: Vec<i64>,
    },
}

impl IssueArgs {
    /// The subcommand name as typed on the command line.
    const fn subcommand(&self) -> &'static str {
        match self {
            Self::Create { .. } => "create",
            Self::View { .. } => "view",
            Self::List { .. } => "list",
            Self::Search { .. } => "search",
            Self::Provenance { .. } => "provenance",
            Self::Edit { .. } => "edit",
            Self::Comment { .. } => "comment",
            Self::Close { .. } => "close",
            Self::Label { .. } => "label",
            Self::BlockedBy { .. } => "blocked-by",
            Self::SubIssue { .. } => "sub-issue",
        }
    }

    /// Whether the subcommand changes the issue backend.
    const fn writes(&self) -> bool {
        !matches!(
            self,
            Self::View { .. } | Self::List { .. } | Self::Search { .. } | Self::Provenance { .. }
        )
    }
}

/// Refusal of a write subcommand to a subagent.
#[derive(Template)]
#[template(path = "issue_reserved.txt")]
struct IssueReserved {
    subcommand: &'static str,
}

/// What the model reads for `issue search`.
#[derive(Template)]
#[template(path = "issue_search.txt", escape = "none")]
struct IssueSearchText<'a> {
    query: &'a str,
    state: &'a str,
    hits: &'a [IssueHit],
}

/// `issue` tool: unified issue tracker over the resolved backend.
#[derive(Debug, Clone)]
pub struct IssueTool<S> {
    store: S,
    provenance: IssueProvenance,
    search: Option<SessionIssueSearch>,
}

/// The `issue` tool as a subagent sees it: the read subcommands work and every
/// write subcommand is refused before it reaches the backend.
#[derive(Debug, Clone)]
pub struct SubagentIssueTool<S> {
    store: S,
    provenance: IssueProvenance,
    search: Option<SessionIssueSearch>,
}

impl<S: IssueStore> SubagentIssueTool<S> {
    /// Bind the read-only tool to the session's issue store.
    #[must_use]
    pub const fn new(store: S, provenance: IssueProvenance) -> Self {
        Self {
            store,
            provenance,
            search: None,
        }
    }

    /// Enable `issue search` through `search`.
    #[must_use]
    pub fn with_search(mut self, search: SessionIssueSearch) -> Self {
        self.search = Some(search);
        self
    }
}

impl<S: IssueStore + Clone + 'static> Tool for SubagentIssueTool<S> {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("issue")
    }

    type Arguments = IssueArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        arguments: Self::Arguments,
        cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        if arguments.writes() {
            let refusal = IssueReserved {
                subcommand: arguments.subcommand(),
            }
            .render()?;
            return Err(IssueError::Reserved(refusal).into());
        }
        execute(
            &self.store,
            &self.provenance,
            &CallOrigin::from_context(&cx),
            self.search.as_ref(),
            arguments,
        )
        .await
        .map_err(Into::into)
    }
}

impl<S: IssueStore> IssueTool<S> {
    /// Bind the tool to the session's issue store.
    #[must_use]
    pub const fn new(store: S, provenance: IssueProvenance) -> Self {
        Self {
            store,
            provenance,
            search: None,
        }
    }

    /// Enable `issue search` through `search`.
    #[must_use]
    pub fn with_search(mut self, search: SessionIssueSearch) -> Self {
        self.search = Some(search);
        self
    }
}

impl<S: IssueStore + Clone + 'static> Tool for IssueTool<S> {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("issue")
    }

    type Arguments = IssueArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        arguments: Self::Arguments,
        cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        execute(
            &self.store,
            &self.provenance,
            &CallOrigin::from_context(&cx),
            self.search.as_ref(),
            arguments,
        )
        .await
        .map_err(Into::into)
    }
}

async fn execute<S: IssueStore>(
    backend: &S,
    provenance: &IssueProvenance,
    origin: &CallOrigin,
    search: Option<&SessionIssueSearch>,
    args: IssueArgs,
) -> Result<ToolResult, IssueError> {
    let record = |number: i64,
                  comment_id: Option<i64>,
                  action: IssueAction,
                  detail: Option<String>| async move {
        provenance
            .record(
                backend,
                origin,
                number,
                comment_id,
                action,
                detail.as_deref(),
            )
            .await
    };
    match args {
        IssueArgs::Create {
            title,
            body,
            labels,
        } => {
            if title.trim().is_empty() {
                return Err(IssueError::Invalid(
                    "issue title must not be empty".to_string(),
                ));
            }
            let detail = backend
                .create(title.trim(), body.as_deref().unwrap_or(""), &labels)
                .await?;
            record(detail.number, None, IssueAction::Create, None).await?;
            Ok(ToolResult::text(render_detail(&detail, None)))
        }
        IssueArgs::View { number } => {
            let detail = backend.get(number).await?;
            let marks = provenance.marks(backend, number).await?;
            Ok(ToolResult::text(render_detail(&detail, Some(&marks))))
        }
        IssueArgs::Provenance { number, messages } => Ok(ToolResult::text(
            provenance.render(backend, number, messages).await?,
        )),
        IssueArgs::List {
            state,
            labels,
            limit,
        } => {
            let summaries = backend.list(state, &labels, limit).await?;
            Ok(ToolResult::text(render_list(&summaries)))
        }
        IssueArgs::Search {
            query,
            state,
            limit,
        } => {
            let Some(search) = search else {
                return Err(IssueError::Search(
                    "this session has no search index".to_string(),
                ));
            };
            let hits = search
                .search(backend, query.trim(), state, limit.clamp(1, 50))
                .await?;
            let text = IssueSearchText {
                query: query.trim(),
                state: state.as_str(),
                hits: &hits,
            }
            .render()
            .map_err(|error| IssueError::Search(error.to_string()))?;
            Ok(ToolResult::text(text))
        }
        IssueArgs::Edit {
            number,
            title,
            body,
            state,
        } => {
            if title.is_none() && body.is_none() && state.is_none() {
                return Err(IssueError::Invalid(
                    "issue edit requires at least one of --title, --body or --state".to_string(),
                ));
            }
            let detail = backend
                .edit(number, title.as_deref(), body.as_deref(), state)
                .await?;
            let touched = [("title", title.is_some()), ("body", body.is_some())]
                .into_iter()
                .filter_map(|(name, changed)| changed.then_some(name))
                .collect::<Vec<_>>();
            if !touched.is_empty() {
                record(number, None, IssueAction::Edit, Some(touched.join(", "))).await?;
            }
            match state {
                Some(IssueState::Closed) => record(number, None, IssueAction::Close, None).await?,
                Some(IssueState::Open) => record(number, None, IssueAction::Reopen, None).await?,
                None => {}
            }
            Ok(ToolResult::text(render_detail(&detail, None)))
        }
        IssueArgs::Comment { number, body } => {
            if body.trim().is_empty() {
                return Err(IssueError::Invalid(
                    "issue comment body must not be empty".to_string(),
                ));
            }
            let commented = backend.comment(number, &body).await?;
            record(
                number,
                Some(commented.comment_id),
                IssueAction::Comment,
                None,
            )
            .await?;
            Ok(ToolResult::text(render_detail(&commented.detail, None)))
        }
        IssueArgs::Close { number } => {
            let detail = backend
                .edit(number, None, None, Some(IssueState::Closed))
                .await?;
            record(number, None, IssueAction::Close, None).await?;
            Ok(ToolResult::text(render_detail(&detail, None)))
        }
        IssueArgs::Label {
            number,
            add,
            remove,
        } => {
            if add.is_empty() && remove.is_empty() {
                return Err(IssueError::Invalid(
                    "issue label requires --add or --remove".to_string(),
                ));
            }
            let detail = backend.edit_labels(number, &add, &remove).await?;
            let changes = add
                .iter()
                .map(|label| format!("+{label}"))
                .chain(remove.iter().map(|label| format!("-{label}")))
                .collect::<Vec<_>>()
                .join(" ");
            record(number, None, IssueAction::Label, Some(changes)).await?;
            Ok(ToolResult::text(render_detail(&detail, None)))
        }
        IssueArgs::BlockedBy {
            number,
            add,
            remove,
        } => {
            edit_relations(
                backend,
                provenance,
                origin,
                number,
                IssueRelationKind::BlockedBy,
                &add,
                &remove,
            )
            .await
        }
        IssueArgs::SubIssue {
            number,
            add,
            remove,
        } => {
            edit_relations(
                backend,
                provenance,
                origin,
                number,
                IssueRelationKind::SubIssue,
                &add,
                &remove,
            )
            .await
        }
    }
}

async fn edit_relations<S: IssueStore>(
    backend: &S,
    provenance: &IssueProvenance,
    origin: &CallOrigin,
    number: i64,
    kind: IssueRelationKind,
    add: &[i64],
    remove: &[i64],
) -> Result<ToolResult, IssueError> {
    if add.is_empty() && remove.is_empty() {
        return Err(IssueError::Invalid(format!(
            "issue {} requires --add or --remove",
            kind.as_str().replace('_', "-")
        )));
    }
    let mut detail = backend.get(number).await?;
    for target in add {
        detail = backend.edit_relation(number, kind, *target, true).await?;
        let change = format!("{} #{target}", kind.as_str());
        provenance
            .record(
                backend,
                origin,
                number,
                None,
                IssueAction::RelationAdd,
                Some(&change),
            )
            .await?;
    }
    for target in remove {
        detail = backend.edit_relation(number, kind, *target, false).await?;
        let change = format!("{} #{target}", kind.as_str());
        provenance
            .record(
                backend,
                origin,
                number,
                None,
                IssueAction::RelationRemove,
                Some(&change),
            )
            .await?;
    }
    Ok(ToolResult::text(render_detail(&detail, None)))
}

const fn render_state(state: IssueState) -> &'static str {
    state.as_str()
}

fn render_list(summaries: &[IssueSummary]) -> String {
    if summaries.is_empty() {
        return "no issues".to_string();
    }
    let mut out = String::new();
    for summary in summaries {
        let labels = if summary.labels.is_empty() {
            String::new()
        } else {
            format!(" [{}]", summary.labels.join(", "))
        };
        let _ = writeln!(
            out,
            "#{}\t{}\t{}{}",
            summary.number,
            render_state(summary.state),
            summary.title,
            labels
        );
    }
    out
}

fn render_related(related: &[crate::issue::RelatedIssue]) -> String {
    related
        .iter()
        .map(|issue| format!("#{} ({})", issue.number, render_state(issue.state)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_detail(detail: &IssueDetail, marks: Option<&IssueMarks>) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "#{} {}\nstate: {}",
        detail.number,
        detail.title,
        render_state(detail.state)
    );
    if let Some(marks) = marks {
        match &marks.created_in {
            Some(session) => {
                let _ = writeln!(out, "written by may: yes (session {session})");
            }
            None => {
                let _ = writeln!(
                    out,
                    "written by may: no (not created by may on this machine)"
                );
            }
        }
    }
    if !detail.labels.is_empty() {
        let _ = writeln!(out, "labels: {}", detail.labels.join(", "));
    }
    if !detail.sub_issues.is_empty() {
        let _ = writeln!(out, "sub-issues: {}", render_related(&detail.sub_issues));
    }
    if !detail.blocked_by.is_empty() {
        let _ = writeln!(out, "blocked-by: {}", render_related(&detail.blocked_by));
    }
    let _ = writeln!(
        out,
        "created: {}\nupdated: {}",
        detail.created_at, detail.updated_at
    );
    if !detail.body.trim().is_empty() {
        out.push('\n');
        out.push_str(detail.body.trim());
        out.push('\n');
    }
    if !detail.comments.is_empty() {
        let _ = writeln!(out, "\ncomments ({}):", detail.comments.len());
        for comment in &detail.comments {
            let author = comment
                .author
                .as_deref()
                .map(|author| format!(" {author}"))
                .unwrap_or_default();
            let mark = marks.map_or("", |marks| {
                if marks.comment_ids.contains(&comment.id) {
                    " [written by may]"
                } else {
                    " [not written by may]"
                }
            });
            let _ = writeln!(
                out,
                "- [{}]{}{} {}",
                comment.created_at, author, mark, comment.body
            );
        }
    }
    out
}

/// Shared checks of the subagent's `issue` tool, run against each backend.
#[cfg(test)]
pub mod subagent_checks {
    use aither_core::llm::ToolContext;

    use super::*;

    /// One of every subcommand that changes the backend.
    pub fn write_args() -> Vec<IssueArgs> {
        vec![
            IssueArgs::Create {
                title: "t".to_string(),
                body: None,
                labels: Vec::new(),
            },
            IssueArgs::Edit {
                number: 1,
                title: Some("t".to_string()),
                body: None,
                state: None,
            },
            IssueArgs::Comment {
                number: 1,
                body: "b".to_string(),
            },
            IssueArgs::Close { number: 1 },
            IssueArgs::Label {
                number: 1,
                add: vec!["bug".to_string()],
                remove: Vec::new(),
            },
            IssueArgs::BlockedBy {
                number: 1,
                add: vec![2],
                remove: Vec::new(),
            },
            IssueArgs::SubIssue {
                number: 1,
                add: vec![2],
                remove: Vec::new(),
            },
        ]
    }

    /// Every write subcommand is refused with the reserved-for-main-agent text.
    pub async fn assert_writes_refused<S: IssueStore + Clone + 'static>(
        tool: &SubagentIssueTool<S>,
    ) {
        for args in write_args() {
            let subcommand = args.subcommand();
            let error = tool
                .call(args, ToolContext::new())
                .await
                .expect_err("a subagent's write must be refused");
            let text = error.to_string();
            assert!(
                text.contains(&format!(
                    "`issue {subcommand}` is reserved for the main agent"
                )),
                "{subcommand}: {text}"
            );
        }
    }

    /// Both read subcommands run against the backend.
    pub async fn assert_reads_work<S: IssueStore + Clone + 'static>(
        tool: &SubagentIssueTool<S>,
        number: i64,
    ) {
        let viewed = tool
            .call(IssueArgs::View { number }, ToolContext::new())
            .await
            .expect("a subagent may view an issue");
        assert!(format!("{viewed:?}").contains(&format!("#{number}")));
        tool.call(
            IssueArgs::List {
                state: IssueListState::All,
                labels: Vec::new(),
                limit: 10,
            },
            ToolContext::new(),
        )
        .await
        .expect("a subagent may list issues");
    }
}

/// Shared checks of the provenance `issue` leaves, run against each backend.
#[cfg(test)]
pub mod provenance_checks {
    use aither_core::llm::{AgentIdentity, CallIdentity, ToolCallId, ToolContext};

    use crate::db::{ChatDb, MessageMeta};
    use crate::session::MessageRole;

    use super::*;

    const SESSION: &str = "session-prov";

    fn call(id: &str) -> ToolContext {
        ToolContext::new()
            .with_identity(CallIdentity::new(ToolCallId::new(id), AgentIdentity::Main))
    }

    async fn run<S: IssueStore + Clone + 'static>(
        tool: &IssueTool<S>,
        id: &str,
        args: IssueArgs,
    ) -> String {
        let result = tool.call(args, call(id)).await.expect("issue command");
        format!("{result:?}")
    }

    /// Create, edit, comment and close issue `number` (the first issue the
    /// backend hands out) through `tool`, then read the provenance back.
    /// `outside` is an issue the backend holds that `tool` never touched.
    pub async fn assert_changes_are_recorded<S: IssueStore + Clone + 'static>(
        db: &ChatDb,
        store: S,
        number: i64,
        outside: i64,
    ) {
        let tool = IssueTool::new(
            store.clone(),
            IssueProvenance::new(db.clone(), SESSION.to_string()),
        );
        let steps = [
            ("call_create", "issue create 'first'".to_string()),
            ("call_edit", format!("issue edit {number} --title second")),
            ("call_comment", format!("issue comment {number} hello")),
            ("call_close", format!("issue close {number}")),
        ];
        db.append_message(
            SESSION,
            MessageRole::User,
            "please track this bug",
            None,
            MessageMeta::default(),
        )
        .await
        .expect("user message");
        let calls = steps
            .iter()
            .map(|(id, script)| {
                serde_json::json!({"id": id, "name": "terminal", "arguments": {"script": script}})
            })
            .collect::<Vec<_>>();
        let calls = serde_json::Value::Array(calls).to_string();
        db.append_message(
            SESSION,
            MessageRole::Assistant,
            "",
            None,
            MessageMeta {
                tool_calls: Some(&calls),
                ..MessageMeta::default()
            },
        )
        .await
        .expect("assistant message");

        run(
            &tool,
            "call_create",
            IssueArgs::Create {
                title: "first".to_string(),
                body: None,
                labels: Vec::new(),
            },
        )
        .await;
        run(
            &tool,
            "call_edit",
            IssueArgs::Edit {
                number,
                title: Some("second".to_string()),
                body: None,
                state: None,
            },
        )
        .await;
        run(
            &tool,
            "call_comment",
            IssueArgs::Comment {
                number,
                body: "hello".to_string(),
            },
        )
        .await;
        run(&tool, "call_close", IssueArgs::Close { number }).await;
        db.append_message(
            SESSION,
            MessageRole::User,
            "a later message",
            None,
            MessageMeta::default(),
        )
        .await
        .expect("later user message");

        let records = db
            .issue_provenance(store.info().name, &store.search_key(), number)
            .await
            .expect("records");
        let actions = records
            .iter()
            .map(|record| record.action.as_str())
            .collect::<Vec<_>>();
        assert_eq!(actions, ["create", "edit", "comment", "close"]);
        for (record, (call_id, _)) in records.iter().zip(&steps) {
            assert_eq!(record.session_id, SESSION);
            assert_eq!(record.agent, "main");
            assert_eq!(record.tool_call_id.as_deref(), Some(*call_id));
            let at = record
                .recorded_at
                .parse::<jiff::Timestamp>()
                .expect("UTC time");
            assert!(record.recorded_at.ends_with('Z'), "{at}");
        }
        assert!(records[2].comment_id.is_some());

        let report = run(
            &tool,
            "call_read",
            IssueArgs::Provenance {
                number,
                messages: 5,
            },
        )
        .await;
        for expected in [
            "call_create",
            "call_close",
            &format!("ran terminal: issue comment {number} hello"),
            "please track this bug",
            SESSION,
        ] {
            assert!(report.contains(expected), "missing {expected}: {report}");
        }
        assert!(
            !report.contains("a later message"),
            "later messages are no explanation: {report}"
        );

        let viewed = run(&tool, "call_view", IssueArgs::View { number }).await;
        assert!(viewed.contains("written by may: yes"), "{viewed}");
        assert!(viewed.contains("[written by may] hello"), "{viewed}");
        let viewed = run(&tool, "call_view", IssueArgs::View { number: outside }).await;
        assert!(viewed.contains("written by may: no"), "{viewed}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn changes_to_may_issues_are_recorded_with_their_session_agent_and_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::db::ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("chat db should open");
        let store = crate::issue::MayIssueStore::new(db.clone(), "test".to_string());
        // Issue 1 exists without the tool; the tool then creates issue 2.
        let outside = store.create("outside", "", &[]).await.expect("outside");
        provenance_checks::assert_changes_are_recorded(
            &db,
            store,
            outside.number + 1,
            outside.number,
        )
        .await;
    }

    #[tokio::test]
    async fn a_subagent_reads_issues_and_its_writes_never_reach_the_may_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::db::ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("chat db should open");
        let store = crate::issue::MayIssueStore::new(db.clone(), "test".to_string());
        let created = store.create("seed", "body", &[]).await.expect("seed issue");
        let before = store
            .list(IssueListState::All, &[], 50)
            .await
            .expect("list");

        let tool = SubagentIssueTool::new(
            store.clone(),
            IssueProvenance::new(db.clone(), "test".to_string()),
        );
        subagent_checks::assert_reads_work(&tool, created.number).await;
        subagent_checks::assert_writes_refused(&tool).await;

        let after = store
            .list(IssueListState::All, &[], 50)
            .await
            .expect("list");
        assert_eq!(before.len(), after.len());
        let detail = store.get(created.number).await.expect("view");
        assert_eq!(detail.title, "seed");
        assert!(detail.comments.is_empty() && detail.labels.is_empty());
    }

    #[test]
    fn args_schema_uses_subcommand_tag_shape() {
        // The IPC bridge parses `issue <subcommand> ...` by matching the
        // first positional against the `action` tag of a `oneOf` schema —
        // this is the contract `IssueArgs` must keep.
        let schema = serde_json::to_value(schemars::schema_for!(IssueArgs)).expect("schema");
        let variants = schema["oneOf"].as_array().expect("oneOf schema");
        let tags: Vec<&str> = variants
            .iter()
            .filter_map(|variant| variant["properties"]["action"]["const"].as_str())
            .collect();
        for expected in [
            "create",
            "view",
            "list",
            "edit",
            "comment",
            "close",
            "label",
            "blocked-by",
            "sub-issue",
        ] {
            assert!(
                tags.contains(&expected),
                "missing subcommand {expected}: {tags:?}"
            );
        }
    }

    #[test]
    fn subcommands_deserialize_from_wire_json() {
        match serde_json::from_value::<IssueArgs>(serde_json::json!({
            "action": "create",
            "title": "a title",
            "body": "text",
            "labels": ["bug"],
        }))
        .expect("create")
        {
            IssueArgs::Create {
                title,
                body,
                labels,
            } => {
                assert_eq!(title, "a title");
                assert_eq!(body.as_deref(), Some("text"));
                assert_eq!(labels, ["bug".to_string()]);
            }
            other => panic!("unexpected args: {other:?}"),
        }
        match serde_json::from_value::<IssueArgs>(serde_json::json!({
            "action": "blocked-by",
            "number": 1,
            "add": [2, 3],
        }))
        .expect("blocked-by")
        {
            IssueArgs::BlockedBy {
                number,
                add,
                remove,
            } => {
                assert_eq!(number, 1);
                assert_eq!(add, [2, 3]);
                assert_eq!(remove, [] as [i64; 0]);
            }
            other => panic!("unexpected args: {other:?}"),
        }
        match serde_json::from_value::<IssueArgs>(serde_json::json!({
            "action": "sub-issue",
            "number": 5,
            "remove": [6],
        }))
        .expect("sub-issue")
        {
            IssueArgs::SubIssue {
                number,
                add,
                remove,
            } => {
                assert_eq!(number, 5);
                assert_eq!(add, [] as [i64; 0]);
                assert_eq!(remove, [6]);
            }
            other => panic!("unexpected args: {other:?}"),
        }
        match serde_json::from_value::<IssueArgs>(serde_json::json!({
            "action": "list",
            "state": "all",
            "limit": 10,
        }))
        .expect("list")
        {
            IssueArgs::List {
                state,
                labels,
                limit,
            } => {
                assert_eq!(state, IssueListState::All);
                assert_eq!(labels, [] as [String; 0]);
                assert_eq!(limit, 10);
            }
            other => panic!("unexpected args: {other:?}"),
        }
    }
}
