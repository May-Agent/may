//! `repo` terminal command: repository trust — show it, set it — and the
//! session's clone of a repository.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use aither_core::llm::{Tool, ToolContext, ToolResult};
use askama::Template;
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
    /// History database, which holds the session's clone registry.
    pub db: crate::db::ChatDb,
    /// The session the clones belong to.
    pub session_id: String,
    /// Host scratch space outside every sandbox where new clones are made.
    pub scratch_root: PathBuf,
    /// The git that makes clones.
    pub host: crate::host_git::HostGit,
    /// The daemon configuration: its `[forge]` map resolves a self-hosted
    /// remote's kind and credentials.
    pub may_config: std::sync::Arc<crate::config::MayConfig>,
    /// Daemon-held default-branch checkouts used to derive GitHub workspaces.
    pub checkouts: Option<crate::repo_clone::DaemonCheckouts>,
    /// Reads forge repository access while default trust is derived.
    pub probe: crate::trust::ForgeProbes<crate::pr::PrTokenSource>,
    /// Whether the directory a command runs in is a host path. A container
    /// session's commands run under the container's paths, so its `repo`
    /// acts on the launch repository alone.
    pub host_paths: bool,
}

/// Repository trust and session clones.
///
/// Each acts on the repository the command runs in, or the launch
/// repository when it runs inside the session directory:
/// `repo trust` shows its level, `repo trust set <full|prs-only|local-branch|local>`
/// persists the decision, and `repo clone [--path <dir>]` gives the session
/// its own clone of it (or names the one it has), where the work is edited,
/// committed and landed with `pr`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RepoArgs {
    /// Show or set the repository's trust level.
    Trust {
        /// Set and persist: `full`, `prs-only`, `local-branch` or `local`.
        #[serde(default)]
        set: Option<String>,
    },
    /// Derive a fresh workspace copy-on-write for this repository, or name an
    /// existing clean workspace when `--new` is omitted.
    Clone {
        /// A directory inside the repository; default where the command runs.
        #[serde(default)]
        path: Option<String>,
        /// Derive a fresh independent workspace even when another workspace on
        /// the same repository already exists (for concurrent changes/PRs).
        #[serde(default)]
        new: bool,
    },
}

/// What `repo clone` reports.
#[derive(Template)]
#[template(path = "repo_clone.txt", escape = "none")]
struct RepoCloneReport<'a> {
    worktree: &'a str,
    outcome: CloneOutcome,
}

/// How `repo clone` ended for a repository.
enum CloneOutcome {
    /// The session's clone, new or existing, is at this path.
    Cloned(String),
    /// `local` trust: the session writes the checkout and holds no clone.
    WritesCheckout,
}

/// The directory a call acts on: `path` resolved against where the command
/// runs, else that directory, unless it lies inside the session directory
/// or is no host path, where the launch directory stands in.
fn target_dir(context: &RepoContext, cx: &ToolContext, path: Option<&str>) -> PathBuf {
    let caller = cx
        .working_dir()
        .filter(|_| context.host_paths)
        .map(PathBuf::from)
        .filter(|dir| !dir.starts_with(&context.session_root));
    let base = caller.unwrap_or_else(|| context.cwd.clone());
    match path {
        Some(path) => base.join(path),
        None => base,
    }
}

