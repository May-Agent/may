//! The May daemon: one resident process that runs every agent loop.
//!
//! The daemon owns the sessions' agents, the task scheduler and the search
//! indexes. It serves the REST API and the `/api/events` stream on a unix
//! socket (`~/.may/daemon.sock`, the TUI's transport) and on TCP (the web
//! UI's, plus any `[daemon] listen` addresses such as a tailnet address).
//! Every UI is a client, so any number of them can attach to one session.

pub mod lifecycle;
pub mod serve;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use skyzen::routing::Router;

use crate::config::MayConfig;
use crate::repo_search::auto_index::AutoIndexer;
use crate::repo_search::indexer::{IndexerStatus, SearchIndexer};
use crate::repo_search::{RepoSearch, RepoSearchCollections};
use crate::ui_event_bus::UiEventTopics;
use crate::ui_server::{DEFAULT_PORT, UiServerMode};

/// How long a shutdown waits for running turns before exiting anyway.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Files the daemon keeps under `~/.may`.
#[derive(Debug, Clone)]
pub struct DaemonPaths {
    root: PathBuf,
}

impl DaemonPaths {
    /// Paths under `may_dir`.
    #[must_use]
    pub const fn new(may_dir: PathBuf) -> Self {
        Self { root: may_dir }
    }

    /// Paths under the user's `~/.may`.
    ///
    /// # Errors
    /// Fails when the home directory cannot be determined.
    pub fn user() -> anyhow::Result<Self> {
        Ok(Self::new(
            MayConfig::may_dir().context("failed to resolve ~/.may")?,
        ))
    }

    /// The unix socket clients connect to.
    #[must_use]
    pub fn socket(&self) -> PathBuf {
        self.root.join("daemon.sock")
    }

    /// The single-instance lock.
    #[must_use]
    pub fn lock(&self) -> PathBuf {
        self.root.join("daemon.lock")
    }

    /// The log a spawned daemon writes to.
    #[must_use]
    pub fn log(&self) -> PathBuf {
        self.root.join("logs").join("daemon.log")
    }

    /// The `~/.may` directory itself.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Identity of an executable build: device, inode, size and mtime of the
/// file. A client whose own build differs from the running daemon's
/// replaces the daemon, so a rebuilt `may` never talks to a stale one.
///
/// # Errors
/// Fails when the file's metadata cannot be read.
pub fn build_id_of(path: &Path) -> std::io::Result<String> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::metadata(path)?;
    Ok(format!(
        "{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.size(),
        metadata.mtime_nsec() + metadata.mtime() * 1_000_000_000
    ))
}

/// Build id of the running executable.
///
/// # Errors
/// Fails when the executable path or its metadata cannot be read.
pub fn current_build_id() -> std::io::Result<String> {
    build_id_of(&dunce::canonicalize(std::env::current_exe()?)?)
}

/// How a daemon run is configured.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// Whether the embedded web UI is served next to the API.
    pub mode: UiServerMode,
    /// TCP address replacing `127.0.0.1:<[daemon] port>`.
    pub tcp: Option<SocketAddr>,
}

impl DaemonOptions {
    /// Default options: the web UI when it is compiled in, on the
    /// configured port.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mode: if cfg!(feature = "ui") {
                UiServerMode::Ui
            } else {
                UiServerMode::Rest
            },
            tcp: None,
        }
    }
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self::new()
    }
}

struct SingleInstance {
    _lock: fd_lock::RwLock<std::fs::File>,
}

fn acquire_single_instance(paths: &DaemonPaths) -> anyhow::Result<SingleInstance> {
    use std::io::Write as _;

    std::fs::create_dir_all(paths.root())?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.lock())?;
    let mut lock = fd_lock::RwLock::new(file);
    let mut guard = match lock.try_write() {
        Ok(guard) => guard,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            let holder = std::fs::read_to_string(paths.lock()).unwrap_or_default();
            anyhow::bail!("a May daemon is already running ({})", holder.trim());
        }
        Err(error) => return Err(error.into()),
    };
    guard.set_len(0)?;
    write!(guard, "pid {}", std::process::id())?;
    // The flock lives on the open file description; forgetting the guard
    // keeps it held while `lock` stays open.
    std::mem::forget(guard);
    Ok(SingleInstance { _lock: lock })
}

