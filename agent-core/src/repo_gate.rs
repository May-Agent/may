//! What happens to a session's repository binding before its first turn.
//!
//! - A forge-bound session with no checkout resolves its trust
//!   ([`crate::trust::resolve_forge_remote`]) and proceeds.
//! - A session that starts in a checkout resolves its trust
//!   ([`crate::trust::resolve_checkout`]) and binds directly from its trust
//!   level: forge-publishing repositories bind to their remote so PRs open
//!   from remote clones while the launch checkout remains read-only unless the
//!   user grants direct write access via `request_workspace`.
//!
//! A repo-less session is left alone.

use std::path::{Path, PathBuf};

use crate::db::{ChatDb, ChatDbError};
use crate::repo_clone::{RepoCloneError, SessionRepos};
use crate::session::{BindingError, RepoBinding};
use crate::trust::ForgeProbe;
use crate::workspace::WorkspaceConfigError;

/// Why the gate could not finish.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    /// The session database failed.
    #[error(transparent)]
    Db(#[from] ChatDbError),
    /// The workspace configuration could not be read or written.
    #[error(transparent)]
    Config(#[from] WorkspaceConfigError),
    /// The session's clone could not be made.
    #[error(transparent)]
    Clone(#[from] RepoCloneError),
    /// The stored binding is malformed.
    #[error(transparent)]
    Binding(#[from] BindingError),
    /// A repository identity could not be read.
    #[error("repository identity: {0}")]
    Identity(String),
}

/// The gate for one session.
pub struct RepoGate<'a, P> {
    /// The session database.
    pub db: &'a ChatDb,
    /// Reads forge repository access while default trust is derived.
    pub probe: &'a P,
    /// The `workspace.toml` that holds the trust decisions.
    pub config_path: &'a Path,
    /// The daemon configuration: its `[forge]` map resolves a self-hosted
    /// remote's kind.
    pub may_config: &'a crate::config::MayConfig,
    /// The session's sandbox root, where its clones live.
    pub session_root: &'a Path,
}

impl<P> std::fmt::Debug for RepoGate<'_, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoGate").finish_non_exhaustive()
    }
}

