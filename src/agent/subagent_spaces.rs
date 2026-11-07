//! The spaces subagent runs work in: a child clone of the repository the run
//! was launched from.
//!
//! aither gives every subagent run a space of its own and has the terminal's
//! [`SubagentSpaces`] fill it first. For a run launched from inside a session
//! clone, May makes a child clone of that clone in the space, at the clone's
//! current commit, and names it as the run's working copy: the run's work is
//! `git diff base..head` of that copy, and reaches the session clone only
//! when the main agent fetches and merges it. A run launched anywhere else
//! gets an empty space. Spaces lie in the launcher's working directory, so
//! they leave with the session, and the main agent can remove one once its
//! work is merged.

use std::path::{Path, PathBuf};

use aither_sandbox::{PreparedSpace, RunSpace, SubagentSpaces};

use crate::repo_clone::{RepoCloneError, SessionRepos};

/// The name of the working copy in a run's space. A run launched from inside
/// it, one level down, clones it in turn.
const WORKING_COPY: &str = "repo";

/// Why a run's space could not be prepared.
#[derive(Debug, thiserror::Error)]
pub enum SubagentSpaceError {
    /// A directory the launch names does not resolve.
    #[error("cannot resolve {path}: {source}")]
    Resolve {
        /// The directory.
        path: PathBuf,
        /// Why it does not resolve.
        source: std::io::Error,
    },
    /// The session's clones could not be read, or the child clone failed.
    #[error(transparent)]
    Clone(#[from] RepoCloneError),
}

/// Prepares each subagent run's space of a session: a child clone of the
/// session clone the run was launched from.
#[derive(Debug, Clone)]
pub struct SessionSubagentSpaces {
    repos: SessionRepos,
}

impl SessionSubagentSpaces {
    /// Spaces for the session whose clones `repos` manages. The child clones
    /// are made by `repos`' git.
    #[must_use]
    pub const fn new(repos: SessionRepos) -> Self {
        Self { repos }
    }

    /// The clone `run` was launched from inside, by its canonical path: a
    /// clone the session registered, or the working copy of the launcher's
    /// own space when the launcher is a subagent.
    async fn launched_from(
        &self,
        run: &RunSpace<'_>,
    ) -> Result<Option<PathBuf>, SubagentSpaceError> {
        let caller = resolve(run.caller).await?;
        let mut sources = Vec::new();
        for record in self.repos.clones().await? {
            match async_fs::canonicalize(self.repos.path(&record.dir)).await {
                Ok(path) => sources.push(path),
                // A clone removed behind the registry is none to launch from.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(SubagentSpaceError::Resolve {
                        path: self.repos.path(&record.dir),
                        source,
                    });
                }
            }
        }
        if run.launcher != resolve(self.repos.session_root()).await? {
            sources.push(run.launcher.join(WORKING_COPY));
        }
        Ok(sources
            .into_iter()
            .find(|source| caller.starts_with(source)))
    }
}

impl SubagentSpaces for SessionSubagentSpaces {
    type Error = SubagentSpaceError;

    async fn prepare(&self, run: RunSpace<'_>) -> Result<PreparedSpace, Self::Error> {
        let Some(source) = self.launched_from(&run).await? else {
            return Ok(PreparedSpace::new());
        };
        let copy = run.space.join(WORKING_COPY);
        self.repos.child_clone(&source, &copy).await?;
        Ok(PreparedSpace::new().with_working_copy(copy))
    }
}

