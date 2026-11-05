//! `search`: code search over a git worktree through the repo-search daemon.

use std::borrow::Cow;
use std::future::Future;
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
use crate::repo_clone::SessionRepos;

/// Default answer budget, matching the repo-search CLI.
const DEFAULT_BUDGET: u32 = 4000;

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

/// The daemon side of `search`: answers one request. Implemented by the
/// repo-search daemon and, in tests, by an in-memory double.
pub trait SearchBackend: Clone + Send + Sync + 'static {
    /// Runs `request` to its final frame.
    fn request(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Final, RepoSearchError>> + Send;
}

impl SearchBackend for RepoSearch {
    async fn request(&self, request: Request) -> Result<Final, RepoSearchError> {
        Ok(self
            .client()
            .await?
            .request(request)
            .await?
            .finish()
            .await?)
    }
}

/// `search` tool: finds code in the session's git worktree, or in the
/// session's clone of it.
#[derive(Debug, Clone)]
pub struct CodeSearchTool<B = RepoSearch> {
    backend: B,
    cwd: PathBuf,
    db: Option<ChatDb>,
    clones: Option<SessionRepos>,
}

impl<B: SearchBackend> CodeSearchTool<B> {
    /// Searches worktrees through `backend`, defaulting to the one
    /// containing `cwd`. Every searched worktree is recorded in `db`'s repo
    /// usage, which keeps it on the daemon's auto-index list. A worktree the
    /// session holds a clone of in `clones` is searched through that clone,
    /// where the agent's edits are.
    #[must_use]
    pub const fn new(
        backend: B,
        cwd: PathBuf,
        db: Option<ChatDb>,
        clones: Option<SessionRepos>,
    ) -> Self {
        Self {
            backend,
            cwd,
            db,
            clones,
        }
    }

    /// The directory repo-search is asked about: the session's clone of
    /// `worktree`, else `worktree` itself.
    async fn search_root(&self, worktree: PathBuf) -> Result<PathBuf, RepoSearchError> {
        let Some(clones) = &self.clones else {
            return Ok(worktree);
        };
        let Some(clone) = clones
            .clone_of_checkout(&worktree)
            .await
            .map_err(|error| RepoSearchError::Io(std::io::Error::other(error)))?
        else {
            return Ok(worktree);
        };
        worktree_root(&clone).ok_or_else(|| {
            RepoSearchError::Io(std::io::Error::other(format!(
                "session clone {} is not a git worktree",
                clone.display()
            )))
        })
    }

