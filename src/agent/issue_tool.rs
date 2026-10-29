//! `issue` terminal command: one CLI over the may and GitHub issue backends.

use std::borrow::Cow;
use std::fmt::Write as _;

use aither_core::llm::{Tool, ToolResult};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::issue::{
    IssueDetail, IssueError, IssueListState, IssueRelationKind, IssueState, IssueStore,
    IssueSummary,
};

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

/// `issue` tool: unified issue tracker over the resolved backend.
#[derive(Debug, Clone)]
pub struct IssueTool<S> {
    store: S,
}

impl<S: IssueStore> IssueTool<S> {
    /// Bind the tool to the session's issue store.
    #[must_use]
    pub const fn new(store: S) -> Self {
        Self { store }
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
        _cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        execute(&self.store, arguments).await.map_err(Into::into)
    }
}

async fn execute<S: IssueStore>(backend: &S, args: IssueArgs) -> Result<ToolResult, IssueError> {
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
            Ok(ToolResult::text(render_detail(&detail)))
        }
        IssueArgs::View { number } => {
            let detail = backend.get(number).await?;
            Ok(ToolResult::text(render_detail(&detail)))
        }
        IssueArgs::List {
            state,
            labels,
            limit,
        } => {
            let summaries = backend.list(state, &labels, limit).await?;
            Ok(ToolResult::text(render_list(&summaries)))
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
            Ok(ToolResult::text(render_detail(&detail)))
        }
        IssueArgs::Comment { number, body } => {
            if body.trim().is_empty() {
                return Err(IssueError::Invalid(
                    "issue comment body must not be empty".to_string(),
                ));
            }
            let detail = backend.comment(number, &body).await?;
            Ok(ToolResult::text(render_detail(&detail)))
        }
        IssueArgs::Close { number } => {
            let detail = backend
                .edit(number, None, None, Some(IssueState::Closed))
                .await?;
            Ok(ToolResult::text(render_detail(&detail)))
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
            Ok(ToolResult::text(render_detail(&detail)))
        }
        IssueArgs::BlockedBy {
            number,
            add,
            remove,
        } => edit_relations(backend, number, IssueRelationKind::BlockedBy, &add, &remove).await,
        IssueArgs::SubIssue {
            number,
            add,
            remove,
        } => edit_relations(backend, number, IssueRelationKind::SubIssue, &add, &remove).await,
    }
}

async fn edit_relations<S: IssueStore>(
    backend: &S,
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
    }
    for target in remove {
        detail = backend.edit_relation(number, kind, *target, false).await?;
    }
    Ok(ToolResult::text(render_detail(&detail)))
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

fn render_detail(detail: &IssueDetail) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "#{} {}\nstate: {}",
        detail.number,
        detail.title,
        render_state(detail.state)
    );
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
            let _ = writeln!(out, "- [{}]{} {}", comment.created_at, author, comment.body);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