/// Removes the LanceDB session index May kept before search moved to
/// repo-search.
fn remove_legacy_search_index(may_dir: &Path) {
    for name in [
        "session_index.lancedb",
        "session_index.meta.json",
        "session_index.semantic.redb",
    ] {
        let path = may_dir.join(name);
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match removed {
            Ok(()) => tracing::info!(path = %path.display(), "removed the legacy session index"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "failed to remove the legacy session index")
            }
        }
    }
}

fn bind_unix(path: &Path) -> anyhow::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt as _;

    // The single-instance lock is held, so any socket file left behind
    // belongs to a daemon that is gone.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to remove the stale daemon socket"),
    }
    let listener = tokio::net::UnixListener::bind(path)
        .with_context(|| format!("failed to bind {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// The extra `[daemon] listen` listeners, rebound whenever the config changes.
struct ExtraListeners {
    router: Arc<Router>,
    running: HashMap<SocketAddr, tokio::task::JoinHandle<()>>,
}

impl ExtraListeners {
    async fn reconcile(&mut self, wanted: &[SocketAddr]) {
        self.running.retain(|addr, task| {
            let keep = wanted.contains(addr);
            if !keep {
                task.abort();
                tracing::info!(%addr, "stopped listening");
            }
            keep
        });
        for addr in wanted {
            if self.running.contains_key(addr) {
                continue;
            }
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    tracing::info!(%addr, "listening");
                    self.running.insert(
                        *addr,
                        tokio::spawn(serve::serve_tcp(listener, self.router.clone())),
                    );
                }
                Err(error) => tracing::warn!(%addr, %error, "failed to listen"),
            }
        }
    }
}

/// Runs the daemon until it is asked to stop (`SIGINT`, `SIGTERM` or
/// `POST /api/daemon/shutdown`).
///
/// # Errors
/// Fails when another daemon holds the lock, or when the server cannot be
/// started or its socket bound.
pub async fn run_daemon(options: DaemonOptions) -> anyhow::Result<()> {
    let paths = DaemonPaths::user()?;
    let _instance = acquire_single_instance(&paths)?;
    remove_legacy_search_index(paths.root());

    let started = crate::ui_server::start(options.mode).await?;
    let state = started.state;
    let router = Arc::new(started.router);

    let config = state.config_snapshot();
    let tcp = options.tcp.unwrap_or_else(|| {
        SocketAddr::from(([127, 0, 0, 1], config.daemon.port.unwrap_or(DEFAULT_PORT)))
    });
    let tcp_listener = tokio::net::TcpListener::bind(tcp)
        .await
        .with_context(|| format!("failed to listen on {tcp}"))?;
    let unix_listener = bind_unix(&paths.socket())?;
    tokio::spawn(serve::serve_tcp(tcp_listener, router.clone()));
    tokio::spawn(serve::serve_unix(unix_listener, router.clone()));
    tracing::info!(%tcp, socket = %paths.socket().display(), "May daemon listening");
    eprintln!(
        "May daemon listening on http://{tcp} and {}",
        paths.socket().display()
    );

    let mut extra = ExtraListeners {
        router: router.clone(),
        running: HashMap::new(),
    };
    extra.reconcile(&config.daemon.listen).await;
    let config_state = state.clone();
    let mut config_events = state.ui_event_bus.subscribe();
    tokio::spawn(async move {
        loop {
            match config_events.recv().await {
                Ok(topics) if topics.contains(UiEventTopics::CONFIG) => {
                    extra
                        .reconcile(&config_state.config_snapshot().daemon.listen)
                        .await;
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let repo_search = RepoSearch::global(paths.root());
    let indexer = SearchIndexer::new(
        state.db.clone(),
        RepoSearchCollections::new(repo_search.clone()),
    );
    tokio::spawn(indexer.run(
        state.search_index_wake.clone(),
        state.search_index_status.clone(),
    ));
    tokio::spawn(AutoIndexer::new(state.db.clone(), repo_search).run());

    wait_for_shutdown(&state.daemon_shutdown).await;
    tracing::info!("May daemon shutting down");
    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    while state.runtime_service.runs().live_count() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = std::fs::remove_file(paths.socket());
    Ok(())
}

async fn wait_for_shutdown(requested: &tokio::sync::Notify) {
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = async {
            match terminate.as_mut() {
                Some(signal) => { signal.recv().await; }
                None => std::future::pending::<()>().await,
            }
        } => {}
        () = requested.notified() => {}
    }
}

/// The search-index state the daemon reports, for `may daemon status`.
#[must_use]
pub fn indexer_status_snapshot(
    status: &IndexerStatus,
) -> crate::repo_search::indexer::IndexerSnapshot {
    status.snapshot()
}
