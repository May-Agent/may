//! repo-search: the daemon behind code, session and issue search.
//!
//! May runs repo-search as a host program — a pinned, hash-verified binary
//! the harness executes itself, never placed on a sandbox `PATH`. Every
//! request opens a fresh connection through the platform launcher, which
//! starts or replaces the daemon when needed.

pub mod auto_index;
pub(crate) mod backend;
mod code_search;
pub mod indexer;

pub use backend::{CollectionBackend, CollectionHit, RepoSearchCollections};
pub use code_search::CodeSearchTool;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aither_sandbox::{HostProgram, ToolRuntimeError, ToolTarget};
use async_lock::OnceCell;
use repo_search_client::client::{Client, ClientError};
use repo_search_client::launcher::{self, ConnectError, Launcher as _};
use repo_search_client::protocol::{BuildId, Hello};

#[cfg(target_os = "macos")]
type PlatformLauncher = repo_search_client::launcher::LaunchdAgents<
    repo_search_client::launcher::install::SystemLaunchctl,
>;
#[cfg(not(target_os = "macos"))]
type PlatformLauncher = repo_search_client::launcher::spawn::OnDemand;

/// Why a repo-search request did not get an answer.
#[derive(Debug, thiserror::Error)]
pub enum RepoSearchError {
    /// The pinned binary could not be downloaded, verified or installed.
    #[error("repo-search could not be installed: {0}")]
    Install(#[from] ToolRuntimeError),
    /// Filesystem failure while preparing the binary or its launcher.
    #[error("repo-search could not be prepared: {0}")]
    Io(#[from] std::io::Error),
    /// The launcher failed to make the daemon reachable.
    #[error("repo-search daemon is unavailable: {0}")]
    Launch(String),
    /// The daemon connection or request failed.
    #[error("repo-search request failed: {0}")]
    Client(#[from] ClientError),
}

impl From<ConnectError> for RepoSearchError {
    fn from(error: ConnectError) -> Self {
        match error {
            ConnectError::Launcher(message) => Self::Launch(message),
            ConnectError::Client(error) => Self::Client(error),
        }
    }
}

#[derive(Debug)]
struct Prepared {
    program: PathBuf,
    hello: Hello,
}

#[derive(Debug)]
struct Inner {
    cache_root: PathBuf,
    prepared: OnceCell<Prepared>,
}

/// Shared handle on the repo-search daemon.
///
/// Cheap to clone; the binary is downloaded and verified once per process,
/// on the first request.
#[derive(Debug, Clone)]
pub struct RepoSearch {
    inner: Arc<Inner>,
}

impl RepoSearch {
    /// Handle that caches the pinned binary under `<may_dir>/cache/host-programs`.
    #[must_use]
    pub fn new(may_dir: &Path) -> Self {
        Self {
            inner: Arc::new(Inner {
                cache_root: may_dir.join("cache").join("host-programs"),
                prepared: OnceCell::new(),
            }),
        }
    }

    /// The process-wide handle; `may_dir` is fixed for a process, so the
    /// first caller's value is the one kept.
    #[must_use]
    pub fn global(may_dir: &Path) -> Self {
        static GLOBAL: std::sync::OnceLock<RepoSearch> = std::sync::OnceLock::new();
        GLOBAL.get_or_init(|| Self::new(may_dir)).clone()
    }

    async fn prepared(&self) -> Result<&Prepared, RepoSearchError> {
        self.inner
            .prepared
            .get_or_try_init(|| async {
                let distribution = crate::tools::host_program_distribution()?;
                let path = distribution
                    .prepare_host_program(
                        HostProgram::RepoSearch,
                        &ToolTarget::native()?,
                        &self.inner.cache_root,
                    )
                    .await?;
                let program = path.path().to_path_buf();
                let hello = Hello::new(BuildId::of_file(&program)?);
                Ok::<_, RepoSearchError>(Prepared { program, hello })
            })
            .await
    }

    /// Opens a connection to the daemon, starting or replacing it first when
    /// needed. Launcher notes (model download, daemon start) go to the log.
    ///
    /// # Errors
    /// Fails when the binary cannot be prepared or the daemon stays
    /// unreachable.
    pub async fn client(&self) -> Result<Client, RepoSearchError> {
        let prepared = self.prepared().await?;
        let mut launcher = PlatformLauncher::user(prepared.program.clone())?;
        let report = launcher.prepare().await;
        Ok(launcher::connect(&mut launcher, &prepared.hello, report, log_note).await?)
    }
}

/// Boots out the repo-search launchd agents the client installed for this
/// process's relocated `$HOME`, returning the services it removed.
///
/// A `$HOME` other than the login directory gets agents of its own, labelled
/// with a hash of that home, which nothing else unloads once the home is
/// gone. Under the login directory the labels are the production ones, and
/// those are never touched.
///
/// # Errors
/// Fails when the home or the launchd state cannot be read, or `launchctl`
/// refuses a `bootout`.
#[cfg(target_os = "macos")]
pub fn retire_relocated_agents() -> Result<Vec<String>, RepoSearchError> {
    use repo_search_client::launcher::install::SystemLaunchctl;
    use repo_search_client::launcher::launchd::{domain, user_labels};

    let labels = user_labels()?;
    boot_out_relocated(&mut SystemLaunchctl, &domain(), labels)
}

/// Boots out the loaded agents among `labels` (daemon, server), unless they
/// are the production labels.
#[cfg(target_os = "macos")]
fn boot_out_relocated(
    launchctl: &mut impl repo_search_client::launcher::install::Launchctl,
    domain: &str,
    (daemon, server): (String, String),
) -> Result<Vec<String>, RepoSearchError> {
    use repo_search_client::launcher::launchd::{LABEL, SERVER_LABEL};

    if daemon == LABEL && server == SERVER_LABEL {
        return Ok(Vec::new());
    }
    let mut removed = Vec::new();
    for label in [server, daemon] {
        let service = format!("{domain}/{label}");
        if launchctl
            .service(&service)
            .map_err(|error| RepoSearchError::Launch(error.to_string()))?
            .is_some()
        {
            launchctl
                .bootout(&service)
                .map_err(|error| RepoSearchError::Launch(error.to_string()))?;
            removed.push(service);
        }
    }
    Ok(removed)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::path::Path;

    use repo_search_client::launcher::install::{Launchctl, LaunchctlError, Service};
    use repo_search_client::launcher::launchd::{LABEL, SERVER_LABEL, labels_for};

    use super::boot_out_relocated;

    /// A launchd holding `loaded` services, recording every `bootout`.
    struct Fake {
        loaded: Vec<String>,
        booted_out: Vec<String>,
    }

    impl Launchctl for Fake {
        fn service(&mut self, service: &str) -> Result<Option<Service>, LaunchctlError> {
            Ok(self
                .loaded
                .iter()
                .any(|loaded| loaded == service)
                .then_some(Service { pid: None }))
        }

        fn bootout(&mut self, service: &str) -> Result<(), LaunchctlError> {
            self.booted_out.push(service.to_owned());
            self.loaded.retain(|loaded| loaded != service);
            Ok(())
        }

        fn bootstrap(&mut self, _: &str, _: &Path) -> Result<(), LaunchctlError> {
            unreachable!("retiring never bootstraps")
        }
    }

    fn fake_with(labels: &[&str]) -> Fake {
        Fake {
            loaded: labels
                .iter()
                .map(|label| format!("user/501/{label}"))
                .collect(),
            booted_out: Vec::new(),
        }
    }

    #[test]
    fn a_relocated_home_loses_both_its_agents_and_only_those() {
        let (daemon, server) = labels_for(Path::new("/tmp/home-a"), Path::new("/Users/login"));
        let other = labels_for(Path::new("/tmp/home-b"), Path::new("/Users/login")).0;
        let mut launchctl = fake_with(&[&daemon, &server, &other, LABEL, SERVER_LABEL]);

        let removed = boot_out_relocated(&mut launchctl, "user/501", (daemon, server))
            .expect("boot out should succeed");

        assert_eq!(removed.len(), 2);
        assert_eq!(launchctl.booted_out, removed);
        assert!(launchctl.loaded.contains(&format!("user/501/{other}")));
        assert!(launchctl.loaded.contains(&format!("user/501/{LABEL}")));
        assert!(
            launchctl
                .loaded
                .contains(&format!("user/501/{SERVER_LABEL}"))
        );
    }

    #[test]
    fn an_agent_that_never_loaded_is_not_booted_out() {
        let (daemon, server) = labels_for(Path::new("/tmp/home-a"), Path::new("/Users/login"));
        let mut launchctl = fake_with(&[&daemon]);

        let removed = boot_out_relocated(&mut launchctl, "user/501", (daemon, server))
            .expect("boot out should succeed");

        assert_eq!(removed.len(), 1);
        assert_eq!(launchctl.loaded, Vec::<String>::new());
    }

    #[test]
    fn the_login_homes_production_agents_are_never_booted_out() {
        let labels = labels_for(Path::new("/Users/login"), Path::new("/Users/login"));
        let mut launchctl = fake_with(&[LABEL, SERVER_LABEL]);

        let removed = boot_out_relocated(&mut launchctl, "user/501", labels)
            .expect("boot out should succeed");

        assert_eq!(removed, Vec::<String>::new());
        assert_eq!(launchctl.booted_out, Vec::<String>::new());
    }
}

/// No launchd agents exist off macOS.
#[cfg(not(target_os = "macos"))]
pub fn retire_relocated_agents() -> Result<Vec<String>, RepoSearchError> {
    Ok(Vec::new())
}

async fn log_note(line: &str) {
    tracing::info!(target: "repo_search", "{line}");
}

/// The git worktree root containing `dir`, as repo-search keys its indexes.
///
/// Returns `None` when `dir` is not inside a git worktree.
#[must_use]
pub fn worktree_root(dir: &Path) -> Option<PathBuf> {
    let repo = git2::Repository::discover(dir).ok()?;
    let workdir = repo.workdir()?;
    dunce::canonicalize(workdir).ok()
}