/// `repo clone` for the repository containing `dir`, falling back to the
/// session's repository binding when no checkout sits at `dir`.
async fn clone_repository(context: &RepoContext, dir: &Path, create_new: bool) -> String {
    let config_path = config_path(context);
    let mut repos = crate::repo_clone::SessionRepos::new(
        &context.session_root,
        context.session_id.clone(),
        context.db.clone(),
    )
    .with_host(context.host.clone());
    if let Some(checkouts) = &context.checkouts {
        repos = repos.with_checkouts(checkouts.clone());
    }
    let (worktree_display, outcome) = match worktree_and_key(dir).await {
        Ok((worktree, _key)) => {
            let display = worktree.to_string_lossy().into_owned();
            let level = match trust::resolve_checkout(&config_path, &worktree, &context.probe).await
            {
                Ok(level) => level,
                Err(e) => return format!("failed to resolve the repository trust: {e}"),
            };
            if !level.clones_work() {
                return RepoCloneReport {
                    worktree: &display,
                    outcome: CloneOutcome::WritesCheckout,
                }
                .render()
                .expect("the repo clone report renders");
            }
            // The resolution may have persisted a level: read the config
            // afresh so a GitHub level picks the remote clone.
            let config = match load_workspace_config(&config_path).await {
                Ok(config) => config,
                Err(e) => return format!("failed to read the workspace config: {e}"),
            };
            let outcome = match repos
                .request_clone_of(&worktree, &config, &context.scratch_root, create_new)
                .await
            {
                Ok(Some(clone)) => CloneOutcome::Cloned(clone.to_string_lossy().into_owned()),
                Ok(None) => CloneOutcome::WritesCheckout,
                Err(e) => {
                    return format!("failed to clone {}: {e}", worktree.display());
                }
            };
            (display, outcome)
        }
        Err(err) => {
            let config = match load_workspace_config(&config_path).await {
                Ok(config) => config,
                Err(e) => return format!("failed to read the workspace config: {e}"),
            };
            let binding = context
                .db
                .get_session(&context.session_id)
                .await
                .ok()
                .flatten()
                .and_then(|row| row.repo)
                .and_then(|stored| crate::session::RepoBinding::parse(&stored).ok());
            let Some(binding) = binding else {
                return err;
            };
            let display = binding.clone_key();
            let outcome = match repos
                .request_clone_of_binding(&binding, &config, &context.scratch_root, create_new)
                .await
            {
                Ok(Some(clone)) => CloneOutcome::Cloned(clone.to_string_lossy().into_owned()),
                Ok(None) => CloneOutcome::WritesCheckout,
                Err(e) => return format!("failed to clone {display}: {e}"),
            };
            (display, outcome)
        }
    };
    RepoCloneReport {
        worktree: &worktree_display,
        outcome,
    }
    .render()
    .expect("the repo clone report renders")
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

    /// Persist `level` for the repository of `worktree` and bind the session
    /// to it at once, replacing a binding the earlier decision gave. The
    /// decision also changes the workspace configuration, which the daemon's
    /// agent fingerprint covers, so the next turn rebuilds the agent whether
    /// or not the binding moved.
    async fn set_trust(&self, worktree: &Path, level: TrustLevel) -> String {
        let (config, worktree, may_config) = (
            config_path(&self.context),
            worktree.to_path_buf(),
            self.context.may_config.clone(),
        );
        let bound = blocking::unblock(move || {
            let kind = trust::repo_kind_with_config(&worktree, may_config.as_ref());
            trust::persist(&config, &worktree, Some(&kind), level)
                .map_err(|error| format!("failed to persist: {error}"))?;
            crate::session::RepoBinding::for_trust_with_config(
                &worktree,
                level,
                may_config.as_ref(),
            )
            .map_err(|error| format!("repository identity: {error}"))
        })
        .await;
        let binding = match bound {
            Ok(binding) => binding,
            Err(message) => return message,
        };
        match self
            .context
            .db
            .set_session_repo(&self.context.session_id, Some(&binding.to_stored()))
            .await
        {
            Ok(()) => format!("trust set: {}", level.as_str()),
            Err(error) => format!("trust set, but the session could not be bound: {error}"),
        }
    }
}

async fn worktree_and_key(dir: &Path) -> Result<(PathBuf, PathBuf), String> {
    let dir = dir.to_path_buf();
    blocking::unblock(move || {
        let worktree = crate::repo_search::worktree_root(&dir)
            .ok_or_else(|| format!("{} is not inside a repository", dir.display()))?;
        let key = crate::issue::repository_identity(&worktree)
            .map_err(|e| format!("repository identity: {e}"))?;
        Ok((worktree, key))
    })
    .await
}

