//! `pr` terminal command: open, merge, list, show, close the session's PRs.

use std::borrow::Cow;

use aither_core::llm::{Tool, ToolResult};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::pr::{PrContext, PrError, PrStore};

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
        /// PR title.
        #[serde(default)]
        title: Option<String>,
        /// Open only; merge later with `pr merge`.
        #[serde(default)]
        hold: bool,
        /// Associate a may issue number.
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

/// The main agent's `pr` command — full read/write on own-session PRs.
pub struct PrTool {
    store: PrStore,
    languages: Option<aither_agent::LanguageGuard>,
}

impl std::fmt::Debug for PrTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrTool").finish_non_exhaustive()
    }
}

impl PrTool {
    /// Build the command bound to one session's PR store.
    #[must_use]
    pub fn new(context: PrContext) -> Self {
        Self {
            store: PrStore::new(context),
            languages: None,
        }
    }

    /// Refuse a PR title outside the working language `guard` enforces.
    #[must_use]
    pub fn with_languages(mut self, guard: aither_agent::LanguageGuard) -> Self {
        self.languages = Some(guard);
        self
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
                    .open(
                        repo.as_deref(),
                        title.as_deref().unwrap_or("session work"),
                        issue,
                        hold,
                    )
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

/// Read-only `pr` for subagents: `list` and `show` only — a subagent never
/// opens, merges or closes a PR.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SubagentPrArgs {
    /// List PRs, newest first.
    List {
        /// `open`, `merged`, `closed` or `all`.
        #[serde(default)]
        state: Option<String>,
        /// Repo key or clone dir.
        #[serde(default)]
        repo: Option<String>,
    },
    /// Show one PR with its event trail.
    Show {
        /// PR number.
        number: i64,
        /// Repo key or clone dir.
        #[serde(default)]
        repo: Option<String>,
    },
}

/// The subagent's `pr` command — reads only.
pub struct SubagentPrTool {
    store: PrStore,
}

impl std::fmt::Debug for SubagentPrTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentPrTool").finish_non_exhaustive()
    }
}

impl SubagentPrTool {
    /// Build the read-only command bound to the same session's PR store.
    #[must_use]
    pub fn new(context: PrContext) -> Self {
        Self {
            store: PrStore::new(context),
        }
    }
}

impl Tool for SubagentPrTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("pr")
    }

    type Arguments = SubagentPrArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        let result = match args {
            SubagentPrArgs::List { state, repo } => {
                self.store.list(repo.as_deref(), state.as_deref()).await
            }
            SubagentPrArgs::Show { number, repo } => self.store.show(number, repo.as_deref()).await,
        };
        Ok(result.map_or_else(
            |e: PrError| ToolResult::text(e.to_string()),
            ToolResult::text,
        ))
    }
}
