//! The May daemon: one resident process that runs every agent loop.
//!
//! The daemon owns the sessions' agents, the task scheduler and the search
//! indexes. It serves the REST API and the `/api/events` stream on a unix
//! socket (`~/.may/daemon.sock`, the TUI's transport) and on TCP (the web
//! UI's, plus any `[daemon] listen` addresses such as a tailnet address).
//! Every UI is a client, so any number of them can attach to one session.

pub mod lifecycle;
pub mod serve;

mod acp_agents;
mod artifact_pages;
mod artifacts;
mod attachments;
mod auth;
#[cfg(test)]
mod auth_tests;
mod bootstrap;
mod browser;
mod chat;
mod computer;
mod config;
mod error;
mod error_chain;
mod events;
mod helpers;
mod interaction;
pub use interaction::ASK_USER_NOT_OPEN;
mod local_url_mapper;
mod message_mapping;
mod model_resolution;
mod notifications;
mod profiles;
mod remote_access;
mod server;
#[cfg(test)]
mod session_directory_tests;
mod sessions;
mod state;
mod tasks;
mod terminal_parsing;
mod title_generation;
mod tool_summary;
mod tunnel;
pub(crate) mod types;

#[cfg(test)]
mod funnel_tests;
#[cfg(test)]
mod paths_tests;
#[cfg(test)]
mod repo_clones_tests;
#[cfg(test)]
mod response_tests;
#[cfg(test)]
mod secret_question_tests;
#[cfg(test)]
mod yolo_tests;

pub use chat::{
    BtwMessage, BtwRequest, BtwResponse, CompactResponse, ContextDumpResponse, RecapRecord,
};
pub use server::{DEFAULT_PORT, DaemonMode, parse_listen_args};
pub(crate) use server::{start, sync_remote_access};
pub use types::DaemonHealth;

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

/// How long a shutdown waits for running turns before exiting anyway.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// How long cancelled runs get to settle their interruption before the
/// process exits anyway.
const CANCEL_SETTLE_GRACE: Duration = Duration::from_secs(5);

/// Resolve every pending interaction as the daemon drains: a request whose
/// answer can never arrive must not hold its run until the grace lapses.
/// Permission and workspace requests resolve as denied; ask-user waiters
/// are interrupted outright.
async fn release_pending_interactions(state: &crate::daemon::state::DaemonState) {
    let Ok(sessions) = state.db().list_sessions().await else {
        return;
    };
    let session_ids: Vec<String> = sessions.into_iter().map(|session| session.id).collect();
    let Ok(summaries) = state
        .db()
        .list_session_runtime_summaries(&session_ids)
        .await
    else {
        return;
    };
    for (session_id, summary) in &summaries {
        if let Some(request) = &summary.waiting_permission
            && let Err(error) = state.permission_approver.resolve(&request.id, false).await
        {
            tracing::warn!(%error, "failed to resolve a pending permission during drain");
        }
        if let Some(request) = &summary.waiting_workspace
            && let Err(error) = state.workspace_approver.resolve(&request.id, false).await
        {
            tracing::warn!(%error, "failed to resolve a pending workspace request during drain");
        }
        if let Some(request) = &summary.waiting_ask_user {
            state
                .ask_user_approver
                .interrupt(session_id, &request.id)
                .await;
        }
    }
}

/// Files the daemon keeps under `~/.may`.
#[derive(Debug, Clone)]
pub struct DaemonPaths {
    root: PathBuf,
    socket: PathBuf,
}

impl DaemonPaths {
    /// Paths under `may_dir`.
    #[must_use]
    pub fn new(may_dir: PathBuf) -> Self {
        Self {
            socket: may_dir.join("daemon.sock"),
            root: may_dir,
        }
    }

    /// Paths under the user's `~/.may`.
    ///
    /// # Errors
    /// Fails when the home directory cannot be determined or resolved.
    pub fn user() -> anyhow::Result<Self> {
        Self::for_home(dirs::home_dir().context("no home directory")?)
    }

    /// Paths under `home`'s `.may`. Files live at the resolved home, which
    /// is what the sandbox grants; the socket keeps `home`'s own spelling,
    /// since a unix socket path is limited to about 104 bytes and the
    /// resolved spelling of a home reached through a link can exceed it.
    fn for_home(home: PathBuf) -> anyhow::Result<Self> {
        let socket = home.join(".may").join("daemon.sock");
        let root = MayConfig::resolve_home(home)
            .context("failed to resolve ~/.may")?
            .join(".may");
        Ok(Self { root, socket })
    }

