//! What happens to a session's repository binding before its first turn.
//!
//! - A GitHub-bound session needs no checkout, so the trust decision for the
//!   repository is asked here when it is still open.
//! - A session that starts in a checkout of a GitHub repository whose trust is
//!   decided gets the checkout-state preflight ([`crate::repo_preflight`]):
//!   clean and current proceeds silently, anything else asks.
//! - Then the session is bound: trust, or the preflight's choice, sets the
//!   binding that groups the session and names its clone.
//!
//! A repo-less session, or one in a repository with no decision yet, is left
//! alone: those are decided by their own flows.

use std::future::Future;
use std::path::{Path, PathBuf};

use crate::ask_user::{Question, QuestionAnswer};
use crate::db::{ChatDb, ChatDbError};
use crate::host_git::HostGit;
use crate::repo_clone::{RepoCloneError, SessionRepos};
use crate::repo_preflight::{Choice, PreflightError};
use crate::session::{BindingError, RepoBinding};
use crate::trust::RepoKind;
use crate::workspace::{WorkspaceConfig, WorkspaceConfigError};

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
pub struct RepoGate<'a, A> {
    /// The session database.
    pub db: &'a ChatDb,
    /// Git on the host.
    pub host: &'a HostGit,
    /// Who the questions go to.
    pub asker: &'a A,
    /// The `workspace.toml` that holds the trust decisions.
    pub config_path: &'a Path,
    /// The session's sandbox root, where its clones live.
    pub session_root: &'a Path,
    /// Host scratch space new clones are made in.
    pub staging: &'a Path,
    /// The remote the preflight compares against.
    pub remote: &'a str,
}

impl<A> std::fmt::Debug for RepoGate<'_, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoGate").finish_non_exhaustive()
    }
}

