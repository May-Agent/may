//! The spaces subagent runs work in: the writable directories its commands
//! use and, from a session clone, a child clone of it.
//!
//! aither gives every subagent run a space of its own and has the terminal's
//! [`SubagentSpaces`] fill it first. Every run gets the temporary, home,
//! cache and config directories of [`WritableDirs`] inside its space, and
//! its commands' environment points at them: the session's own are only
//! readable to a subagent. For a run launched from inside a session clone,
//! May also makes a child clone of that clone in the space, at the clone's
//! current commit, and names it as the run's working copy: the run's work is
//! `git diff base..head` of that copy, and reaches the session clone only
//! when the main agent fetches and merges it. A run launched anywhere else,
//! or from a shell with no session clones, gets the directories alone. Spaces
//! lie in the launcher's working directory, so they leave with the session,
//! and the main agent can remove one once its work is merged.

use std::path::{Path, PathBuf};

use aither_sandbox::{PreparedSpace, RunSpace, SubagentSpaces};

use crate::repo_clone::{RepoCloneError, SessionRepos};
use crate::shell::WritableDirs;

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
    /// A writable directory of the space could not be created.
    #[error("cannot create the run's writable directories: {0}")]
    Directories(#[source] std::io::Error),
    /// The space's git configuration could not be written.
    #[error(transparent)]
    Signing(#[from] crate::commit_signing::SigningError),
}

/// Prepares each subagent run's space of a shell: the writable directories
/// of its commands' environment and, from a session clone, a child clone of
/// it.
#[derive(Debug, Clone)]
pub struct SessionSubagentSpaces {
    /// The session's clones; none for a shell that has none to clone from.
    repos: Option<SessionRepos>,
    /// Whether the session's commands run inside a container: then a caller
    /// working directory is a container path, resolved through the workspace
    /// mount rather than on the host.
    container: bool,
}

impl SessionSubagentSpaces {
    /// Spaces for the session whose clones `repos` manages. The child clones
    /// are made by `repos`' git.
    #[must_use]
    pub const fn new(repos: SessionRepos) -> Self {
        Self {
            repos: Some(repos),
            container: false,
        }
    }

    /// Spaces for a shell with no session clones: each holds the writable
    /// directories alone.
    #[must_use]
    pub const fn without_clones() -> Self {
        Self {
            repos: None,
            container: false,
        }
    }

    /// Resolves caller working directories as container paths: the session
    /// directory is mounted at the container workspace, and a caller outside
    /// it maps to nothing.
    #[must_use]
    pub const fn for_container(mut self) -> Self {
        self.container = true;
        self
    }

    /// The host path `run.caller` names, when it maps to one. A container
    /// caller below the workspace mount maps onto the session root; a
    /// container caller outside it maps to nothing, so the run gets an
    /// empty space rather than an `ENOENT`. A missing directory is `None`
    /// for a container caller and an error for a host one.
    async fn host_caller(
        &self,
        repos: &SessionRepos,
        caller: &Path,
    ) -> Result<Option<PathBuf>, SubagentSpaceError> {
        match resolve(caller).await {
            Ok(path) => return Ok(Some(path)),
            Err(error) if !self.container => return Err(error),
            Err(_) => {}
        }
        let Ok(relative) = caller.strip_prefix(crate::container::lifecycle::CONTAINER_WORKSPACE)
        else {
            return Ok(None);
        };
        Ok(resolve(&repos.session_root().join(relative)).await.ok())
    }

    /// The clone `run` was launched from inside, by its canonical path: a
    /// clone `repos` registered, or the working copy of the launcher's own
    /// space when the launcher is a subagent.
    async fn launched_from(
        &self,
        repos: &SessionRepos,
        run: &RunSpace<'_>,
    ) -> Result<Option<PathBuf>, SubagentSpaceError> {
        let Some(caller) = self.host_caller(repos, run.caller).await? else {
            return Ok(None);
        };
        let mut sources = Vec::new();
        for record in repos.clones().await? {
            match async_fs::canonicalize(repos.path(&record.dir)).await {
                Ok(path) => sources.push(path),
                // A clone removed behind the registry is none to launch from.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(SubagentSpaceError::Resolve {
                        path: repos.path(&record.dir),
                        source,
                    });
                }
            }
        }
        if run.launcher != resolve(repos.session_root()).await? {
            sources.push(run.launcher.join(WORKING_COPY));
        }
        // A session bound to a local checkout it writes, or whose clone is
        // not made yet, still gives the run an isolated clone of that
        // checkout rather than letting it write the user's checkout.
        if let Some(checkout) = repos.bound_checkout(&caller).await? {
            sources.push(checkout);
        }
        Ok(sources
            .into_iter()
            .find(|source| caller.starts_with(source)))
    }
}

impl SubagentSpaces for SessionSubagentSpaces {
    type Error = SubagentSpaceError;

