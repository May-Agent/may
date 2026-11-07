//! `pr` terminal command: open, merge, list, show, close the session's PRs.

use std::borrow::Cow;

use aither_core::llm::{Tool, ToolResult};
use aither_sandbox::{CommandRole, TerminalBinding};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::pr::{PrContext, PrError, PrStore};
use crate::repo_clone::CloneSandbox;

/// Land session work as a numbered PR.
///
/// Subcommands:
/// - `pr open [--hold] [--issue n] [--title t] [--repo key]` — open the PR;
///   without `--hold` it merges as soon as checks pass
/// - `pr merge <n> [--repo key]` — check and land the PR's pinned commit;
///   also the retry after a rejection
/// - `pr update <n> [--repo key]` — pin the clone's current HEAD as the
///   PR's commit (after a rebase or new commits)
/// - `pr list [--state open|merged|closed|all] [--repo key]`
/// - `pr show <n> [--repo key]`
/// - `pr close <n> [--repo key]`
/// - `pr adopt <n> [--repo key]` — take over a PR whose session is gone
///   (or still another's) so this session may merge or close it
///
/// Run inside a session clone, `pr` acts on that clone's repository;
/// elsewhere `--repo` names it, else the session's only clone is meant.
/// Merging deletes the clone, ignored files included: `merge` refuses while
/// it holds uncommitted changes or untracked files that are not ignored,
/// and lists them. Keep what must survive committed, or under `artifacts/`,
/// `outputs/`, `data/` or `tmp/`.
///
/// Writes are bound to this session's own PRs; reads are open.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PrArgs {
    /// Open a PR for this repo's session clone.
    Open {
        /// PR title; defaults to the subject of the first commit.
        #[serde(default)]
        title: Option<String>,
        /// Open only; merge later with `pr merge`.
        #[serde(default)]
        hold: bool,
        /// The project's issue number: a GitHub project's PR body closes it.
        #[serde(default)]
        issue: Option<i64>,
        /// Repo key or clone dir when several clones exist.
        #[serde(default)]
        repo: Option<String>,
    },
    /// Re-pin own PR `n` to the clone's current HEAD.
    Update {
        /// PR number.
        number: i64,
        /// Repo key or clone dir when several clones exist.
        #[serde(default)]
        repo: Option<String>,
    },
    /// Merge PR `n` (checks → serialized land → cleanup); also retries.
    Merge {
        /// PR number.
        number: i64,
        /// Repo key or clone dir when several clones exist.
        #[serde(default)]
        repo: Option<String>,
    },
    /// List PRs, newest first.
    List {
        /// `open`, `merged`, `closed` or `all`.
        #[serde(default)]
        state: Option<String>,
        /// Repo key or clone dir; default the session's only clone or all.
        #[serde(default)]
        repo: Option<String>,
    },
    /// Show one PR with its event trail.
    Show {
        /// PR number.
        number: i64,
        /// Repo key or clone dir when several clones exist.
        #[serde(default)]
        repo: Option<String>,
    },
    /// Take over a PR owned by a deleted (or other) session.
    Adopt {
        /// PR number.
        number: i64,
        /// `local:<path>` or `github:owner/repo`.
        repo: Option<String>,
    },
    /// Close an own, unmerged PR.
    Close {
        /// PR number.
        number: i64,
        /// Repo key or clone dir when several clones exist.
        #[serde(default)]
        repo: Option<String>,
    },
}

/// Builds the `pr` command for each terminal that serves it.
#[derive(Debug, Clone)]
pub struct PrCommand {
    context: PrContext,
    languages: Option<aither_agent::LanguageGuard>,
}

impl PrCommand {
    /// The command for one session's PRs.
    #[must_use]
    pub const fn new(context: PrContext) -> Self {
        Self {
            context,
            languages: None,
        }
    }

    /// Refuse a PR title outside the working language `guard` enforces.
    #[must_use]
    pub fn with_languages(mut self, guard: aither_agent::LanguageGuard) -> Self {
        self.languages = Some(guard);
        self
    }

    /// The command as the terminal `binding` serves it: what works inside a
    /// session clone runs in that terminal's sandbox, and a subagent's
    /// terminal answers the reading actions alone.
    #[must_use]
    pub fn for_terminal(&self, binding: &TerminalBinding) -> PrTool {
        PrTool {
            store: PrStore::new(
                self.context.clone(),
                CloneSandbox::new(binding.runner().clone()),
            ),
            languages: self.languages.clone(),
            role: binding.role(),
        }
    }
}

