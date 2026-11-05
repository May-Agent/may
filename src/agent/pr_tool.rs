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
}

impl Tool for PrTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("pr")
    }

    type Arguments = PrArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        if self.role == CommandRole::Subagent
            && let Some(action) = args.write_action()
        {
            return Ok(ToolResult::text(PrError::MainAgentOnly(action).to_string()));
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
}