async fn resolve(path: &Path) -> Result<PathBuf, SubagentSpaceError> {
    async_fs::canonicalize(path)
        .await
        .map_err(|source| SubagentSpaceError::Resolve {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ChatDb;
    use crate::host_git::HostGit;
    use crate::repo_clone::test_sandbox::{arm_hostile_config, markers};

    fn git(dir: &Path, args: &[&str]) -> String {
        HostGit::inherit().output(Some(dir), args).unwrap()
    }

    /// A session with a clone of a checkout holding committed work, an
    /// uncommitted file and a `.gitattributes` the clone's filter names.
    struct Session {
        tmp: PathBuf,
        root: PathBuf,
        clone: PathBuf,
        spaces: SessionSubagentSpaces,
    }

    impl Session {
        async fn new(tmp: &tempfile::TempDir) -> Self {
            let tmp = dunce::canonicalize(tmp.path()).unwrap();
            let checkout = tmp.join("checkout");
            std::fs::create_dir_all(&checkout).unwrap();
            git(&checkout, &["init", "-q", "."]);
            git(&checkout, &["config", "user.email", "t@t"]);
            git(&checkout, &["config", "user.name", "t"]);
            std::fs::write(checkout.join("file.txt"), "hello").unwrap();
            std::fs::write(checkout.join(".gitattributes"), "* filter=hostile\n").unwrap();
            git(&checkout, &["add", "."]);
            git(&checkout, &["commit", "-q", "-m", "init"]);

            let root = tmp.join("session");
            let db = ChatDb::open(&tmp.join("history.db")).await.unwrap();
            let repos = SessionRepos::new(&root, "s1", db);
            let key = dunce::canonicalize(checkout.join(".git")).unwrap();
            let clone = repos
                .ensure_local_clone(&checkout, &key, &tmp.join("staging"))
                .await
                .unwrap();
            let clone = dunce::canonicalize(clone).unwrap();
            git(&clone, &["config", "user.email", "t@t"]);
            git(&clone, &["config", "user.name", "t"]);
            std::fs::write(clone.join("work.txt"), "committed").unwrap();
            git(&clone, &["add", "work.txt"]);
            git(&clone, &["commit", "-q", "-m", "work"]);
            std::fs::write(clone.join("dirty.txt"), "uncommitted").unwrap();
            Self {
                tmp,
                root: dunce::canonicalize(root).unwrap(),
                clone,
                spaces: SessionSubagentSpaces::new(repos),
            }
        }

        /// A fresh space under `launcher`, as aither makes one.
        fn space(launcher: &Path, name: &str) -> PathBuf {
            let space = launcher.join("subagent-spaces").join(name);
            std::fs::create_dir_all(&space).unwrap();
            dunce::canonicalize(space).unwrap()
        }
    }

    /// A run launched from inside a session clone works in a child clone of
    /// the clone's current commit, and making it runs nothing the clone
    /// configures: not its hooks, its filters, its transport or its
    /// pager.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hostile_session_clone_runs_nothing_on_the_host_while_its_child_is_made() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::new(&tmp).await;
        let host = arm_hostile_config(&session.root, &session.clone, &session.tmp);
        let space = Session::space(&session.root, "run-1");
        let caller = session.clone.join("src");
        std::fs::create_dir_all(&caller).unwrap();

        let prepared = session
            .spaces
            .prepare(RunSpace {
                space: &space,
                launcher: &session.root,
                caller: &caller,
            })
            .await
            .unwrap();

        let copy = space.join(WORKING_COPY);
        assert_eq!(prepared.working_copies(), std::slice::from_ref(&copy));
        assert_eq!(markers(&host), Vec::<String>::new(), "ran on the host");
        assert_eq!(markers(&session.root.join("ran")), Vec::<String>::new());
        // The clone's current commit, never its uncommitted work.
        assert_eq!(
            std::fs::read_to_string(copy.join("work.txt")).unwrap(),
            "committed"
        );
        assert!(copy.join("file.txt").exists());
        assert!(!copy.join("dirty.txt").exists());
        // Nothing the clone configured came along, hooks included.
        for key in [
            "core.hooksPath",
            "filter.hostile.smudge",
            "core.sshCommand",
            "core.fsmonitor",
        ] {
            assert!(
                HostGit::inherit()
                    .output(Some(&copy), &["config", "--local", "--get", key])
                    .is_err(),
                "{key} reached the child clone"
            );
        }
        assert!(!copy.join(".git/hooks").exists(), "a hook was installed");
        // The run's base is the parent's commit, and the parent is its
        // origin: the main agent fetches the work back from the child.
        assert_eq!(
            git(&copy, &["rev-parse", "HEAD"]),
            git(&session.clone, &["rev-parse", "HEAD"])
        );
        assert_eq!(
            git(&copy, &["remote", "get-url", "origin"]).trim(),
            session.clone.to_string_lossy()
        );
    }

    /// A run launched from outside every session clone gets an empty space.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_launched_elsewhere_gets_an_empty_space() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::new(&tmp).await;
        let space = Session::space(&session.root, "run-1");
        let elsewhere = session.root.join("tmp");
        std::fs::create_dir_all(&elsewhere).unwrap();

        let prepared = session
            .spaces
            .prepare(RunSpace {
                space: &space,
                launcher: &session.root,
                caller: &elsewhere,
            })
            .await
            .unwrap();

        assert_eq!(prepared.working_copies(), &[] as &[PathBuf]);
        assert_eq!(std::fs::read_dir(&space).unwrap().count(), 0);
    }

    /// A subagent launched from inside its own working copy hands its
    /// subagent a clone of that copy, not of the session clone.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_nested_run_clones_its_launchers_working_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::new(&tmp).await;
        let outer = Session::space(&session.root, "outer");
        session
            .spaces
            .prepare(RunSpace {
                space: &outer,
                launcher: &session.root,
                caller: &session.clone,
            })
            .await
            .unwrap();
        let outer_copy = outer.join(WORKING_COPY);
        std::fs::write(outer_copy.join("outer.txt"), "outer work").unwrap();
        git(&outer_copy, &["config", "user.email", "t@t"]);
        git(&outer_copy, &["config", "user.name", "t"]);
        git(&outer_copy, &["add", "outer.txt"]);
        git(&outer_copy, &["commit", "-q", "-m", "outer"]);

        let inner = Session::space(&outer, "inner");
        session
            .spaces
            .prepare(RunSpace {
                space: &inner,
                launcher: &outer,
                caller: &outer_copy,
            })
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(inner.join(WORKING_COPY).join("outer.txt")).unwrap(),
            "outer work"
        );
        assert_eq!(
            git(&inner.join(WORKING_COPY), &["remote", "get-url", "origin"]).trim(),
            outer_copy.to_string_lossy()
        );
    }

    /// A launch directory that does not resolve fails the launch.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_launch_from_a_missing_directory_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::new(&tmp).await;
        let space = Session::space(&session.root, "run-1");

        let error = session
            .spaces
            .prepare(RunSpace {
                space: &space,
                launcher: &session.root,
                caller: &session.root.join("missing"),
            })
            .await
            .unwrap_err();

        assert!(
            matches!(error, SubagentSpaceError::Resolve { .. }),
            "{error}"
        );
    }
}