/// The `pr` command one terminal serves: full read/write on own-session PRs
/// for the main agent, `list` and `show` for a subagent.
pub struct PrTool {
    store: PrStore,
    languages: Option<aither_agent::LanguageGuard>,
    role: CommandRole,
}

impl std::fmt::Debug for PrTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrTool").finish_non_exhaustive()
    }
}

impl PrArgs {
    /// The action's name, for a write a subagent is refused.
    const fn write_action(&self) -> Option<&'static str> {
        match self {
            Self::Open { .. } => Some("open"),
            Self::Update { .. } => Some("update"),
            Self::Merge { .. } => Some("merge"),
            Self::Close { .. } => Some("close"),
            Self::Adopt { .. } => Some("adopt"),
            Self::List { .. } | Self::Show { .. } => None,
        }
    }

    /// The repository the call names.
    const fn repo_mut(&mut self) -> &mut Option<String> {
        match self {
            Self::Open { repo, .. }
            | Self::Update { repo, .. }
            | Self::Merge { repo, .. }
            | Self::List { repo, .. }
            | Self::Show { repo, .. }
            | Self::Adopt { repo, .. }
            | Self::Close { repo, .. } => repo,
        }
    }
}

impl Tool for PrTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("pr")
    }

    type Arguments = PrArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        mut args: Self::Arguments,
        cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        if self.role == CommandRole::Subagent
            && let Some(action) = args.write_action()
        {
            return Ok(ToolResult::text(PrError::MainAgentOnly(action).to_string()));
        }
        // A call made inside a session clone means that clone's repository.
        let repo = args.repo_mut();
        if repo.is_none()
            && let Some(dir) = cx.working_dir()
        {
            match self.store.clone_containing(std::path::Path::new(dir)).await {
                Ok(found) => *repo = found,
                Err(error) => return Ok(ToolResult::text(error.to_string())),
            }
        }
        if let (
            Some(guard),
            PrArgs::Open {
                title: Some(title), ..
            },
        ) = (&self.languages, &args)
        {
            guard.check_working(title, aither_agent::WorkingText::Public)?;
        }
        let result = match args {
            PrArgs::Open {
                title,
                hold,
                issue,
                repo,
            } => {
                self.store
                    .open(repo.as_deref(), title.as_deref(), issue, hold)
                    .await
            }
            PrArgs::Update { number, repo } => self.store.update(number, repo.as_deref()).await,
            PrArgs::Merge { number, repo } => self.store.merge(number, repo.as_deref()).await,
            PrArgs::List { state, repo } => {
                self.store.list(repo.as_deref(), state.as_deref()).await
            }
            PrArgs::Show { number, repo } => self.store.show(number, repo.as_deref()).await,
            PrArgs::Close { number, repo } => self.store.close(number, repo.as_deref()).await,
            PrArgs::Adopt { number, repo } => self.store.adopt(number, repo.as_deref()).await,
        };
        Ok(result.map_or_else(|e| ToolResult::text(e.to_string()), ToolResult::text))
    }
}

#[cfg(test)]
mod tests {
    use aither_core::llm::ToolContext;

    use super::{CommandRole, PrArgs, PrStore, PrTool, Tool};
    use crate::pr::PrContext;
    use crate::repo_clone::test_sandbox::TestTerminal;

    fn text(result: &aither_core::llm::ToolResult) -> String {
        format!("{result:?}")
    }

    /// A subagent's `pr` reads PRs and is refused every write.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_subagent_reads_prs_and_writes_none() {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap().join("session");
        let db = crate::db::ChatDb::open(&tmp.path().join("history.db"))
            .await
            .unwrap();
        let terminal = TestTerminal::new(&root).await;
        let tool = PrTool {
            store: PrStore::new(
                PrContext {
                    db: Some(db),
                    session_id: "s1".to_string(),
                    session_root: root.clone(),
                    scratch_root: tmp.path().join("scratch"),
                    cwd: root,
                    workspace_config: crate::workspace::WorkspaceConfig::default(),
                    screen: None,
                    signing: crate::commit_signing::CommitSigning::new(
                        crate::host_git::HostGit::inherit(),
                        tmp.path(),
                        "s1",
                    ),
                    github_api: PrContext::default_github_api(),
                    task_events: None,
                    ci: None,
                    publish: None,
                },
                terminal.sandbox(),
            ),
            languages: None,
            role: CommandRole::Subagent,
        };