    async fn run(&self, args: CodeSearchArgs) -> Result<String, RepoSearchError> {
        let dir = args.repo.clone().unwrap_or_else(|| self.cwd.clone());
        let Some(worktree) = worktree_root(&dir) else {
            return NotWorktree {
                dir: &crate::fs_utils::display_path(&dir),
            }
            .render()
            .map_err(render_error);
        };
        if let Some(db) = &self.db
            && let Err(error) = db.touch_repo_usage(&worktree).await
        {
            tracing::warn!(%error, "failed to record repo usage");
        }
        let root = self.search_root(worktree).await?;
        let request = request_for(&args, root);
        match self.backend.request(request).await? {
            Final::Answer(answer) => Ok(answer.output),
            Final::Status(report) => {
                let model = match &report.model {
                    ModelStatus::Ready { .. } => "ready".to_string(),
                    ModelStatus::Loading => "loading".to_string(),
                    ModelStatus::Downloading { .. } => "downloading".to_string(),
                    ModelStatus::Failed { message } => message.clone(),
                };
                Ok(StatusView {
                    worktree: &crate::fs_utils::display_path(&report.worktree),
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
            mode: WireMode::Hybrid,
            paths: args.path.clone(),
            langs: args.lang.clone(),
            kinds: args.kind.clone(),
            symbol: args.symbol.clone(),
            budget: args.budget.unwrap_or(DEFAULT_BUDGET),
            files_only: args.files,
            format: Format::Text,
        }),
    }
}

impl<B: SearchBackend> Tool for CodeSearchTool<B> {
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

    use askama::Template;
    use repo_search_client::protocol::{Request, WireMode};

    use super::{CodeSearchArgs, Unavailable, request_for};

    fn args(cli: serde_json::Value) -> CodeSearchArgs {
        serde_json::from_value(cli).expect("valid arguments")
    }

    #[test]
    fn a_query_becomes_a_text_search_request() {
        let request = request_for(
            &args(serde_json::json!({
                "query": "where tokens are refreshed",
                "path": ["src/**"],
                "files": true
            })),
            PathBuf::from("/repo"),
        );
        let Request::Search(search) = request else {
            panic!("expected a search request");
        };
        assert_eq!(search.query, "where tokens are refreshed");
        assert_eq!(search.mode, WireMode::Hybrid);
        assert_eq!(search.paths, ["src/**"]);
        assert!(search.files_only);
        assert_eq!(search.budget, 4000);
    }

    #[test]
    fn unavailable_names_an_internal_error() {
        let text = Unavailable {
            reason: "connection refused",
        }
        .render()
        .expect("the template renders");
        assert_eq!(
            text,
            "Code search is unavailable: connection refused\nThis is an internal May error. Report a bug."
        );
    }

    use std::fmt::Write as _;
    use std::path::Path;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use repo_search_client::protocol::{Answer, Final, QueryStats};

    use super::{CodeSearchTool, RepoSearchError, SearchBackend};
    use crate::db::ChatDb;
    use crate::repo_clone::SessionRepos;

    /// A daemon double: records each request's root, answers with the
    /// matching lines of `lib.rs` under that root, and adds the daemon's
    /// in-progress freshness line while `reconciling`.
    #[derive(Debug, Clone, Default)]
    struct Daemon {
        roots: Arc<Mutex<Vec<PathBuf>>>,
        reconciling: bool,
    }

    #[allow(clippy::unused_async_trait_impl)]
    impl SearchBackend for Daemon {
        async fn request(&self, request: Request) -> Result<Final, RepoSearchError> {
            let Request::Search(search) = request else {
                panic!("expected a search request");
            };
            self.roots.lock().unwrap().push(search.worktree.clone());
            let source = std::fs::read_to_string(search.worktree.join("lib.rs")).unwrap();
            let mut output = String::new();
            for line in source.lines().filter(|line| line.contains(&search.query)) {
                writeln!(
                    output,
                    "{}: {line}",
                    search.worktree.join("lib.rs").display()
                )
                .unwrap();
            }
            output.push_str(if self.reconciling {
                "-- fresh @ abc123 +0 changed files reconciled; vectors 100%; indexing in progress — files changed since they were indexed are left out\n"
            } else {
                "-- fresh @ abc123 +0 changed files reconciled; vectors 100%\n"
            });
            Ok(Final::Answer(Answer {
                output,
                has_results: true,
                stats: QueryStats {
                    barrier_us: 0,
                    indexing_wait_us: 0,
                    indexing_overtaken: 0,
                    embedded_on_the_spot: 0,
                    total_us: 0,
                },
            }))
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A checkout with one committed file, and a tool whose session holds
    /// a clone of it (or none).
    async fn fixture(
        tmp: &Path,
        with_clone: bool,
        daemon: Daemon,
    ) -> (CodeSearchTool<Daemon>, PathBuf, Option<PathBuf>) {
        let checkout = tmp.join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        git(&checkout, &["init"]);
        std::fs::write(checkout.join("lib.rs"), "fn original() {}\n").unwrap();
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-m", "init"]);
        let checkout = crate::repo_search::worktree_root(&checkout).unwrap();
        let db = ChatDb::open(&tmp.join("history.db")).await.unwrap();
        let repos = SessionRepos::new(&tmp.join("session"), "s1", db.clone());
        let clone = if with_clone {
            let key = crate::issue::repository_identity(&checkout).unwrap();
            Some(repos.ensure_local_clone(&checkout, &key).await.unwrap())
        } else {
            None
        };
        let tool = CodeSearchTool::new(daemon, checkout.clone(), Some(db), Some(repos));
        (tool, checkout, clone)
    }

    #[tokio::test]
    async fn a_session_with_a_clone_searches_the_clone_and_sees_its_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = Daemon::default();
        let (tool, _checkout, clone) = fixture(tmp.path(), true, daemon.clone()).await;
        let clone = crate::repo_search::worktree_root(&clone.unwrap()).unwrap();
        std::fs::write(
            clone.join("lib.rs"),
            "fn original() {}\nfn edited_in_clone() {}\n",
        )
        .unwrap();

        let text = tool
            .run(args(serde_json::json!({"query": "edited_in_clone"})))
            .await
            .unwrap();

        assert_eq!(*daemon.roots.lock().unwrap(), std::slice::from_ref(&clone));
        assert!(text.contains(&format!(
            "{}: fn edited_in_clone",
            clone.join("lib.rs").display()
        )));
        assert!(!text.contains("indexing in progress"));
    }

    #[tokio::test]
    async fn a_search_while_the_clone_is_indexing_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = Daemon {
            reconciling: true,
            ..Daemon::default()
        };
        let (tool, _checkout, _clone) = fixture(tmp.path(), true, daemon).await;

        let text = tool
            .run(args(serde_json::json!({"query": "original"})))
            .await
            .unwrap();

        assert!(text.contains("fn original"));
        assert!(text.ends_with(
            "-- fresh @ abc123 +0 changed files reconciled; vectors 100%; indexing in progress — files changed since they were indexed are left out\n"
        ));
    }

    #[tokio::test]
    async fn a_search_while_the_clone_is_indexing_passes_the_daemon_line_through() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = Daemon {
            reconciling: true,
            ..Daemon::default()
        };
        let (tool, _checkout, _clone) = fixture(tmp.path(), true, daemon).await;
        let text = tool
            .run(args(serde_json::json!({"query": "original"})))
            .await
            .unwrap();
        assert_eq!(text.matches("indexing in progress").count(), 1);
    }

    #[tokio::test]
    async fn a_session_without_a_clone_searches_the_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = Daemon::default();
        let (tool, checkout, _clone) = fixture(tmp.path(), false, daemon.clone()).await;

        tool.run(args(serde_json::json!({"query": "original"})))
            .await
            .unwrap();

        assert_eq!(*daemon.roots.lock().unwrap(), [checkout]);
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