    /// The unix socket clients connect to.
    #[must_use]
    pub fn socket(&self) -> PathBuf {
        self.socket.clone()
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
    pub mode: DaemonMode,
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
                DaemonMode::Ui
            } else {
                DaemonMode::Rest
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

/// Removes the `LanceDB` session index May kept before search moved to
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
                tracing::warn!(path = %path.display(), %error, "failed to remove the legacy session index");
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
    state: Arc<state::DaemonState>,
    local_router: Arc<Router>,
    direct_router: Arc<Router>,
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
            // A non-loopback bind is remote access: every request to it must
            // carry an identity, resolved from the peer address through
            // tailscaled's `WhoIs` — the Tailscale header is only valid
            // through tailscale serve's own proxy on the remote listener.
            // Loopback extras stay local.
            let direct = !addr.ip().is_loopback();
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    tracing::info!(%addr, remote = !addr.ip().is_loopback(), "listening");
                    let task = if direct {
                        tokio::spawn(serve::serve_tcp_tailnet(
                            listener,
                            self.direct_router.clone(),
                            self.state.clone(),
                        ))
                    } else {
                        tokio::spawn(serve::serve_tcp(listener, self.local_router.clone()))
                    };
                    self.running.insert(*addr, task);
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
    // heel executes the sandboxed commands this process runs; configure the
    // managed binary before any spawned task can reach a sandbox.
    crate::configure_heel_binary().await;
    remove_legacy_search_index(paths.root());

    // Agents of relocated homes whose runs were killed before they could
    // retire them, swept before the daemon answers so a client never sees
    // them. A failure leaves other homes' leftovers loaded, which must not
    // stop this daemon, so it is logged like the retire path's.
    match blocking::unblock(crate::repo_search::sweep_if_home_relocated).await {
        Ok(removed) => {
            for service in removed {
                tracing::info!(%service, "booted out a repo-search agent of a deleted relocated home");
            }
        }
        Err(error) => tracing::warn!(%error, "could not sweep stale repo-search agents"),
    }

    let started = crate::daemon::start(options.mode).await?;
    let state = started.state;
    let router = Arc::new(started.router);
    let remote_router = Arc::new(started.remote_router);
    let direct_router = Arc::new(started.direct_router);
    state
        .remote_access_manager
        .set_webhook_router(Arc::new(started.webhook_router));

    let config = state.config_snapshot();
    let tcp = options.tcp.unwrap_or_else(|| {
        SocketAddr::from(([127, 0, 0, 1], config.daemon.port.unwrap_or(DEFAULT_PORT)))
    });
    let tcp_listener = tokio::net::TcpListener::bind(tcp)
        .await
        .with_context(|| format!("failed to listen on {tcp}"))?;
    // Remote access needs identity: it forwards the tailnet to the remote
    // listener, bound on loopback because only the tunnel process connects.
    let remote_listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .context("failed to bind the remote listener")?;
    let unix_listener = bind_unix(&paths.socket())?;
    state.remote_access_manager.set_local_addr(
        remote_listener
            .local_addr()
            .context("the daemon's remote listener has no address")?,
    );
    crate::daemon::sync_remote_access(&state).await?;
    tokio::spawn(serve::serve_tcp(tcp_listener, router.clone()));
    tokio::spawn(serve::serve_unix(unix_listener, router.clone()));
    tokio::spawn(serve::serve_tcp(remote_listener, remote_router.clone()));
    tracing::info!(%tcp, socket = %paths.socket().display(), "May daemon listening");
    eprintln!(
        "May daemon listening on http://{tcp} and {}",
        paths.socket().display()
    );

    let mut extra = ExtraListeners {
        state: state.clone(),
        local_router: router.clone(),
        direct_router: direct_router.clone(),
        running: HashMap::new(),
    };
    extra.reconcile(&config.daemon.listen).await;
    chat::watch_aux_settings(&state);
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
    tokio::spawn(AutoIndexer::new(state.db.clone(), repo_search.clone()).run());

    wait_for_shutdown(&state.daemon_shutdown).await;
    tracing::info!("May daemon shutting down");
    // A pending interaction can never be answered during the drain — release
    // it now so the waiting run settles inside the grace instead of being
    // torn down while it still blocks on the request.
    release_pending_interactions(&state).await;
    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    while state.runtime_service.runs().live_count() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // Runs that outlasted the grace stop with an explicit interruption —
    // never the silent drop of a process kill.
    for session_id in state.runtime_service.runs().live_session_ids() {
        let cancelled = state
            .runtime_service
            .runs()
            .cancel(&session_id, crate::session::CancelReason::DaemonRestarting);
        tracing::info!(session_id = %session_id, cancelled, "interrupted a run outlasting the drain");
    }
    let deadline = tokio::time::Instant::now() + CANCEL_SETTLE_GRACE;
    while state.runtime_service.runs().live_count() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    match repo_search.retire().await {
        Ok(removed) => {
            for service in removed {
                tracing::info!(%service, "booted out a repo-search agent of the relocated home");
            }
        }
        Err(error) => tracing::warn!(%error, "could not boot out the repo-search agents"),
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