        let listed = tool
            .call(
                PrArgs::List {
                    state: None,
                    repo: None,
                },
                ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(text(&listed).contains("no PRs"), "{listed:?}");
        for (args, action) in [
            (
                PrArgs::Open {
                    title: None,
                    hold: true,
                    issue: None,
                    repo: None,
                },
                "open",
            ),
            (
                PrArgs::Merge {
                    number: 1,
                    repo: None,
                },
                "merge",
            ),
            (
                PrArgs::Adopt {
                    number: 1,
                    repo: None,
                },
                "adopt",
            ),
        ] {
            let refused = text(&tool.call(args, ToolContext::new()).await.unwrap());
            assert!(
                refused.contains(&format!("`pr {action}` is the main agent's")),
                "{refused}"
            );
        }
    }

    /// One session clones two repositories with `repo clone`, commits in
    /// each, and lands a PR in each with `pr open` run inside its clone:
    /// each checkout changes only when its own PR lands, and each merge
    /// deletes only its own clone.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_lands_a_pr_in_each_of_two_repositories() {
        use crate::agent::repo_tool::{RepoArgs, RepoTool, tests::context};
        use crate::commit_signing::test_user::TestUser;
        use std::path::Path;

        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let at = |dir: &Path| ToolContext::new().with_working_dir(dir.to_string_lossy());
        let tmp = tempfile::tempdir().unwrap();
        let tmp = dunce::canonicalize(tmp.path()).unwrap();
        let user = TestUser::new(&tmp.join("user"));
        let checkouts = [tmp.join("aither"), tmp.join("may")];
        for checkout in &checkouts {
            std::fs::create_dir_all(checkout).unwrap();
            git(checkout, &["init", "-q", "-b", "main"]);
            user.identify(checkout);
            std::fs::write(checkout.join("README"), "base").unwrap();
            git(checkout, &["add", "."]);
            git(checkout, &["commit", "-q", "-m", "init"]);
        }
        let session_root = tmp.join("profile/sessions/s1");
        let repo_context = context(&checkouts[1], &session_root, &tmp).await;
        let db = repo_context.db.clone();
        let repo = RepoTool::new(repo_context);
        let terminal = TestTerminal::new(&session_root).await;
        let pr = PrTool {
            store: PrStore::new(
                PrContext {
                    db: Some(db.clone()),
                    session_id: "s1".to_string(),
                    session_root: session_root.clone(),
                    scratch_root: tmp.join("staging"),
                    cwd: checkouts[1].clone(),
                    workspace_config: crate::workspace::WorkspaceConfig::default(),
                    screen: None,
                    signing: user.signing(&tmp.join("may-home"), "s1"),
                    github_api: PrContext::default_github_api(),
                    task_events: None,
                    ci: None,
                    publish: None,
                },
                terminal.sandbox(),
            ),
            languages: None,
            role: CommandRole::Main,
        };

        let mut work = Vec::new();
        for (checkout, file) in checkouts.iter().zip(["aither.txt", "may.txt"]) {
            let trust = RepoArgs::Trust {
                set: Some("local-branch".to_string()),
            };
            repo.call(trust, at(checkout)).await.unwrap();
            let report = repo
                .call(RepoArgs::Clone { path: None }, at(checkout))
                .await
                .unwrap();
            let report = report.as_text().unwrap();
            let clone = Path::new(report.split(' ').next().unwrap()).to_path_buf();
            assert!(clone.starts_with(session_root.join("repos")), "{report}");
            user.identify(&clone);
            std::fs::write(clone.join(file), file).unwrap();
            git(&clone, &["add", "."]);
            git(&clone, &["commit", "-q", "-m", file]);
            work.push((clone, file));
        }
        assert_eq!(db.list_repo_clones("s1").await.unwrap().len(), 2);
        for (checkout, (_, file)) in checkouts.iter().zip(&work) {
            assert!(!checkout.join(file).exists(), "edits stay in the clone");
        }

        for (index, (clone, file)) in work.iter().enumerate() {
            let open = PrArgs::Open {
                title: None,
                hold: false,
                issue: None,
                repo: None,
            };
            let landed = text(&pr.call(open, at(clone)).await.unwrap());
            assert!(landed.contains("merged PR #1 as "), "{landed}");
            assert!(checkouts[index].join(file).exists(), "{landed}");
            assert!(!clone.exists(), "the merge deletes its own clone");
            for (other, (other_clone, other_file)) in checkouts.iter().zip(&work).skip(index + 1) {
                assert!(other_clone.exists(), "another repository's clone stays");
                assert!(!other.join(other_file).exists(), "nor does it land");
            }
        }
        assert!(db.list_repo_clones("s1").await.unwrap().is_empty());
    }
}