impl<A: Asker + Sync> RepoGate<'_, A> {
    /// Run the gate for `session_id`.
    ///
    /// # Errors
    /// Fails when the session, its configuration or its checkout cannot be
    /// read, a clone cannot be made, or the user cannot be asked.
    pub async fn run(&self, session_id: &str) -> Result<Verdict, GateError> {
        let Some(row) = self.db.get_session(session_id).await? else {
            return Ok(Verdict::Proceed);
        };
        let path = self.config_path.to_path_buf();
        let config = blocking::unblock(move || WorkspaceConfig::load(&path)).await?;
        match row.repo.as_deref().map(RepoBinding::parse).transpose()? {
            Some(RepoBinding::Github(repository)) => {
                self.decide_remote_trust(&config, &repository.to_string())
                    .await
            }
            Some(RepoBinding::Local(_)) => Ok(Verdict::Proceed),
            None => match row.cwd {
                Some(cwd) => {
                    self.bind_checkout(session_id, &config, PathBuf::from(cwd))
                        .await
                }
                None => Ok(Verdict::Proceed),
            },
        }
    }

    /// First contact with a GitHub repository that has no checkout: ask for
    /// the trust level once.
    async fn decide_remote_trust(
        &self,
        config: &WorkspaceConfig,
        owner_repo: &str,
    ) -> Result<Verdict, GateError> {
        // Matching a project reads its checkout's remote: off the executor.
        let decided = {
            let (config, owner_repo) = (config.clone(), owner_repo.to_string());
            blocking::unblock(move || crate::trust::trust_for_github(&config, &owner_repo)).await
        };
        if decided.is_some() {
            return Ok(Verdict::Proceed);
        }
        let answers = self
            .asker
            .ask(crate::trust::remote_question(owner_repo))
            .await?;
        let Some(level) = answers.first().and_then(crate::trust::level_for_answer) else {
            return Ok(Verdict::Exit);
        };
        let (path, owner_repo) = (self.config_path.to_path_buf(), owner_repo.to_string());
        blocking::unblock(move || crate::trust::persist_github(&path, &owner_repo, level)).await?;
        Ok(Verdict::Proceed)
    }

    /// A session that starts in `cwd`: preflight a GitHub checkout, then bind
    /// the session to its repository.
    async fn bind_checkout(
        &self,
        session_id: &str,
        config: &WorkspaceConfig,
        cwd: PathBuf,
    ) -> Result<Verdict, GateError> {
        let config = config.clone();
        let found = blocking::unblock(move || {
            let worktree = crate::repo_search::worktree_root(&cwd)?;
            let identity = crate::issue::repository_identity(&worktree).ok()?;
            let level = crate::trust::trust_for(&config, &identity, &worktree)?;
            Some((worktree, identity, level))
        })
        .await;
        let Some((worktree, identity, level)) = found else {
            return Ok(Verdict::Proceed);
        };
        if level.publishes_to_github()
            && let Some(verdict) = self.preflight(session_id, &worktree, &identity).await?
        {
            return Ok(verdict);
        }
        let binding = {
            let worktree = worktree.clone();
            blocking::unblock(move || RepoBinding::for_trust(&worktree, level))
                .await
                .map_err(|error| GateError::Identity(error.to_string()))?
        };
        self.bind(session_id, &binding).await?;
        Ok(Verdict::Proceed)
    }

    /// The checkout-state preflight. `None` when the checkout needs no
    /// choice (or the session already holds its clone), so the trust level
    /// binds the session; `Some` when a choice settled the binding.
    async fn preflight(
        &self,
        session_id: &str,
        worktree: &Path,
        identity: &Path,
    ) -> Result<Option<Verdict>, GateError> {
        let RepoKind::Github { owner_repo } = ({
            let worktree = worktree.to_path_buf();
            blocking::unblock(move || crate::trust::repo_kind(&worktree)).await
        }) else {
            return Ok(None);
        };
        let repos = SessionRepos::new(self.session_root, session_id, self.db.clone())
            .with_host(self.host.clone());
        let identity_key = identity.to_string_lossy();
        if repos.clone_for(&owner_repo).await?.is_some()
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
            blocking::unblock(move || {
                crate::repo_preflight::inspect_against(&host, &worktree, &remote)
            })
            .await
        };
        let state = match inspected {
            Ok(state) => state,
            Err(PreflightError::RemoteUnreachable(_)) => {
                return self
                    .offline(session_id, &repos, worktree, identity, &owner_repo)
                    .await
                    .map(Some);
            }
            Err(error) => return Err(error.into()),
        };
        if !state.needs_choice() {
            return Ok(None);
        }
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
                let repository = owner_repo.parse().map_err(
                    |error: crate::workspace::GitHubRepositoryError| {
                        GateError::Identity(error.to_string())
                    },
                )?;
                self.bind(session_id, &RepoBinding::Github(repository))
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
                    .ensure_remote_wip_clone(url.trim(), &owner_repo, self.staging, worktree)
                    .await?;
                let repository = owner_repo.parse().map_err(
                    |error: crate::workspace::GitHubRepositoryError| {
                        GateError::Identity(error.to_string())
                    },
                )?;
                self.bind(session_id, &RepoBinding::Github(repository))
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
    use std::sync::Mutex;

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
        #[allow(clippy::unused_async_trait_impl, reason = "the trait method is async")]
        async fn ask(&self, question: Question) -> Result<Vec<QuestionAnswer>, GateError> {
            let section = question.section.clone();
            self.asked.lock().unwrap().push(question);
            let answer = self
                .answer
                .ok_or_else(|| GateError::Ask("no one was meant to be asked".to_string()))?;
            Ok(vec![QuestionAnswer {
                section,
                answer: AnswerValue::Selected(vec![answer.to_string()]),
            }])
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
                Some(&RepoKind::Github {
                    owner_repo: "acme/app".to_string(),
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
            RepoGate {
                db: &self.db,
                host: &host,
                asker,
                config_path: &self.config_path,
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
    async fn an_undecided_repository_is_left_for_its_own_flow() {
        let fx = Fixture::new().await;
        fx.start_in_checkout().await;
        let asker = Scripted::silent();
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(fx.binding().await, None);
    }

    #[tokio::test]
    async fn a_repo_less_session_stays_unbound() {
        let fx = Fixture::new().await;
        let asker = Scripted::silent();
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 0);
        assert_eq!(fx.binding().await, None);
    }

    #[tokio::test]
    async fn a_github_bound_session_with_no_checkout_asks_trust_once() {
        let fx = Fixture::new().await;
        fx.db
            .set_session_repo("s", Some("github:acme/app"))
            .await
            .unwrap();
        let asker = Scripted::answering("prs-only — GitHub PRs only");
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 1);
        let config = WorkspaceConfig::load(&fx.config_path).unwrap();
        assert_eq!(
            crate::trust::trust_for_github(&config, "acme/app"),
            Some(TrustLevel::PrsOnly)
        );
        // Decided now: the next turn asks nothing.
        assert_eq!(fx.run(&asker).await, Verdict::Proceed);
        assert_eq!(asker.asked(), 1);
    }

    #[tokio::test]
    async fn declining_the_remote_trust_question_exits() {
        let fx = Fixture::new().await;
        fx.db
            .set_session_repo("s", Some("github:acme/app"))
            .await
            .unwrap();
        let asker = Scripted::answering("exit — Do not bind this repository");
        assert_eq!(fx.run(&asker).await, Verdict::Exit);
        let config = WorkspaceConfig::load(&fx.config_path).unwrap();
        assert_eq!(crate::trust::trust_for_github(&config, "acme/app"), None);
    }
}
