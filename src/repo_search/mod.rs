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
type PlatformLauncher = repo_search_client::launcher::LaunchdAgents;
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
                let binary = distribution
                    .load_host_program(
                        HostProgram::RepoSearch,
                        &ToolTarget::native()?,
                        &self.inner.cache_root.join("artifacts"),
                    )
                    .await?;
                let path = binary
                    .prepare_local(&self.inner.cache_root.join("bin"))
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
        Ok(
            launcher::connect(&mut launcher, &prepared.hello, report, log_note)
                .await?,
        )
    }
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
