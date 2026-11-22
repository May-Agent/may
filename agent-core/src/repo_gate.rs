//! What happens to a session's repository binding before its first turn.
//!
//! - A GitHub-bound session needs no checkout, so its trust is resolved
//!   here ([`crate::trust::resolve_github_remote`]).
//! - A session that starts in a checkout has its trust resolved
//!   ([`crate::trust::resolve_checkout`]): a level that publishes to GitHub
//!   gets the checkout-state preflight ([`crate::repo_preflight`]) — clean
//!   and current proceeds silently, anything else asks unless the session
//!   is non-interactive.
//! - Then the session is bound: trust, or the preflight's choice, sets the
//!   binding that groups the session and names its clone.
//!
//! A repo-less session is left alone.

use std::future::Future;
use std::path::{Path, PathBuf};

use crate::ask_user::{Question, QuestionAnswer};
use crate::db::{ChatDb, ChatDbError};
use crate::host_git::HostGit;
use crate::repo_clone::{RepoCloneError, SessionRepos};
use crate::repo_preflight::{Choice, PreflightError};
use crate::session::{BindingError, RepoBinding};
use crate::trust::{ForgeProbe, RepoKind};
use crate::workspace::WorkspaceConfigError;

/// Puts a question to the user and waits for the answers.
pub trait Asker {
    /// Ask `question`.
    ///
    /// # Errors
    /// Fails when nobody can be asked.
    fn ask(
        &self,
        question: Question,
    ) -> impl Future<Output = Result<Vec<QuestionAnswer>, GateError>> + Send;
}

/// Why the gate could not finish.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    /// The session database failed.
    #[error(transparent)]
    Db(#[from] ChatDbError),
    /// The workspace configuration could not be read or written.
    #[error(transparent)]
    Config(#[from] WorkspaceConfigError),
    /// The checkout could not be inspected.
    #[error(transparent)]
    Preflight(#[from] PreflightError),
    /// The session's clone could not be made.
    #[error(transparent)]
    Clone(#[from] RepoCloneError),
    /// The stored binding is malformed.
    #[error(transparent)]
    Binding(#[from] BindingError),
    /// A repository identity could not be read.
    #[error("repository identity: {0}")]
    Identity(String),
    /// The question could not be put to the user.
    #[error("cannot ask the user: {0}")]
    Ask(String),
}

/// Whether the session may run its turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The session proceeds.
    Proceed,
    /// The user chose not to work here; the turn does not run.
    Exit,
}

/// The gate for one session.
pub struct RepoGate<'a, A, P> {
    /// The session database.
    pub db: &'a ChatDb,
    /// Git on the host.
    pub host: &'a HostGit,
    /// Who the questions go to.
    pub asker: &'a A,
    /// Reads forge repository access while default trust is derived.
    pub probe: &'a P,
    /// The live forge authenticator clones authenticate with.
    pub forge_auth: &'a crate::forge::LiveForgeAuth,
    /// The `workspace.toml` that holds the trust decisions.
    pub config_path: &'a Path,
    /// The daemon configuration: its `[forge]` map resolves a self-hosted
    /// remote's kind.
    pub may_config: &'a crate::config::MayConfig,
    /// The session's sandbox root, where its clones live.
    pub session_root: &'a Path,
    /// Host scratch space new clones are made in.
    pub staging: &'a Path,
    /// The remote the preflight compares against.
    pub remote: &'a str,
}

impl<A, P> std::fmt::Debug for RepoGate<'_, A, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoGate").finish_non_exhaustive()
    }
}