/// The workspace config, read off the executor.
async fn load_workspace_config(
    config_path: &Path,
) -> Result<crate::workspace::WorkspaceConfig, crate::workspace::WorkspaceConfigError> {
    let config_path = config_path.to_path_buf();
    blocking::unblock(move || crate::workspace::WorkspaceConfig::load(&config_path)).await
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
    type Error = core::convert::Infallible;

    async fn call(&self, args: Self::Arguments, cx: ToolContext) -> Result<Self::Res, Self::Error> {
        Ok(ToolResult::text(match args {
            RepoArgs::Clone { path, new } => {
                clone_repository(
                    &self.context,
                    &target_dir(&self.context, &cx, path.as_deref()),
                    new,
                )
                .await
            }
            RepoArgs::Trust { set } => {
                let (worktree, _key) =
                    match worktree_and_key(&target_dir(&self.context, &cx, None)).await {
                        Ok(v) => v,
                        Err(e) => return Ok(ToolResult::text(e)),
                    };
                let Some(value) = set else {
                    let level = match trust::resolve_checkout(
                        &config_path(&self.context),
                        &worktree,
                        &self.context.probe,
                    )
                    .await
                    {
                        Ok(level) => level,
                        Err(e) => {
                            return Ok(ToolResult::text(format!(
                                "failed to resolve the repository trust: {e}"
                            )));
                        }
                    };
                    return Ok(ToolResult::text(format!("trust: {}", level.as_str())));
                };
                let Some(level) = TrustLevel::parse(&value) else {
                    return Ok(ToolResult::text(
                        "level must be full, prs-only, local-branch or local".to_string(),
                    ));
                };
                self.set_trust(&worktree, level).await
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
    type Error = core::convert::Infallible;

    async fn call(&self, args: Self::Arguments, cx: ToolContext) -> Result<Self::Res, Self::Error> {
        Ok(ToolResult::text(match args {
            RepoArgs::Trust { set: Some(_) } => "trust decisions are the main agent's".to_string(),
            RepoArgs::Clone { .. } => {
                "session clones are the main agent's; work where `subagent` granted you".to_string()
            }
            RepoArgs::Trust { set: None } => {
                let (_worktree, key) =
                    match worktree_and_key(&target_dir(&self.context, &cx, None)).await {
                        Ok(v) => v,
                        Err(e) => return Ok(ToolResult::text(e)),
                    };
                format!(
                    "trust: {}",
                    trust::effective_for(&self.context.workspace_config, &key).as_str()
                )
            }
        }))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::workspace::WorkspaceConfig;

    /// A `repo` context for session `s1` launched in `cwd`, whose workspace
    /// file lives in the profile two levels above `session_root`.
    pub async fn context(cwd: &Path, session_root: &Path, scratch: &Path) -> RepoContext {
        std::fs::create_dir_all(session_root).expect("session dir");
        let db = crate::db::ChatDb::open(&scratch.join("history.db"))
            .await
            .expect("db");
        RepoContext {
            cwd: cwd.to_path_buf(),
            session_root: session_root.to_path_buf(),
            workspace_config: WorkspaceConfig::default(),
            db,
            session_id: "s1".to_string(),
            scratch_root: scratch.join("staging"),
            host: crate::host_git::HostGit::inherit(),
            may_config: std::sync::Arc::new(crate::config::MayConfig::default()),
            checkouts: None,
            // The probe always fails like an offline `gh`: resolution
            // falls back without persisting.
            probe: crate::trust::ForgeProbes::new(
                crate::issue::github::GitHubRest::new(crate::pr::PrTokenSource::Unauthenticated),
                crate::forge::LiveForgeAuth::default(),
            ),
            host_paths: true,
        }
    }

    /// A repository at `dir` with one commit.
    fn init_repo(dir: &Path) {
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["commit", "-q", "--allow-empty", "-m", "init"],
        ] {
            std::fs::create_dir_all(dir).expect("repo dir");
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .status()
                    .expect("git")
                    .success()
            );
        }
    }

    fn at(dir: &Path) -> ToolContext {
        ToolContext::new().with_working_dir(dir.to_string_lossy())
    }

    /// `repo clone` run in a second repository resolves its trust and
    /// clones it, once; `repo trust` reports the level in force.
    #[tokio::test(flavor = "multi_thread")]
    async fn repo_clone_clones_the_repository_it_runs_in() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tmp = dunce::canonicalize(tmp.path()).expect("canonical");
        let (launch, other) = (tmp.join("launch"), tmp.join("other"));
        init_repo(&launch);
        init_repo(&other);
        let session_root = tmp.join("profile/sessions/s1");
        let tool = RepoTool::new(context(&launch, &session_root, &tmp).await);
        let clone = || RepoArgs::Clone {
            path: None,
            new: false,
        };

        // A non-GitHub repository defaults to `local-branch` and clones.
        let first = tool.call(clone(), at(&other)).await.expect("clone");
        let text = first.as_text().unwrap().to_string();
        assert!(
            text.contains("is this session's writable workspace"),
            "{text}"
        );
        let clones: Vec<_> = std::fs::read_dir(session_root.join("repos"))
            .expect("repos")
            .map(|e| e.expect("entry").path())
            .collect();
        assert_eq!(clones.len(), 1);
        assert!(text.starts_with(&clones[0].to_string_lossy().into_owned()));
        assert!(clones[0].starts_with(session_root.join("repos")));

        // Again, and from a subdirectory: the same clone.
        std::fs::create_dir_all(other.join("sub")).expect("sub");
        let again = tool
            .call(clone(), at(&other.join("sub")))
            .await
            .expect("again");
        assert_eq!(again.as_text().unwrap(), text);
        // `repo trust` reports the effective level: the launch repository,
        // undecided, reads its `local-branch` default.
        let launch_trust = tool
            .call(RepoArgs::Trust { set: None }, at(&launch))
            .await
            .expect("trust");
        assert_eq!(launch_trust.as_text(), Some("trust: local-branch"));
        // An explicit decision wins and persists.
        tool.call(
            RepoArgs::Trust {
                set: Some("local".to_string()),
            },
            at(&launch),
        )
        .await
        .expect("set");
        let launch_trust = tool
            .call(RepoArgs::Trust { set: None }, at(&launch))
            .await
            .expect("trust");
        assert_eq!(launch_trust.as_text(), Some("trust: local"));
    }

    /// A GitHub checkout whose access lookup fails — offline or an
    /// unauthenticated `gh`, which `context`'s probe is — falls back to
    /// `local-branch` without persisting: `repo clone` still makes a
    /// session clone and writes no `[[projects]]` entry.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_offline_github_checkout_clones_without_persisting() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tmp = dunce::canonicalize(tmp.path()).expect("canonical");
        let repo = tmp.join("repo");
        init_repo(&repo);
        assert!(
            std::process::Command::new("git")
                .args(["remote", "add", "origin", "https://github.com/acme/app.git"])
                .current_dir(&repo)
                .status()
                .expect("git remote")
                .success()
        );
        let session_root = tmp.join("profile/sessions/s1");
        let context = context(&repo, &session_root, &tmp).await;
        let config_path = config_path(&context);
        let tool = RepoTool::new(context);

        let result = tool
            .call(
                RepoArgs::Clone {
                    path: None,
                    new: false,
                },
                at(&repo),
            )
            .await
            .expect("clone");
        let text = result.as_text().unwrap().to_string();
        assert!(
            text.contains("is this session's writable workspace"),
            "{text}"
        );
        let clones: Vec<_> = std::fs::read_dir(session_root.join("repos"))
            .expect("repos")
            .map(|e| e.expect("entry").path())
            .collect();
        assert_eq!(clones.len(), 1);
        assert!(clones[0].starts_with(session_root.join("repos")));
        assert!(clones[0].join(".git").is_dir());

        // The transient lookup failure persists nothing.
        let loaded = WorkspaceConfig::load(&config_path).expect("load");
        assert!(loaded.projects.is_empty());
    }

    /// `local` trust works in the checkout itself: no clone.
    #[tokio::test(flavor = "multi_thread")]
    async fn local_trust_makes_no_clone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tmp = dunce::canonicalize(tmp.path()).expect("canonical");
        let repo = tmp.join("repo");
        init_repo(&repo);
        let session_root = tmp.join("profile/sessions/s1");
        let context = context(&repo, &session_root, &tmp).await;
        trust::persist(&config_path(&context), &repo, None, TrustLevel::Local).expect("persist");
        let tool = RepoTool::new(context);
        let result = tool
            .call(
                RepoArgs::Clone {
                    path: None,
                    new: false,
                },
                at(&repo),
            )
            .await
            .expect("call");
        assert!(
            result.as_text().unwrap().contains("`local` trust"),
            "{result:?}"
        );
        assert!(!session_root.join("repos").exists());
    }

    /// A subagent makes no clone.
    #[tokio::test]
    async fn a_subagent_makes_no_clone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tmp = dunce::canonicalize(tmp.path()).expect("canonical");
        let repo = tmp.join("repo");
        init_repo(&repo);
        let session_root = tmp.join("profile/sessions/s1");
        let tool = SubagentRepoTool::new(context(&repo, &session_root, &tmp).await);
        let result = tool
            .call(
                RepoArgs::Clone {
                    path: None,
                    new: false,
                },
                at(&repo),
            )
            .await
            .expect("call");
        assert!(
            result.as_text().unwrap().contains("main agent's"),
            "{result:?}"
        );
        assert!(!session_root.join("repos").exists());
    }

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
        // The workspace file lives in the profile, two levels above a session.
        let profile = tempfile::tempdir().expect("tempdir");
        let session_root = profile.path().join("sessions").join("s1");
        let context = context(repo.path(), &session_root, profile.path()).await;
        let config_path = config_path(&context);
        let tool = RepoTool::new(context);
        let set = |level: &str| RepoArgs::Trust {
            set: Some(level.to_string()),
        };

        let first = tool
            .call(set("local-branch"), ToolContext::new())
            .await
            .expect("first set");
        assert_eq!(first.as_text(), Some("trust set: local-branch"));
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
