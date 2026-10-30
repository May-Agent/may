//! `search`: code search over a git worktree through the repo-search daemon.

use std::borrow::Cow;
use std::path::PathBuf;

use aither_core::llm::{Tool, ToolResult};
use askama::Template;
use repo_search_client::client::ClientError;
use repo_search_client::protocol::{
    Final, Format, IndexStatus, ModelStatus, Request, SearchRequest, WireMode, WirePhase,
};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{RepoSearch, RepoSearchError, worktree_root};
use crate::db::ChatDb;

/// Default answer budget, matching the repo-search CLI.
const DEFAULT_BUDGET: u32 = 4000;

/// How a query is matched.
#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CodeSearchMode {
    /// Lexical and semantic matches, fused.
    #[default]
    Hybrid,
    /// Identifier and keyword matches only.
    Lexical,
    /// Meaning matches only.
    Semantic,
}

impl From<CodeSearchMode> for WireMode {
    fn from(mode: CodeSearchMode) -> Self {
        match mode {
            CodeSearchMode::Hybrid => Self::Hybrid,
            CodeSearchMode::Lexical => Self::Lexical,
            CodeSearchMode::Semantic => Self::Semantic,
        }
    }
}

/// Search the repository for code you can describe but cannot name.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CodeSearchArgs {
    /// What to find, described in words or by identifiers. `status` reports
    /// the index; `index` reconciles it now.
    pub query: String,
    /// Directory inside the git worktree to search; defaults to the
    /// workspace directory.
    #[serde(default)]
    pub repo: Option<PathBuf>,
    /// Approximate token budget of the answer.
    #[serde(default)]
    pub budget: Option<u32>,
    /// List matching files only.
    #[serde(default)]
    pub files: bool,
    /// Matching: `hybrid` (default), `lexical` or `semantic`.
    #[serde(default)]
    pub mode: CodeSearchMode,
    /// Glob restricting the searched paths; repeatable.
    #[serde(default)]
    pub path: Vec<String>,
    /// Languages to search, e.g. `rust`; repeatable.
    #[serde(default)]
    pub lang: Vec<String>,
    /// Symbol kinds to return, e.g. `fn`, `struct`; repeatable.
    #[serde(default)]
    pub kind: Vec<String>,
    /// Symbol path pattern, e.g. `Env::*`.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Also search files git ignores.
    #[serde(default)]
    pub include_ignored: bool,
    /// With `index`: return once every chunk has a vector.
    #[serde(default)]
    pub wait: bool,
}

#[derive(Template)]
#[template(path = "code_search_unavailable.txt")]
struct Unavailable<'a> {
    reason: &'a str,
}

#[derive(Template)]
#[template(path = "code_search_not_worktree.txt")]
struct NotWorktree<'a> {
    dir: &'a str,
}

#[derive(Template)]
#[template(path = "code_search_status.txt")]
struct StatusView<'a> {
    worktree: &'a str,
    index: Option<&'a IndexStatus>,
    first_index: bool,
    model: &'a str,
}

#[derive(Template)]
#[template(path = "code_search_indexed.txt")]
struct IndexedView {
    with_vector: u64,
    total: u64,
}

/// `search` tool: finds code in the session's git worktree.
#[derive(Debug, Clone)]
pub struct CodeSearchTool {
    repo_search: RepoSearch,
    cwd: PathBuf,
    db: Option<ChatDb>,
}

impl CodeSearchTool {
    /// Searches worktrees through `repo_search`, defaulting to the one
    /// containing `cwd`. Every searched worktree is recorded in `db`'s repo
    /// usage, which keeps it on the daemon's auto-index list.
    #[must_use]
    pub const fn new(repo_search: RepoSearch, cwd: PathBuf, db: Option<ChatDb>) -> Self {
        Self {
            repo_search,
            cwd,
            db,
        }
    }