impl<P: ForgeProbe + Sync> RepoGate<'_, P> {
    /// Run the gate for `session_id`.
    ///
    /// # Errors
    /// Fails when the session, its configuration or its checkout cannot be
    /// read.
    pub async fn run(&self, session_id: &str) -> Result<(), GateError> {
        let started = std::time::Instant::now();
        tracing::debug!(session_id, "repo gate: start");
        let outcome = self.gate(session_id).await;
        match &outcome {
            Ok(()) => tracing::info!(
                session_id,
                elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "repo gate: done"
            ),
            Err(error) => tracing::info!(
                session_id,
                %error,
                elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "repo gate: failed"
            ),
        }
        outcome
    }

    async fn gate(&self, session_id: &str) -> Result<(), GateError> {
        let Some(row) = self.db.get_session(session_id).await? else {
            return Ok(());
        };
        match row.repo.as_deref().map(RepoBinding::parse).transpose()? {
            Some(RepoBinding::Forge(repository)) => match row.cwd {
                Some(cwd) => self.gate_bound_checkout(session_id, cwd).await,
                None => self.decide_remote_trust(&repository).await,
            },
            Some(RepoBinding::Local(identity)) => match row.cwd {
                Some(cwd) => {
                    let repos = SessionRepos::new(self.session_root, session_id, self.db.clone());
                    let key = identity.to_string_lossy();
                    if repos.clone_for(&key).await?.is_some() {
                        Ok(())
                    } else {
                        self.bind_checkout(session_id, PathBuf::from(cwd)).await
                    }
                }
                None => Ok(()),
            },
            None => match row.cwd {
                Some(cwd) => self.bind_checkout(session_id, PathBuf::from(cwd)).await,
                None => Ok(()),
            },
        }
    }

    /// A forge repository bound with no checkout: resolve its trust —
    /// persisted, or derived from the viewer's access — and proceed.
    async fn decide_remote_trust(
        &self,
        repository: &crate::forge::ForgeRepo,
    ) -> Result<(), GateError> {
        crate::trust::resolve_forge_remote(
            self.config_path,
            repository,
            self.may_config,
            self.probe,
        )
        .await?;
        Ok(())
    }

    /// A forge-bound session that keeps its launch checkout: rebind through
    /// the checkout only when `/permission` demoted persisted trust away from
    /// forge publishing (`local-branch` or `local`).
    async fn gate_bound_checkout(&self, session_id: &str, cwd: String) -> Result<(), GateError> {
        let demoted = {
            let (dir, config_path) = (cwd.clone(), self.config_path.to_path_buf());
            blocking::unblock(move || {
                if !crate::trust::has_non_forge_project(&config_path)? {
                    return Ok(None);
                }
                crate::trust::persisted_level(Path::new(&dir), &config_path)
            })
            .await?
        }
        .is_some_and(|level| !level.publishes_to_forge());
        if demoted {
            return self.bind_checkout(session_id, PathBuf::from(cwd)).await;
        }
        Ok(())
    }

    /// A session that starts in `cwd`: resolve its trust and bind the session
    /// directly without prompting about checkout dirtiness or remote skew.
    async fn bind_checkout(&self, session_id: &str, cwd: PathBuf) -> Result<(), GateError> {
        let worktree = {
            let cwd = cwd.clone();
            blocking::unblock(move || {
                let worktree = crate::repo_search::worktree_root(&cwd)?;
                crate::issue::repository_identity(&worktree).ok()?;
                Some(worktree)
            })
            .await
        };
        let Some(worktree) = worktree else {
            return Ok(());
        };
        let level = crate::trust::resolve_checkout(self.config_path, &worktree, self.probe).await?;
        let binding = {
            let (worktree, may_config) = (worktree.clone(), self.may_config.clone());
            blocking::unblock(move || {
                RepoBinding::for_trust_with_config(&worktree, level, &may_config)
            })
            .await
            .map_err(|error| GateError::Identity(error.to_string()))?
        };
        self.bind(session_id, &binding).await?;
        Ok(())
    }

    async fn bind(&self, session_id: &str, binding: &RepoBinding) -> Result<(), GateError> {
        self.db
            .set_session_repo(session_id, Some(&binding.to_stored()))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::{RepoKind, TrustLevel};
    use crate::workspace::WorkspaceConfig;
    use std::future::Future;

    /// A [`crate::trust::ForgeProbe`] reporting `access`, or offline when
    /// `None`.
    struct Probe(Option<crate::issue::github::RepositoryAccess>);

    impl Probe {
        fn owned() -> Self {
            Self(Some(crate::issue::github::RepositoryAccess {
                owned: true,
                can_push: true,
                has_issues: true,
            }))
        }

        const fn offline() -> Self {
            Self(None)
        }
    }

    impl crate::trust::ForgeProbe for Probe {
        fn repository_access(
            &self,
            _repository: &crate::forge::ForgeRepo,
        ) -> impl Future<
            Output = Result<crate::issue::github::RepositoryAccess, crate::issue::IssueError>,
        > + Send {
            std::future::ready(
                self.0
                    .ok_or_else(|| crate::issue::IssueError::GitHubAuth("offline".to_string())),
            )
        }
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "synchronous git fixture setup in unit tests"
    )]
    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git");
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    struct Fixture {
        tmp: tempfile::TempDir,
        origin: PathBuf,
        checkout: PathBuf,
        db: ChatDb,
        config_path: PathBuf,
        probe: Probe,
    }

    impl Fixture {
        /// A checkout whose `origin` names GitHub, beside a bare repository
        /// standing in for the network.
        #[allow(
            clippy::disallowed_methods,
            reason = "synchronous fixture directory creation in unit tests"
        )]
        async fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let origin = root.join("origin.git");
            let seed = root.join("seed");
            let checkout = root.join("checkout");
            std::fs::create_dir_all(&seed).unwrap();
            git(&seed, &["init", "-q", "-b", "main"]);
            std::fs::write(seed.join("a.txt"), "a").unwrap();
            git(&seed, &["add", "."]);
            git(&seed, &["commit", "-qm", "init"]);
            git(
                &root,
                &[
                    "clone",
                    "-q",
                    "--bare",
                    seed.to_str().unwrap(),
                    origin.to_str().unwrap(),
                ],
            );
            git(
                &root,
                &[
                    "clone",
                    "-q",
                    origin.to_str().unwrap(),
                    checkout.to_str().unwrap(),
                ],
            );
            git(
                &checkout,
                &[
                    "remote",
                    "set-url",
                    "origin",
                    "https://github.com/acme/app.git",
                ],
            );
            let db = ChatDb::open(&root.join("history.db")).await.unwrap();
            db.create_session("s", None, None).await.unwrap();
            let config_path = root.join("workspace.toml");
            Self {
                tmp,
                origin,
                checkout,
                db,
                config_path,
                probe: Probe::owned(),
            }
        }

        #[allow(
            clippy::disallowed_methods,
            reason = "synchronous canonicalization in unit tests"
        )]
        fn root(&self) -> PathBuf {
            self.tmp.path().canonicalize().unwrap()
        }

        async fn start_in_checkout(&self) {
            self.db
                .set_session_cwd("s", self.checkout.to_str().unwrap())
                .await
                .unwrap();
        }

        fn trust(&self, level: TrustLevel) {
            crate::trust::persist(
                &self.config_path,
                &self.checkout,
                Some(&RepoKind::Forge {
                    repo: crate::forge::ForgeRepo::parse_binding("github:acme/app").unwrap(),
                }),
                level,
            )
            .unwrap();
        }

        async fn run(&self) {
            let may_config = crate::config::MayConfig::default();
            RepoGate {
                db: &self.db,
                probe: &self.probe,
                config_path: &self.config_path,
                may_config: &may_config,
                session_root: &self.root().join("session"),
            }
            .run("s")
            .await
            .unwrap();
        }

        async fn binding(&self) -> Option<String> {
            self.db.get_session("s").await.unwrap().unwrap().repo
        }

        #[allow(
            clippy::disallowed_methods,
            reason = "synchronous git remote setup in unit tests"
        )]
        fn push_remote_work(&self) {
            let other = self.root().join("other");
            git(
                &self.root(),
                &[
                    "clone",
                    "-q",
                    self.origin.to_str().unwrap(),
                    other.to_str().unwrap(),
                ],
            );
            std::fs::write(other.join("b.txt"), "b").unwrap();
            git(&other, &["add", "."]);
            git(&other, &["commit", "-qm", "remote work"]);
            git(&other, &["push", "-q", "origin", "main"]);
        }
    }

    #[tokio::test]
    async fn clean_checkout_binds_silently_to_github() {
        let fx = Fixture::new().await;
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::Full);
        fx.run().await;
        assert_eq!(fx.binding().await.as_deref(), Some("github:acme/app"));
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods, reason = "synchronous test fixture writes")]
    async fn dirty_behind_and_unreachable_checkouts_bind_silently_to_github_without_copying() {
        let fx = Fixture::new().await;
        fx.push_remote_work();
        std::fs::write(fx.checkout.join("wip.txt"), "uncommitted").unwrap();
        std::fs::remove_dir_all(&fx.origin).unwrap();
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::Full);
        fx.run().await;
        assert_eq!(fx.binding().await.as_deref(), Some("github:acme/app"));
        let repos = SessionRepos::new(&fx.root().join("session"), "s", fx.db.clone());
        assert!(
            repos.clones().await.unwrap().is_empty(),
            "gate must not eagerly copy uncommitted checkout files"
        );
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods, reason = "synchronous test fixture writes")]
    async fn trust_binds_a_local_branch_checkout_without_asking() {
        let fx = Fixture::new().await;
        std::fs::write(fx.checkout.join("wip.txt"), "x").unwrap();
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::LocalBranch);
        fx.run().await;
        assert!(fx.binding().await.unwrap().starts_with("local:"));
    }

    #[tokio::test]
    async fn an_undecided_checkout_resolves_trust_and_binds() {
        let fx = Fixture::new().await;
        fx.start_in_checkout().await;
        fx.run().await;
        assert_eq!(fx.binding().await.as_deref(), Some("github:acme/app"));
        let config = WorkspaceConfig::load(&fx.config_path).unwrap();
        let identity = crate::issue::repository_identity(&fx.checkout).unwrap();
        assert_eq!(
            crate::trust::persisted_for(&config, &identity),
            Some(TrustLevel::Full)
        );
        // Decided now: the next run resolves from the persisted entry.
        fx.run().await;
        assert_eq!(fx.binding().await.as_deref(), Some("github:acme/app"));
    }

    /// A failed access lookup falls back to `local-branch`, persists
    /// nothing and binds the session to the checkout.
    #[tokio::test]
    async fn an_offline_lookup_falls_back_to_local_branch_unpersisted() {
        let mut fx = Fixture::new().await;
        fx.start_in_checkout().await;
        fx.probe = Probe::offline();
        fx.run().await;
        assert!(fx.binding().await.unwrap().starts_with("local:"));
        assert!(!fx.config_path.exists());
    }

    /// An explicit decision wins over the access lookup: the probe is not
    /// consulted and the level stays.
    #[tokio::test]
    async fn a_persisted_decision_is_never_overwritten() {
        let fx = Fixture::new().await;
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::Local);
        fx.run().await;
        assert!(fx.binding().await.unwrap().starts_with("local:"));
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods, reason = "synchronous test fixture writes")]
    async fn session_launch_repo_local_checkout_rebinds_to_github_silently() {
        let fx = Fixture::new().await;
        std::fs::write(fx.checkout.join("wip.txt"), "x").unwrap();
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::Full);
        let identity = crate::issue::repository_identity(&fx.checkout).unwrap();
        let local_binding = format!("local:{}", identity.display());
        fx.db
            .set_session_repo("s", Some(&local_binding))
            .await
            .unwrap();
        fx.run().await;
        assert_eq!(fx.binding().await.as_deref(), Some("github:acme/app"));
    }

    /// A `/permission` switch away from a publishing level rebinds a
    /// GitHub-bound checkout session to `local:<identity>` on the next
    /// run, without asking anything.
    #[tokio::test]
    async fn a_demoted_trust_rebinds_a_github_bound_checkout_session_locally() {
        let fx = Fixture::new().await;
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::Full);
        fx.run().await;
        assert_eq!(fx.binding().await.as_deref(), Some("github:acme/app"));
        fx.trust(TrustLevel::LocalBranch);
        fx.run().await;
        let identity = crate::issue::repository_identity(&fx.checkout).unwrap();
        assert_eq!(
            fx.binding().await,
            Some(format!("local:{}", identity.display()))
        );
        // The local binding holds: the next run does not flip back.
        fx.run().await;
        assert_eq!(
            fx.binding().await,
            Some(format!("local:{}", identity.display()))
        );
    }

    #[tokio::test]
    async fn a_repo_less_session_stays_unbound() {
        let fx = Fixture::new().await;
        fx.run().await;
        assert_eq!(fx.binding().await, None);
    }

    /// A session launched on a GitHub repository with no checkout resolves
    /// its trust from access and persists it — no question is asked.
    #[tokio::test]
    async fn a_github_bound_session_with_no_checkout_resolves_trust() {
        let mut fx = Fixture::new().await;
        fx.db
            .set_session_repo("s", Some("github:acme/app"))
            .await
            .unwrap();
        fx.run().await;
        let config = WorkspaceConfig::load(&fx.config_path).unwrap();
        assert_eq!(
            crate::trust::trust_for_github(&config, "acme/app"),
            Some(TrustLevel::Full)
        );
        // Decided now: the next turn resolves from the persisted entry.
        fx.probe = Probe::offline();
        fx.run().await;
    }

    /// An offline access lookup on a remote binding falls back to
    /// `prs-only` and persists nothing.
    #[tokio::test]
    async fn an_offline_remote_binding_proceeds_unpersisted() {
        let mut fx = Fixture::new().await;
        fx.db
            .set_session_repo("s", Some("github:acme/app"))
            .await
            .unwrap();
        fx.probe = Probe::offline();
        fx.run().await;
        assert!(!fx.config_path.exists());
    }
}