impl<A: Asker + Sync, P: ForgeProbe + Sync> RepoGate<'_, A, P> {
    /// Run the gate for `session_id`.
    ///
    /// # Errors
    /// Fails when the session, its configuration or its checkout cannot be
    /// read, a clone cannot be made, or the user cannot be asked.
    pub async fn run(&self, session_id: &str) -> Result<Verdict, GateError> {
        let started = std::time::Instant::now();
        tracing::debug!(session_id, "repo gate: start");
        let verdict = self.gate(session_id).await;
        match &verdict {
            Ok(verdict) => tracing::info!(
                session_id,
                ?verdict,
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
        verdict
    }

    async fn gate(&self, session_id: &str) -> Result<Verdict, GateError> {
        let Some(row) = self.db.get_session(session_id).await? else {
            return Ok(Verdict::Proceed);
        };
        let non_interactive = row.non_interactive;
        match row.repo.as_deref().map(RepoBinding::parse).transpose()? {
            Some(RepoBinding::Forge(repository)) => match row.cwd {
                Some(cwd) => {
                    self.gate_bound_checkout(session_id, cwd, &repository, non_interactive)
                        .await
                }
                None => self.decide_remote_trust(&repository).await,
            },
            Some(RepoBinding::Local(identity)) => match row.cwd {
                Some(cwd) => {
                    let repos = SessionRepos::new(self.session_root, session_id, self.db.clone())
                        .with_host(self.host.clone());
                    let key = identity.to_string_lossy();
                    if repos.clone_for(&key).await?.is_some() {
                        Ok(Verdict::Proceed)
                    } else {
                        self.bind_checkout(session_id, PathBuf::from(cwd), non_interactive)
                            .await
                    }
                }
                None => Ok(Verdict::Proceed),
            },
            None => match row.cwd {
                Some(cwd) => {
                    self.bind_checkout(session_id, PathBuf::from(cwd), non_interactive)
                        .await
                }
                None => Ok(Verdict::Proceed),
            },
        }
    }

    /// A forge repository bound with no checkout: resolve its trust —
    /// persisted, or derived from the viewer's access — and proceed.
    async fn decide_remote_trust(
        &self,
        repository: &crate::forge::ForgeRepo,
    ) -> Result<Verdict, GateError> {
        crate::trust::resolve_forge_remote(
            self.config_path,
            repository,
            self.may_config,
            self.probe,
        )
        .await?;
        Ok(Verdict::Proceed)
    }

    /// A forge-bound session that keeps its launch checkout: rebind
    /// through the checkout when the remote clone the binding names was
    /// never made, or when the checkout's persisted trust no longer
    /// publishes to the forge — `/permission` switching to `local-branch`
    /// or `local` moves the session to `local:<identity>`. A session whose
    /// remote clone exists under a still-publishing trust proceeds
    /// untouched.
    async fn gate_bound_checkout(
        &self,
        session_id: &str,
        cwd: String,
        repository: &crate::forge::ForgeRepo,
        non_interactive: bool,
    ) -> Result<Verdict, GateError> {
        let demoted = {
            let (dir, config_path) = (cwd.clone(), self.config_path.to_path_buf());
            blocking::unblock(move || crate::trust::persisted_level(Path::new(&dir), &config_path))
                .await?
        }
        .is_some_and(|level| !level.publishes_to_forge());
        if !demoted {
            let repos = SessionRepos::new(self.session_root, session_id, self.db.clone())
                .with_host(self.host.clone());
            if repos.clone_for(&repository.cache_key()).await?.is_some() {
                return Ok(Verdict::Proceed);
            }
        }
        self.bind_checkout(session_id, PathBuf::from(cwd), non_interactive)
            .await
    }

    /// A session that starts in `cwd`: resolve its trust, preflight a
    /// GitHub-publishing checkout, then bind the session to its repository.
    async fn bind_checkout(
        &self,
        session_id: &str,
        cwd: PathBuf,
        non_interactive: bool,
    ) -> Result<Verdict, GateError> {
        let found = {
            let cwd = cwd.clone();
            blocking::unblock(move || {
                let worktree = crate::repo_search::worktree_root(&cwd)?;
                let identity = crate::issue::repository_identity(&worktree).ok()?;
                Some((worktree, identity))
            })
            .await
        };
        let Some((worktree, identity)) = found else {
            return Ok(Verdict::Proceed);
        };
        let level = crate::trust::resolve_checkout(self.config_path, &worktree, self.probe).await?;
        if level.publishes_to_forge()
            && let Some(verdict) = self
                .preflight(session_id, &worktree, &identity, non_interactive)
                .await?
        {
            return Ok(verdict);
        }
        let binding = {
            let (worktree, may_config) = (worktree.clone(), self.may_config.clone());
            blocking::unblock(move || {
                RepoBinding::for_trust_with_config(&worktree, level, &may_config)
            })
            .await
            .map_err(|error| GateError::Identity(error.to_string()))?
        };
        self.bind(session_id, &binding).await?;
        Ok(Verdict::Proceed)
    }

    /// The checkout-state preflight. `None` when the checkout needs no
    /// choice (or the session already holds its clone), so the trust level
    /// binds the session; `Some` when a choice settled the binding. A
    /// non-interactive session never asks: it proceeds on the trust level.
    async fn preflight(
        &self,
        session_id: &str,
        worktree: &Path,
        identity: &Path,
        non_interactive: bool,
    ) -> Result<Option<Verdict>, GateError> {
        let RepoKind::Forge { repo } = ({
            let (worktree, may_config) = (worktree.to_path_buf(), self.may_config.clone());
            blocking::unblock(move || crate::trust::repo_kind_with_config(&worktree, &may_config))
                .await
        }) else {
            return Ok(None);
        };
        let owner_repo = repo.host_and_path();
        let repos = SessionRepos::new(self.session_root, session_id, self.db.clone())
            .with_host(self.host.clone())
            .with_may_config(self.may_config.clone())
            .with_forge_auth(self.forge_auth.clone());
        let identity_key = identity.to_string_lossy();
        if repos.clone_for(&repo.cache_key()).await?.is_some()
            || repos.clone_for(&identity_key).await?.is_some()
        {
            return Ok(None);
        }
        let inspected = {
            let (host, worktree, remote) = (
                self.host.clone(),
                worktree.to_path_buf(),
                self.remote.to_string(),
            );
            let started = std::time::Instant::now();
            let inspected = blocking::unblock(move || {
                crate::repo_preflight::inspect_against(&host, &worktree, &remote)
            })
            .await;
            tracing::debug!(
                session_id,
                elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "repo gate: checkout state inspected"
            );
            inspected
        };
        let state = match inspected {
            Ok(state) => state,
            Err(PreflightError::RemoteUnreachable(_)) => {
                if non_interactive {
                    return Ok(None);
                }
                return self
                    .offline(session_id, &repos, worktree, identity, &owner_repo)
                    .await
                    .map(Some);
            }
            Err(error) => return Err(error.into()),
        };
        if !state.needs_choice() || non_interactive {
            return Ok(None);
        }
        tracing::info!(
            session_id,
            worktree = %worktree.display(),
            ?state,
            "repo gate: checkout state needs a choice; asking"
        );
        let answers = self
            .asker
            .ask(crate::repo_preflight::question(
                worktree,
                &owner_repo,
                state,
            ))
            .await?;
        match answers.first().and_then(Choice::of_answer) {
            Some(Choice::Remote) => {
                self.bind(session_id, &RepoBinding::Forge(repo.clone()))
                    .await?;
                Ok(Some(Verdict::Proceed))
            }
            Some(Choice::Checkout) if state.loses_github() => {
                repos
                    .ensure_wip_clone(worktree, identity, self.staging)
                    .await?;
                self.bind(session_id, &RepoBinding::Local(identity.to_path_buf()))
                    .await?;
                Ok(Some(Verdict::Proceed))
            }
            // Only stale-and-dirty gives up GitHub: any other checkout is the
            // remote's history plus, at most, its uncommitted work, so the
            // work-in-progress commit goes on a clone of the remote.
            Some(Choice::Checkout) => {
                let url = {
                    let (host, worktree, remote) = (
                        self.host.clone(),
                        worktree.to_path_buf(),
                        self.remote.to_string(),
                    );
                    blocking::unblock(move || {
                        host.output(Some(&worktree), &["ls-remote", "--get-url", &remote])
                    })
                    .await
                    .map_err(|error| GateError::Identity(error.to_string()))?
                };
                repos
                    .ensure_remote_wip_clone(url.trim(), &repo.cache_key(), self.staging, worktree)
                    .await?;
                self.bind(session_id, &RepoBinding::Forge(repo.clone()))
                    .await?;
                Ok(Some(Verdict::Proceed))
            }
            Some(Choice::Exit) | None => Ok(Some(Verdict::Exit)),
        }
    }

    /// The remote cannot be reached, so the checkout's state against it is
    /// unknown: the user chooses to continue on the checkout as it stands
    /// (a local binding on its work in progress) or to exit.
    async fn offline(
        &self,
        session_id: &str,
        repos: &SessionRepos,
        worktree: &Path,
        identity: &Path,
        owner_repo: &str,
    ) -> Result<Verdict, GateError> {
        let dirty = {
            let (host, worktree) = (self.host.clone(), worktree.to_path_buf());
            blocking::unblock(move || crate::repo_preflight::is_dirty(&host, &worktree)).await?
        };
        let answers = self
            .asker
            .ask(crate::repo_preflight::offline_question(
                worktree, owner_repo, dirty,
            ))
            .await?;
        match answers.first().and_then(Choice::of_answer) {
            Some(Choice::Checkout) => {
                repos
                    .ensure_wip_clone(worktree, identity, self.staging)
                    .await?;
                self.bind(session_id, &RepoBinding::Local(identity.to_path_buf()))
                    .await?;
                Ok(Verdict::Proceed)
            }
            Some(Choice::Remote | Choice::Exit) | None => Ok(Verdict::Exit),
        }
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
    use crate::ask_user::AnswerValue;
    use crate::trust::TrustLevel;
    use crate::workspace::WorkspaceConfig;
    use std::sync::Mutex;

    /// A [`crate::trust::GitHubProbe`] reporting `access`, or offline when
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

    /// Answers every question with a fixed option and records the questions.
    struct Scripted {
        answer: Option<&'static str>,
        asked: Mutex<Vec<Question>>,
    }

    impl Scripted {
        fn answering(answer: &'static str) -> Self {
            Self {
                answer: Some(answer),
                asked: Mutex::default(),
            }
        }

        fn silent() -> Self {
            Self {
                answer: None,
                asked: Mutex::default(),
            }
        }

        fn asked(&self) -> usize {
            self.asked.lock().unwrap().len()
        }
    }

    impl Asker for Scripted {
        fn ask(
            &self,
            question: Question,
        ) -> impl Future<Output = Result<Vec<QuestionAnswer>, GateError>> + Send {
            let section = question.section.clone();
            self.asked.lock().unwrap().push(question);
            let answers = self.answer.map(|answer| {
                vec![QuestionAnswer {
                    section,
                    answer: AnswerValue::Selected(vec![answer.to_string()]),
                }]
            });
            std::future::ready(
                answers.ok_or_else(|| GateError::Ask("no one was meant to be asked".to_string())),
            )
        }
    }

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

        async fn run(&self, asker: &Scripted) -> Verdict {
            // The user's git configuration: the work-in-progress commit needs
            // an author.
            let config = self.root().join("gitconfig");
            std::fs::write(&config, "[user]\n\tname = t\n\temail = t@t\n").unwrap();
            let host = HostGit::from_file(config);
            let staging = self.root().join("staging");
            let remote = self.origin.to_str().unwrap().to_string();
            let may_config = crate::config::MayConfig::default();
            let forge_auth = crate::forge::LiveForgeAuth::default();
            RepoGate {
                db: &self.db,
                host: &host,
                asker,
                probe: &self.probe,
                forge_auth: &forge_auth,
                config_path: &self.config_path,
                may_config: &may_config,
                session_root: &self.root().join("session"),
                staging: &staging,
                remote: &remote,
            }
            .run("s")
            .await
            .unwrap()
        }

        async fn binding(&self) -> Option<String> {
            self.db.get_session("s").await.unwrap().unwrap().repo
        }

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
    async fn a_clean_current_checkout_binds_silently_to_github() {
        let fx = Fixture::new().await;
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::Full);
        let asker = Scripted::silent();
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 0);
        assert_eq!(fx.binding().await.as_deref(), Some("github:acme/app"));
    }

    #[tokio::test]
    async fn a_dirty_checkout_asks_and_the_remote_option_binds_github() {
        let fx = Fixture::new().await;
        std::fs::write(fx.checkout.join("wip.txt"), "x").unwrap();
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::Full);
        let asker = Scripted::answering("remote — Base on the latest clean remote");
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 1);
        assert_eq!(fx.binding().await.as_deref(), Some("github:acme/app"));
    }

    #[tokio::test]
    async fn a_behind_checkout_asks_and_the_checkout_option_binds_locally_on_a_wip_base() {
        let fx = Fixture::new().await;
        fx.push_remote_work();
        std::fs::write(fx.checkout.join("wip.txt"), "uncommitted").unwrap();
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::Full);
        let asker = Scripted::answering("checkout — Base on this checkout");
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 1);
        let identity = crate::issue::repository_identity(&fx.checkout).unwrap();
        assert_eq!(
            fx.binding().await,
            Some(format!("local:{}", identity.display()))
        );
        let repos = SessionRepos::new(&fx.root().join("session"), "s", fx.db.clone());
        let record = repos.clones().await.unwrap().remove(0);
        assert!(record.local);
        let base = record.base_sha.expect("a work-in-progress base commit");
        let clone = repos.path(&record.dir);
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), base);
        assert_eq!(
            std::fs::read_to_string(clone.join("wip.txt")).unwrap(),
            "uncommitted"
        );
        assert_eq!(git(&clone, &["status", "--porcelain"]), "");
    }

    #[tokio::test]
    async fn a_dirty_current_checkout_keeps_github_on_a_remote_clone_with_the_wip_base() {
        let fx = Fixture::new().await;
        std::fs::write(fx.checkout.join("wip.txt"), "uncommitted").unwrap();
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::Full);
        let asker = Scripted::answering("checkout — Base on this checkout");
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(fx.binding().await.as_deref(), Some("github:acme/app"));
        let repos = SessionRepos::new(&fx.root().join("session"), "s", fx.db.clone());
        let record = repos.clones().await.unwrap().remove(0);
        assert!(!record.local, "the clone is sourced from the remote");
        let base = record.base_sha.expect("a work-in-progress base commit");
        let clone = repos.path(&record.dir);
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), base);
        assert_eq!(
            std::fs::read_to_string(clone.join("wip.txt")).unwrap(),
            "uncommitted"
        );
    }

    #[tokio::test]
    async fn an_unreachable_remote_asks_and_the_checkout_option_binds_locally() {
        let fx = Fixture::new().await;
        std::fs::write(fx.checkout.join("wip.txt"), "offline").unwrap();
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::Full);
        std::fs::remove_dir_all(&fx.origin).unwrap();
        let asker = Scripted::answering("checkout — Work on this checkout as it stands");
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 1);
        assert!(fx.binding().await.unwrap().starts_with("local:"));
        let repos = SessionRepos::new(&fx.root().join("session"), "s", fx.db.clone());
        let record = repos.clones().await.unwrap().remove(0);
        assert!(record.local && record.base_sha.is_some());
    }

    #[tokio::test]
    async fn an_unreachable_remote_can_end_the_start() {
        let fx = Fixture::new().await;
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::Full);
        std::fs::remove_dir_all(&fx.origin).unwrap();
        let asker = Scripted::answering("exit — Do not work in this directory");
        assert_eq!(fx.run(&asker).await, Verdict::Exit);
        assert_eq!(fx.binding().await, None);
    }

    #[tokio::test]
    async fn exit_leaves_the_session_unbound_and_does_not_run() {
        let fx = Fixture::new().await;
        std::fs::write(fx.checkout.join("wip.txt"), "x").unwrap();
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::Full);
        let asker = Scripted::answering("exit — Do not work in this directory");
        assert_eq!(fx.run(&asker).await, Verdict::Exit);
        assert_eq!(fx.binding().await, None);
    }

    #[tokio::test]
    async fn trust_binds_a_local_branch_checkout_without_asking() {
        let fx = Fixture::new().await;
        std::fs::write(fx.checkout.join("wip.txt"), "x").unwrap();
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::LocalBranch);
        let asker = Scripted::silent();
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 0);
        assert!(fx.binding().await.unwrap().starts_with("local:"));
    }

    #[tokio::test]
    async fn an_undecided_checkout_resolves_trust_and_binds() {
        let fx = Fixture::new().await;
        fx.start_in_checkout().await;
        let asker = Scripted::silent();
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 0);
        assert_eq!(fx.binding().await.as_deref(), Some("github:acme/app"));
        let config = WorkspaceConfig::load(&fx.config_path).unwrap();
        let identity = crate::issue::repository_identity(&fx.checkout).unwrap();
        assert_eq!(
            crate::trust::persisted_for(&config, &identity),
            Some(TrustLevel::Full)
        );
        // Decided now: the next run resolves from the persisted entry.
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 0);
    }

    /// A failed access lookup falls back to `local-branch`, persists
    /// nothing and binds the session to the checkout.
    #[tokio::test]
    async fn an_offline_lookup_falls_back_to_local_branch_unpersisted() {
        let mut fx = Fixture::new().await;
        fx.start_in_checkout().await;
        fx.probe = Probe::offline();
        let asker = Scripted::silent();
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 0);
        assert!(fx.binding().await.unwrap().starts_with("local:"));
        assert!(!fx.config_path.exists());
    }

    /// A non-interactive session resolves the same way and never asks, even
    /// when the remote cannot be reached.
    #[tokio::test]
    async fn a_non_interactive_session_never_asks() {
        let fx = Fixture::new().await;
        fx.start_in_checkout().await;
        fx.db.set_session_non_interactive("s").await.unwrap();
        let asker = Scripted::silent();
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 0);
        assert_eq!(fx.binding().await.as_deref(), Some("github:acme/app"));
    }

    /// An explicit decision wins over the access lookup: the probe is not
    /// consulted and the level stays.
    #[tokio::test]
    async fn a_persisted_decision_is_never_overwritten() {
        let fx = Fixture::new().await;
        fx.start_in_checkout().await;
        fx.trust(TrustLevel::Local);
        let asker = Scripted::silent();
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 0);
        assert!(fx.binding().await.unwrap().starts_with("local:"));
    }

    #[tokio::test]
    async fn session_launch_repo_local_checkout_runs_preflight() {
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
        let asker = Scripted::answering("remote — Base on the latest clean remote");
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 1);
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
        let asker = Scripted::silent();
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(fx.binding().await.as_deref(), Some("github:acme/app"));
        fx.trust(TrustLevel::LocalBranch);
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 0);
        let identity = crate::issue::repository_identity(&fx.checkout).unwrap();
        assert_eq!(
            fx.binding().await,
            Some(format!("local:{}", identity.display()))
        );
        // The local binding holds: the next run does not flip back.
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(
            fx.binding().await,
            Some(format!("local:{}", identity.display()))
        );
        assert_eq!(asker.asked(), 0);
    }

    #[tokio::test]
    async fn a_repo_less_session_stays_unbound() {
        let fx = Fixture::new().await;
        let asker = Scripted::silent();
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 0);
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
        let asker = Scripted::silent();
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 0);
        let config = WorkspaceConfig::load(&fx.config_path).unwrap();
        assert_eq!(
            crate::trust::trust_for_github(&config, "acme/app"),
            Some(TrustLevel::Full)
        );
        // Decided now: the next turn resolves from the persisted entry.
        fx.probe = Probe::offline();
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 0);
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
        let asker = Scripted::silent();
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 0);
        assert!(!fx.config_path.exists());
    }
}