    async fn run(&self, args: CodeSearchArgs) -> Result<String, RepoSearchError> {
        let dir = args.repo.clone().unwrap_or_else(|| self.cwd.clone());
        let Some(worktree) = worktree_root(&dir) else {
            return Ok(NotWorktree {
                dir: &dir.display().to_string(),
            }
            .render()
            .map_err(render_error)?);
        };
        if let Some(db) = &self.db
            && let Err(error) = db.touch_repo_usage(&worktree).await
        {
            tracing::warn!(%error, "failed to record repo usage");
        }
        let request = request_for(&args, worktree);
        let client = self.repo_search.client().await?;
        match client.request(request).await?.finish().await? {
            Final::Answer(answer) => Ok(answer.output),
            Final::Status(report) => {
                let model = match &report.model {
                    ModelStatus::Ready { .. } => "ready".to_string(),
                    ModelStatus::Loading => "loading".to_string(),
                    ModelStatus::Downloading { .. } => "downloading".to_string(),
                    ModelStatus::Failed { message } => message.clone(),
                };
                Ok(StatusView {
                    worktree: &report.worktree.display().to_string(),
                    index: report.index.as_ref(),
                    first_index: report
                        .index
                        .as_ref()
                        .is_some_and(|index| index.phase == WirePhase::FirstIndex),
                    model: &model,
                }
                .render()
                .map_err(render_error)?)
            }
            Final::Indexed { with_vector, total } => Ok(IndexedView { with_vector, total }
                .render()
                .map_err(render_error)?),
            Final::Collection(_) | Final::Error(_) => Err(RepoSearchError::Client(
                ClientError::Protocol("unexpected answer"),
            )),
        }
    }
}

fn render_error(error: askama::Error) -> RepoSearchError {
    RepoSearchError::Io(std::io::Error::other(error))
}

/// The daemon request for one `search` call.
fn request_for(args: &CodeSearchArgs, worktree: PathBuf) -> Request {
    match args.query.trim() {
        "status" => Request::Status { worktree },
        "index" => Request::Index {
            worktree,
            wait: args.wait,
        },
        query => Request::Search(SearchRequest {
            worktree,
            query: query.to_string(),
            mode: args.mode.into(),
            paths: args.path.clone(),
            langs: args.lang.clone(),
            kinds: args.kind.clone(),
            symbol: args.symbol.clone(),
            budget: args.budget.unwrap_or(DEFAULT_BUDGET),
            files_only: args.files,
            include_ignored: args.include_ignored,
            format: Format::Text,
        }),
    }
}

impl Tool for CodeSearchTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("search")
    }

    type Arguments = CodeSearchArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        arguments: Self::Arguments,
        _cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        match self.run(arguments).await {
            Ok(text) => Ok(ToolResult::text(text)),
            Err(error) => Ok(ToolResult::text(
                Unavailable {
                    reason: &error.to_string(),
                }
                .render()?,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use repo_search_client::protocol::{Request, WireMode};

    use super::{CodeSearchArgs, request_for};

    fn args(cli: serde_json::Value) -> CodeSearchArgs {
        serde_json::from_value(cli).expect("valid arguments")
    }

    #[test]
    fn a_query_becomes_a_text_search_request() {
        let request = request_for(
            &args(serde_json::json!({
                "query": "where tokens are refreshed",
                "mode": "semantic",
                "path": ["src/**"],
                "files": true
            })),
            PathBuf::from("/repo"),
        );
        let Request::Search(search) = request else {
            panic!("expected a search request");
        };
        assert_eq!(search.query, "where tokens are refreshed");
        assert_eq!(search.mode, WireMode::Semantic);
        assert_eq!(search.paths, ["src/**"]);
        assert!(search.files_only);
        assert_eq!(search.budget, 4000);
    }

    #[test]
    fn status_and_index_are_actions() {
        assert!(matches!(
            request_for(
                &args(serde_json::json!({"query": "status"})),
                PathBuf::from("/r")
            ),
            Request::Status { .. }
        ));
        assert!(matches!(
            request_for(
                &args(serde_json::json!({"query": "index", "wait": true})),
                PathBuf::from("/r")
            ),
            Request::Index { wait: true, .. }
        ));
    }
}
