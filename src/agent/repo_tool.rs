//! `repo` terminal command: repository trust — show it, set it.

use std::borrow::Cow;
use std::path::PathBuf;

use aither_core::llm::{Tool, ToolResult};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::trust::{self, TrustLevel};

/// Everything `repo` needs: the session cwd, sandbox and workspace config.
#[derive(Debug, Clone)]
pub struct RepoContext {
    /// Session cwd — repo resolution falls back to it.
    pub cwd: PathBuf,
    /// Session sandbox — the workspace.toml lives under it.
    pub session_root: PathBuf,
    /// Workspace config — trust reads and writes `[[projects]]`.
    pub workspace_config: crate::workspace::WorkspaceConfig,
}

/// Repository trust: `repo trust` shows the launch repo's level,
/// `repo trust set <full|prs-only|local-branch|local>` persists the decision.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RepoArgs {
    /// Show or set the launch repository's trust level.
    Trust {
        /// Set and persist: `full`, `prs-only`, `local-branch` or `local`.
        #[serde(default)]
        set: Option<String>,
    },
}

/// The `repo` command — main agent may set trust.
pub struct RepoTool {
    context: RepoContext,
}

impl std::fmt::Debug for RepoTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoTool").finish_non_exhaustive()
    }
}

impl RepoTool {
    /// Build the command for one session.
    #[must_use]
    pub const fn new(context: RepoContext) -> Self {
        Self { context }
    }
}

fn worktree_and_key(context: &RepoContext) -> Result<(PathBuf, PathBuf), String> {
    let worktree = crate::repo_search::worktree_root(&context.cwd)
        .ok_or_else(|| "not inside a repository".to_string())?;
    let key = crate::issue::repository_identity(&worktree)
        .map_err(|e| format!("repository identity: {e}"))?;
    Ok((worktree, key))
}

fn config_path(context: &RepoContext) -> PathBuf {
    crate::session::SessionPaths::new(context.session_root.clone()).workspace_config()
}

impl Tool for RepoTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("repo")
    }

    type Arguments = RepoArgs;
    type Res = ToolResult;

    #[allow(clippy::unused_async_trait_impl)]
    async fn call(
        &self,
        args: Self::Arguments,
        _cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        Ok(ToolResult::text(match args {
            RepoArgs::Trust { set } => {
                let (worktree, key) = match worktree_and_key(&self.context) {
                    Ok(v) => v,
                    Err(e) => return Ok(ToolResult::text(e)),
                };
                set.map_or_else(
                    || {
                        trust::trust_for(&self.context.workspace_config, &key, &worktree)
                            .map_or_else(
                                || "trust undecided — the launch gate asks on startup; `repo trust set` decides it here".to_string(),
                                |level| format!("trust: {}", level.as_str()),
                            )
                    },
                    |value| {
                        TrustLevel::parse(&value).map_or_else(
                            || "level must be full, prs-only, local-branch or local".to_string(),
                            |level| {
                                let kind = trust::repo_kind(&worktree);
                                match trust::persist(
                                    &config_path(&self.context),
                                    &worktree,
                                    Some(&kind),
                                    level,
                                ) {
                                    Ok(()) => format!("trust set: {}", level.as_str()),
                                    Err(e) => format!("failed to persist: {e}"),
                                }
                            },
                        )
                    },
                )
            }
        }))
    }
}

/// The subagent's `repo` command — trust is read-only for subagents.
pub struct SubagentRepoTool {
    context: RepoContext,
}

impl std::fmt::Debug for SubagentRepoTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentRepoTool").finish_non_exhaustive()
    }
}

impl SubagentRepoTool {
    /// Build the read-only command for one session.
    #[must_use]
    pub const fn new(context: RepoContext) -> Self {
        Self { context }
    }
}

impl Tool for SubagentRepoTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("repo")
    }

    type Arguments = RepoArgs;
    type Res = ToolResult;

    #[allow(clippy::unused_async_trait_impl)]
    async fn call(
        &self,
        args: Self::Arguments,
        _cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        Ok(ToolResult::text(match args {
            RepoArgs::Trust { set: Some(_) } => "trust decisions are the main agent's".to_string(),
            RepoArgs::Trust { set: None } => {
                let (worktree, key) = match worktree_and_key(&self.context) {
                    Ok(v) => v,
                    Err(e) => return Ok(ToolResult::text(e)),
                };
                trust::trust_for(&self.context.workspace_config, &key, &worktree).map_or_else(
                    || "trust undecided — the launch gate asks on startup".to_string(),
                    |level| format!("trust: {}", level.as_str()),
                )
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::WorkspaceConfig;
    use aither_core::llm::ToolContext;

    #[tokio::test]
    async fn trust_set_keeps_a_field_changed_after_the_agent_was_built() {
        let repo = tempfile::tempdir().expect("tempdir");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .current_dir(repo.path())
                .status()
                .expect("git init")
                .success()
        );
        let session = tempfile::tempdir().expect("tempdir");
        let context = RepoContext {
            cwd: repo.path().to_path_buf(),
            session_root: session.path().to_path_buf(),
            workspace_config: WorkspaceConfig::default(),
        };
        let config_path = config_path(&context);
        let tool = RepoTool::new(context);
        let set = |level: &str| RepoArgs::Trust {
            set: Some(level.to_string()),
        };

        tool.call(set("local-branch"), ToolContext::new())
            .await
            .expect("first set");
        let mut newer = WorkspaceConfig::load(&config_path).expect("load");
        newer.projects[0].checks = Some("cargo test".to_string());
        newer.save(&config_path).expect("save");

        let result = tool
            .call(set("prs-only"), ToolContext::new())
            .await
            .expect("second set");
        assert_eq!(result.as_text(), Some("trust set: prs-only"));
        let loaded = WorkspaceConfig::load(&config_path).expect("load");
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].checks.as_deref(), Some("cargo test"));
    }
}
