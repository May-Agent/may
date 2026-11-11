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

use crate::tools::{HostProgram, ToolRuntimeError, ToolTarget};
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
    /// The May daemon is shutting down and has retired its repo-search agents.
    #[error("repo-search is retired: the May daemon is shutting down")]
    Retired,
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
    /// `true` once [`RepoSearch::retire`] ran. Every launch holds it shared
    /// while it may install agents, so a retirement waits for those in flight
    /// and no launch installs after it. The binary is prepared before the
    /// lock is taken, so a retirement never waits on the network.
    retired: async_lock::RwLock<bool>,
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
                retired: async_lock::RwLock::new(false),
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
                let distribution = crate::tools::embedded_distribution()?;
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
    /// unreachable, and with [`RepoSearchError::Retired`] after
    /// [`RepoSearch::retire`].
    pub async fn client(&self) -> Result<Client, RepoSearchError> {
        // Preparing the binary installs no agents, so it runs outside the
        // lock: a retirement must not wait out a download's retries.
        let prepared = self.prepared().await?;
        let retired = self.inner.retired.read().await;
        if *retired {
            return Err(RepoSearchError::Retired);
        }
        let mut launcher = PlatformLauncher::user(prepared.program.clone())?;
        let report = launcher.prepare().await;
        let client = launcher::connect(&mut launcher, &prepared.hello, report, log_note).await?;
        // Held until here: the launch above is what installs agents.
        drop(retired);
        Ok(client)
    }

    /// Boots out the launchd agents this process installed for a relocated
    /// `$HOME` (see [`retire_relocated_agents`]) once every launch in flight
    /// has finished, and refuses every later one, so none reinstalls them.
    /// Returns the services it removed.
    ///
    /// # Errors
    /// As [`retire_relocated_agents`].
    pub async fn retire(&self) -> Result<Vec<String>, RepoSearchError> {
        // The write lock waits out every launch in flight; once the flag is
        // set, no later one gets past its check.
        *self.inner.retired.write().await = true;
        blocking::unblock(retire_relocated_agents).await
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
fn retire_relocated_agents() -> Result<Vec<String>, RepoSearchError> {
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
    let launch = |error: repo_search_client::launcher::install::LaunchctlError| {
        RepoSearchError::Launch(error.to_string())
    };
    let mut removed = Vec::new();
    for label in [server, daemon] {
        let service = format!("{domain}/{label}");
        if launchctl.service(&service).map_err(launch)?.is_some()
            && boot_out_loaded(launchctl, &service).map_err(launch)?
        {
            removed.push(service);
        }
    }
    await_bootouts(launchctl, &removed)?;
    Ok(removed)
}

/// Blocks until every service in `booted_out` has left launchd.
///
/// `bootout` returns once launchd has signalled the job, not once the process
/// exited: a server stopping mid-load stays registered, in state `SIGTERMed`,
/// until it exits or launchd's exit timeout kills it.
///
/// # Errors
/// Fails when the launchd state cannot be read, or a service is still
/// registered [`BOOTOUT_SETTLE`] after its bootout.
#[cfg(target_os = "macos")]
fn await_bootouts(
    launchctl: &mut impl repo_search_client::launcher::install::Launchctl,
    booted_out: &[String],
) -> Result<(), RepoSearchError> {
    let deadline = std::time::Instant::now() + BOOTOUT_SETTLE;
    for service in booted_out {
        while launchctl
            .service(service)
            .map_err(|error| RepoSearchError::Launch(error.to_string()))?
            .is_some()
        {
            if std::time::Instant::now() >= deadline {
                return Err(RepoSearchError::Launch(format!(
                    "{service} is still registered {BOOTOUT_SETTLE:?} after its bootout"
                )));
            }
            std::thread::sleep(BOOTOUT_POLL);
        }
    }
    Ok(())
}

/// How long a booted-out agent may stay registered while its process stops:
/// twice the 5 s exit timeout launchd applies before it kills the process.
#[cfg(target_os = "macos")]
const BOOTOUT_SETTLE: std::time::Duration = std::time::Duration::from_secs(10);

/// The interval at which a booted-out agent's registration is rechecked.
#[cfg(target_os = "macos")]
const BOOTOUT_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Boots out every relocated-home repo-search agent whose plist no longer
/// exists, returning the services it removed.
///
/// A run killed before its daemon stopped leaves its relocated home's agents
/// loaded after the home is deleted. Production agents are never touched.
///
/// # Errors
/// Fails when the launchd state cannot be read, or `launchctl` refuses a
/// `bootout`.
#[cfg(target_os = "macos")]
pub fn sweep_stale_relocated_agents() -> Result<Vec<String>, RepoSearchError> {
    use repo_search_client::launcher::install::SystemLaunchctl;
    use repo_search_client::launcher::launchd::domain;

    sweep_stale(&mut SystemLaunchctl, &domain())
}

/// [`sweep_stale_relocated_agents`] when this process runs under a relocated
/// `$HOME`; under the login directory nothing is swept.
///
/// # Errors
/// As [`sweep_stale_relocated_agents`], or when the home cannot be read.
#[cfg(target_os = "macos")]
pub fn sweep_if_home_relocated() -> Result<Vec<String>, RepoSearchError> {
    use repo_search_client::launcher::launchd::{LABEL, user_labels};

    if user_labels()?.0 == LABEL {
        return Ok(Vec::new());
    }
    sweep_stale_relocated_agents()
}

/// No launchd agents exist off macOS.
///
/// # Errors
/// Never; the signature is the macOS variant's.
#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)] // the macOS variant's signature
pub const fn sweep_if_home_relocated() -> Result<Vec<String>, RepoSearchError> {
    Ok(Vec::new())
}

/// No launchd agents exist off macOS.
///
/// # Errors
/// Never; the signature is the macOS variant's.
#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)] // the macOS variant's signature
pub const fn sweep_stale_relocated_agents() -> Result<Vec<String>, RepoSearchError> {
    Ok(Vec::new())
}

/// Whether `label` is a relocated home's repo-search agent: the production
/// daemon or server label followed by `.h-` and the home's hash.
#[cfg(target_os = "macos")]
fn is_relocated_label(label: &str) -> bool {
    use repo_search_client::launcher::launchd::{LABEL, SERVER_LABEL};

    [LABEL, SERVER_LABEL].iter().any(|production| {
        label
            .strip_prefix(production)
            .is_some_and(|rest| rest.starts_with(".h-"))
    })
}

/// Boots out the relocated-home agents loaded in `domain` whose plist is gone.
///
/// An agent launchd reports no plist for is kept: nothing proves its home
/// gone, and a live home's agent must never be removed.
#[cfg(target_os = "macos")]
fn sweep_stale(
    launchctl: &mut impl repo_search_client::launcher::install::Launchctl,
    domain: &str,
) -> Result<Vec<String>, RepoSearchError> {
    let launch = |error: repo_search_client::launcher::install::LaunchctlError| {
        RepoSearchError::Launch(error.to_string())
    };
    let mut removed = Vec::new();
    for label in launchctl.labels(domain).map_err(launch)? {
        if !is_relocated_label(&label) {
            continue;
        }
        let service = format!("{domain}/{label}");
        let Some(plist) = launchctl
            .service(&service)
            .map_err(launch)?
            .and_then(|loaded| loaded.path)
        else {
            continue;
        };
        if !plist.try_exists()? && boot_out_loaded(launchctl, &service).map_err(launch)? {
            removed.push(service);
        }
    }
    await_bootouts(launchctl, &removed)?;
    Ok(removed)
}

/// Boots out `service`, returning whether this call removed it.
///
/// Concurrent sweeps (every daemon and client start runs one) find the same
/// stale service; the loser's `bootout` fails because the winner's already
/// took it out. A refused `bootout` of a service launchd still holds stays an
/// error.
///
/// # Errors
/// The `bootout` error, when the service is still loaded after it.
#[cfg(target_os = "macos")]
pub(crate) fn boot_out_loaded(
    launchctl: &mut impl repo_search_client::launcher::install::Launchctl,
    service: &str,
) -> Result<bool, repo_search_client::launcher::install::LaunchctlError> {
    match launchctl.bootout(service) {
        Ok(()) => Ok(true),
        Err(error)
            if launchctl
                .service(service)
                .is_ok_and(|loaded| loaded.is_none()) =>
        {
            tracing::debug!(%service, %error, "another sweep booted the service out first");
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::path::Path;

    use repo_search_client::launcher::install::{Launchctl, LaunchctlError, Service};
    use repo_search_client::launcher::launchd::{LABEL, SERVER_LABEL, labels_for};

    use super::{boot_out_relocated, sweep_stale};

    /// A launchd holding `loaded` services, recording every `bootout`.
    /// `plists` gives the plist launchd reports for a service, by label.
    /// A booted-out service stays registered for `linger` more lookups, as a
    /// process still stopping does.
    struct Fake {
        loaded: Vec<String>,
        booted_out: Vec<String>,
        plists: Vec<(String, std::path::PathBuf)>,
        linger: usize,
        stopping: Vec<(String, usize)>,
    }

    impl Launchctl for Fake {
        fn service(&mut self, service: &str) -> Result<Option<Service>, LaunchctlError> {
            if let Some(index) = self.stopping.iter().position(|(name, _)| name == service) {
                let left = &mut self.stopping[index].1;
                if *left > 0 {
                    *left -= 1;
                    return Ok(Some(Service {
                        pid: None,
                        path: None,
                    }));
                }
                self.stopping.remove(index);
            }
            Ok(self
                .loaded
                .iter()
                .any(|loaded| loaded == service)
                .then(|| Service {
                    pid: None,
                    path: self
                        .plists
                        .iter()
                        .find(|(label, _)| service.ends_with(label.as_str()))
                        .map(|(_, path)| path.clone()),
                }))
        }

        fn labels(&mut self, domain: &str) -> Result<Vec<String>, LaunchctlError> {
            let prefix = format!("{domain}/");
            Ok(self
                .loaded
                .iter()
                .filter_map(|service| service.strip_prefix(&prefix).map(str::to_owned))
                .collect())
        }

        fn bootout(&mut self, service: &str) -> Result<(), LaunchctlError> {
            self.booted_out.push(service.to_owned());
            self.loaded.retain(|loaded| loaded != service);
            if self.linger > 0 {
                self.stopping.push((service.to_owned(), self.linger));
            }
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
            plists: Vec::new(),
            linger: 0,
            stopping: Vec::new(),
        }
    }

    #[test]
    fn the_sweep_returns_once_its_swept_agents_have_left_launchd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let login = Path::new("/Users/login");
        let (stale, stale_server) = labels_for(Path::new("/tmp/gone-home"), login);
        let mut launchctl = fake_with(&[&stale, &stale_server]);
        let gone = dir.path().join("gone.plist");
        launchctl.plists = vec![(stale, gone.clone()), (stale_server, gone)];
        launchctl.linger = 3;

        let removed = sweep_stale(&mut launchctl, "user/501").expect("sweep should succeed");

        assert_eq!(removed.len(), 2);
        for service in &removed {
            assert!(
                launchctl.service(service).expect("lookup").is_none(),
                "{service} should have left launchd when the sweep returned"
            );
        }
    }

    #[test]
    fn the_sweep_boots_out_only_relocated_agents_whose_plist_is_gone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live_plist = dir.path().join("live.plist");
        std::fs::write(&live_plist, "").expect("plist written");
        let login = Path::new("/Users/login");
        let (stale, stale_server) = labels_for(Path::new("/tmp/gone-home"), login);
        let (live, _) = labels_for(Path::new("/tmp/live-home"), login);
        let (unknown, _) = labels_for(Path::new("/tmp/unknown-home"), login);
        let mut launchctl = fake_with(&[
            &stale,
            &stale_server,
            &live,
            &unknown,
            LABEL,
            SERVER_LABEL,
            "com.example.other",
        ]);
        let gone = dir.path().join("gone.plist");
        launchctl.plists = vec![
            (stale.clone(), gone.clone()),
            (stale_server.clone(), gone),
            (live.clone(), live_plist),
            // Production plists are absent too: still never touched.
            (LABEL.to_owned(), dir.path().join("absent.plist")),
            (SERVER_LABEL.to_owned(), dir.path().join("absent.plist")),
        ];

        let removed = sweep_stale(&mut launchctl, "user/501").expect("sweep should succeed");

        assert_eq!(removed.len(), 2);
        assert_eq!(launchctl.booted_out, removed);
        assert!(removed.contains(&format!("user/501/{stale}")));
        assert!(removed.contains(&format!("user/501/{stale_server}")));
        for kept in [live.as_str(), unknown.as_str(), LABEL, SERVER_LABEL] {
            assert!(
                launchctl.loaded.contains(&format!("user/501/{kept}")),
                "{kept} should stay loaded"
            );
        }
        assert!(
            launchctl
                .loaded
                .contains(&"user/501/com.example.other".to_owned())
        );
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
#[allow(clippy::unnecessary_wraps)] // the macOS variant's signature
const fn retire_relocated_agents() -> Result<Vec<String>, RepoSearchError> {
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