    async fn prepare(&self, run: RunSpace<'_>) -> Result<PreparedSpace, Self::Error> {
        let writable = WritableDirs::under(run.space);
        writable
            .create()
            .await
            .map_err(SubagentSpaceError::Directories)?;
        let prepared = writable
            .variables()
            .into_iter()
            .fold(PreparedSpace::new(), |prepared, (name, directory)| {
                prepared.with_env_dir(name, directory)
            });
        // Each subagent keeps its own `tasks.md` in its space rather than
        // inheriting the main session's through `MAY_TASKS`; the file is
        // created so tools resolving the env path find it there.
        let tasks_md = run.space.join(crate::tasks_md::FILE);
        if !crate::harness::fs_util::path_exists(&tasks_md)
            .await
            .unwrap_or(false)
        {
            async_fs::write(&tasks_md, "")
                .await
                .map_err(SubagentSpaceError::Directories)?;
        }
        let prepared = prepared.with_env_dir(crate::tasks_md::ENV, tasks_md);
        // A subagent's `HOME` is its space: the sandbox `.gitconfig` the
        // launcher's space carries (signing program, identity) is copied so
        // the run's commits look like the session's own.
        let launcher_gitconfig = run.launcher.join(".gitconfig");
        if crate::harness::fs_util::path_exists(&launcher_gitconfig)
            .await
            .unwrap_or(false)
        {
            async_fs::copy(&launcher_gitconfig, run.space.join(".gitconfig"))
                .await
                .map_err(SubagentSpaceError::Directories)?;
        }
        let Some(repos) = &self.repos else {
            return Ok(prepared);
        };
        let Some(source) = self.launched_from(repos, &run).await? else {
            return Ok(prepared);
        };
        let copy = run.space.join(WORKING_COPY);
        repos.child_clone(&source, &copy).await?;
        // A run with a working copy may commit in it, so its `HOME` gets
        // the session's git configuration: its commits are signed through
        // the same harness, and `sign-commit` lets its space sign — the
        // grant is host-held memory, not a file the run could write.
        let signing = repos.commit_signing();
        signing.install_sandbox_git(run.space, None).await?;
        signing.signing_roots().grant(run.space);
        Ok(prepared.with_working_copy(copy))
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
    use crate::repo_clone::CloneSandbox;
    use crate::repo_clone::test_sandbox::{TestTerminal, arm_hostile_config, markers};

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

        assert_eq!(prepared, expected_env(&space));
        assert!(!space.join(WORKING_COPY).exists());
    }

    /// What every prepared space records: the layout of [`WritableDirs`],
    /// spelled out, plus the run's own `tasks.md` named by `MAY_TASKS`.
    fn expected_env(space: &Path) -> PreparedSpace {
        let tmp = space.join("tmp");
        let cache = space.join(".cache");
        PreparedSpace::new()
            .with_env_dir("TMPDIR", &tmp)
            .with_env_dir("TMP", &tmp)
            .with_env_dir("TEMP", &tmp)
            .with_env_dir("HOME", space)
            .with_env_dir("XDG_CACHE_HOME", &cache)
            .with_env_dir("XDG_CONFIG_HOME", space.join(".config"))
            .with_env_dir("BUN_INSTALL_CACHE_DIR", cache.join("bun"))
            .with_env_dir(crate::tasks_md::ENV, space.join(crate::tasks_md::FILE))
    }

    fn assert_env_dirs_exist(space: &Path) {
        for directory in ["tmp", ".cache", ".cache/bun", ".config"] {
            assert!(space.join(directory).is_dir(), "{directory} is not made");
        }
    }

    /// A run with a child clone also gets the writable directories, each
    /// recorded inside the space and made.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_with_a_clone_records_its_writable_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::new(&tmp).await;
        let space = Session::space(&session.root, "run-1");

        let prepared = session
            .spaces
            .prepare(RunSpace {
                space: &space,
                launcher: &session.root,
                caller: &session.clone,
            })
            .await
            .unwrap();

        assert_eq!(
            prepared,
            expected_env(&space).with_working_copy(space.join(WORKING_COPY))
        );
        assert_env_dirs_exist(&space);
    }

    /// A shell with no session clones still gives its runs the writable
    /// directories, and no working copy.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_without_clones_records_its_writable_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let space = Session::space(&root, "run-1");

        let prepared = SessionSubagentSpaces::without_clones()
            .prepare(RunSpace {
                space: &space,
                launcher: &root,
                caller: &root,
            })
            .await
            .unwrap();

        assert_eq!(prepared, expected_env(&space));
        assert_env_dirs_exist(&space);
        assert!(!space.join(WORKING_COPY).exists());
    }

    /// A native subagent's command writes a temporary file, its home and its
    /// cache directory, all inside its space.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_subagents_command_writes_temp_home_and_cache_inside_its_space() {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let terminal = TestTerminal::new(&root).await;
        let child = terminal
            .shell()
            .subagent(aither_sandbox::SubagentGrant::new(), None, "worker")
            .await
            .expect("the space is prepared");
        let space = child.working_dir().clone();

        let printed = CloneSandbox::new(child, std::time::Duration::from_secs(60))
            .run(
                &space,
                "file=$(mktemp) && touch \"$file\" \"$HOME/home-file\" \
                 \"$XDG_CACHE_HOME/cache-file\" && printf '%s' \"$file\"",
            )
            .await
            .expect("mktemp and both writes succeed");

        let made = PathBuf::from(printed.trim());
        assert!(made.starts_with(&space), "{made:?} is outside {space:?}");
        assert!(made.exists());
        assert!(space.join("home-file").exists());
        assert!(space.join(".cache/cache-file").exists());
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
