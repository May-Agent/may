//! The May daemon: one resident process that runs every agent loop.
//!
//! The daemon owns the sessions' agents, the task scheduler and the search
//! indexes. It serves the REST API and the `/api/events` stream on a unix
//! socket (`~/.may/daemon.sock`, the TUI's transport); TCP arrives only when
//! asked — the `[daemon] port` local listener a `gui`/`rest` run pins, the
//! `[daemon] listen` addresses such as a tailnet address, and the explicit
//! address of a foreground `may rest`. Every UI is a client, so any number
//! of them can attach to one session.
#![recursion_limit = "512"]

pub use may_client::daemon::lifecycle;
pub mod serve;

mod acp_agents;
mod archive;
mod artifact_pages;
mod artifacts;
mod attachments;
mod auth;
#[cfg(test)]
mod auth_tests;
mod auto_update;
mod bootstrap;
mod browser;
mod chat;
mod cloud_home;
mod cloud_sessions;
/// The session's virtual-desktop agent (renamed off `computer`, which the
/// agent-core re-exports occupy).
#[path = "computer.rs"]
mod daemon_computer;
/// The daemon's `/api/config` handlers (renamed off `config`, which the
/// agent-core re-exports occupy).
#[path = "config.rs"]
mod daemon_config;
pub mod disk_exhaustion;
mod error;
mod error_chain;
mod events;
/// Forge-authentication routes and their wire types.
///
/// Exposed publicly so the CLI and TUI share the daemon's request and
/// response shapes.
pub mod forge_auth;
#[cfg(test)]
mod forge_auth_tests;
mod github_repos;
#[cfg(test)]
mod github_repos_tests;
mod helpers;
mod interaction;
pub use may_protocol::daemon_api::ASK_USER_NOT_OPEN;
#[cfg(test)]
mod attachments_tests;
/// The `/api/rules/import` route (renamed off `rules_import`, which the
/// agent-core re-exports occupy).
#[path = "rules_import.rs"]
mod daemon_rules_import;
/// The session sleep/wake registry (renamed off `sleep`, which the
/// agent-core re-exports occupy).
#[path = "sleep.rs"]
mod daemon_sleep;
mod local_url_mapper;
mod message_mapping;
mod model_resolution;
mod notifications;
mod pr_checks;
mod process_session;
mod profiles;
mod prs;
mod remote_access;
mod server;
#[cfg(test)]
mod session_commands_tests;
#[cfg(test)]
mod session_directory_tests;
pub mod session_events;
#[cfg(test)]
mod session_events_tests;
mod session_ids;
mod sessions;
mod state;
mod tasks;
mod terminal_parsing;
mod title_generation;
pub use may_client::daemon::tool_summary;
/// Scheduled task execution.
pub mod scheduler;
/// Tool request dispatch to session handlers.
pub mod tool_dispatch;
mod transcript;
#[cfg(test)]
mod transcript_tests;
mod tunnel;
pub use may_client::daemon::types;
mod user_wait;

#[cfg(test)]
mod compression_tests;
#[cfg(test)]
mod funnel_tests;
#[cfg(test)]
mod issue_brief_tests;
#[cfg(test)]
mod memory_off_tests;
#[cfg(test)]
mod paths_tests;
#[cfg(test)]
mod repo_clones_tests;
#[cfg(test)]
mod response_tests;
#[cfg(test)]
mod secret_question_tests;
#[cfg(test)]
mod user_wait_tests;
#[cfg(test)]
mod yolo_tests;

pub use chat::{
    BtwMessage, BtwRequest, BtwResponse, CompactResponse, ContextDumpResponse, ProgressReport,
    RecapRecord,
};
pub use may_protocol::daemon_api::DEFAULT_PORT;
pub use server::DaemonMode;
pub use server::{start, sync_remote_access};
pub use types::{DaemonHealth, LocalListenerResponse};

// `crate::x` paths inside this crate were written against `may-agent`'s root;
// these aliases keep them resolving to the agent-core modules that now hold
// that code. The daemon's own same-named modules keep distinct idents
// (`daemon_config`, `daemon_sleep`, `daemon_computer`, `daemon_rules_import`).
use may_agent_core::{
    ChatDb, MayAgent, ModelSelector, ModelSelectorParseError, PermissionHistoryMode, SkillManager,
    XmlBlock, XmlBlockError,
};
use may_agent_core::{
    agent, agent_link, ask_user, bash, browser_service, cache_lifetime, client, cloud,
    command_output, command_policy, command_reading, commit_signing, computer, config, container,
    copilot, db, decode_meter, forge, grok, harness, host_git, id, ipc, issue, jobserver,
    live_response, location, mcp, memory, model_selector, models, network_policy, notification,
    panic_boundary, pr, profile, prompt_mentions, quota, repo_clone, repo_gate, repo_search,
    rules_import, secrets, session, shell, skill, sleep, task_rows, task_trigger, task_wake,
    tasks_md, terminal_commands, tools, trust, ui_event_bus, update, vision_ocr, workspace,
    workspace_request,
};

#[cfg(test)]
use may_agent_core::{
    PermissionRequestMode, WorkspaceConfig, terminal_permission_handler, test_sandbox,
};
#[cfg(test)]
mod test_support {
    #[allow(unused_imports)]
    pub use may_protocol::test_support::{
        ApiReply, ApiRequest, ApiRequests, install_global_executor, serve_api,
    };
}

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use skyzen::routing::Router;

use crate::repo_search::RepoSearchCollections;
use crate::repo_search::auto_index::AutoIndexer;
use crate::repo_search::indexer::{IndexerStatus, SearchIndexer};
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
async fn release_pending_interactions(state: &crate::state::DaemonState) {
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

pub use may_client::daemon::{DaemonPaths, build_id_of, current_build_id};

/// How a daemon run is configured.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// Whether the embedded web UI is served next to the API.
    pub mode: DaemonMode,
    /// TCP address this run must serve (`may rest`); a failed bind is
    /// fatal. The configured `[daemon] port` and `listen` binds happen
    /// regardless, best-effort.
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

    crate::session::ensure_private_dir_sync(paths.root())
        .with_context(|| format!("failed to secure {}", paths.root().display()))?;
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

fn bind_unix(path: &Path) -> anyhow::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt as _;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        crate::session::ensure_private_dir_sync(parent)
            .with_context(|| format!("failed to secure {}", parent.display()))?;
    }
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

/// The routers a TCP bind is served with, chosen by its address when it is
/// bound. A loopback address admits loopback `Host` and `Origin` headers
/// only. Any other address is remote access: every request to it must carry
/// an identity, resolved from the peer address through tailscaled's `WhoIs`
/// — the Tailscale header is only valid through tailscale serve's own proxy
/// on the remote listener.
struct TcpRouters {
    loopback: Arc<Router>,
    direct: Arc<Router>,
}

impl TcpRouters {
    /// The router serving a bind on `addr`.
    const fn router_for(&self, addr: SocketAddr) -> &Arc<Router> {
        if addr.ip().is_loopback() {
            &self.loopback
        } else {
            &self.direct
        }
    }

    /// Serves `listener`, bound on `addr`, with its policy's router.
    fn spawn(
        &self,
        listener: tokio::net::TcpListener,
        addr: SocketAddr,
        state: &Arc<state::DaemonState>,
    ) -> tokio::task::JoinHandle<()> {
        let router = self.router_for(addr).clone();
        if addr.ip().is_loopback() {
            tokio::spawn(serve::serve_tcp(listener, router))
        } else {
            tokio::spawn(serve::serve_tcp_tailnet(listener, router, state.clone()))
        }
    }
}

/// The daemon's TCP listeners.
///
/// The unix socket is the always-on transport for the TUI and one-shot
/// runs; TCP is optional and arrives by three routes — the `[daemon] port`
/// local listener a `gui` or `rest` run pins and serves, the `[daemon]
/// listen` addresses, and the explicit address a foreground `may rest`
/// asks for.
pub struct TcpListeners {
    /// The routers serving every bind, wired once the daemon's routers exist.
    routers: std::sync::OnceLock<TcpRouters>,
    inner: tokio::sync::Mutex<TcpListenersInner>,
}

#[derive(Default)]
struct TcpListenersInner {
    /// Bound address → its accept task, for every managed bind.
    running: HashMap<SocketAddr, tokio::task::JoinHandle<()>>,
    /// The bound address `[daemon] port` names — the loopback listener the
    /// web UI and its clients use. `None` until such a bind serves.
    local: Option<SocketAddr>,
    /// The explicit `may rest` bind; reconciles keep it however configured.
    explicit: Option<SocketAddr>,
}

impl TcpListenersInner {
    /// The address tunnels and links for this machine name: the local
    /// listener's when one serves, else the explicit bind's.
    fn local_addr(&self) -> Option<SocketAddr> {
        self.local.or(self.explicit)
    }
}

impl std::fmt::Debug for TcpListeners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpListeners").finish_non_exhaustive()
    }
}

impl Default for TcpListeners {
    fn default() -> Self {
        Self::new()
    }
}

impl TcpListeners {
    /// An empty registry; the routers are wired by [`Self::set_routers`]
    /// before the daemon serves.
    #[must_use]
    pub fn new() -> Self {
        Self {
            routers: std::sync::OnceLock::new(),
            inner: tokio::sync::Mutex::new(TcpListenersInner {
                running: HashMap::new(),
                local: None,
                explicit: None,
            }),
        }
    }

    /// Wires the routers every bind is served with; called once, before
    /// the first listener accepts.
    fn set_routers(&self, routers: TcpRouters) {
        self.routers
            .set(routers)
            .ok()
            .expect("the TCP routers are set once");
    }

    fn routers(&self) -> &TcpRouters {
        self.routers
            .get()
            .expect("the TCP routers are set before the daemon serves")
    }

    /// The addresses currently bound, for the startup report.
    async fn bound_addrs(&self) -> Vec<SocketAddr> {
        let mut addrs: Vec<SocketAddr> = self.inner.lock().await.running.keys().copied().collect();
        addrs.sort_unstable();
        addrs
    }

    /// Publishes the address links for this machine resolve to.
    fn report_local_addr(inner: &TcpListenersInner, state: &state::DaemonState) {
        state
            .remote_access_manager
            .set_local_listener_addr(inner.local_addr());
    }

    /// Serves `addr` as this run's required bind (`may rest`): a failure
    /// propagates so the run exits.
    ///
    /// # Errors
    /// Fails when the address cannot be bound.
    async fn bind_explicit(
        &self,
        addr: SocketAddr,
        state: &Arc<state::DaemonState>,
    ) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().await;
        if let std::collections::hash_map::Entry::Vacant(e) = inner.running.entry(addr) {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("failed to listen on {addr}"))?;
            tracing::info!(%addr, "listening");
            e.insert(self.routers().spawn(listener, addr, state));
        }
        inner.explicit = Some(addr);
        Self::report_local_addr(&inner, state);
        drop(inner);
        Ok(())
    }

    /// Reconciles the managed binds with `daemon`: the `[daemon] port`
    /// local listener and every `[daemon] listen` address, bound
    /// best-effort — a taken port warns, it never kills the daemon whose
    /// unix socket still serves.
    async fn reconcile_config(
        &self,
        daemon: &crate::config::DaemonConfig,
        state: &Arc<state::DaemonState>,
    ) {
        let local = daemon
            .port
            .map(|port| SocketAddr::from(([127, 0, 0, 1], port)));
        let mut inner = self.inner.lock().await;
        let explicit = inner.explicit;
        inner.running.retain(|addr, task| {
            let keep =
                Some(*addr) == explicit || Some(*addr) == local || daemon.listen.contains(addr);
            if !keep {
                task.abort();
                tracing::info!(%addr, "stopped listening");
            }
            keep
        });
        for addr in daemon.listen.iter().copied().chain(local) {
            if inner.running.contains_key(&addr) {
                continue;
            }
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    tracing::info!(%addr, remote = !addr.ip().is_loopback(), "listening");
                    inner
                        .running
                        .insert(addr, self.routers().spawn(listener, addr, state));
                }
                Err(error) => tracing::warn!(%addr, %error, "failed to listen"),
            }
        }
        inner.local = local.filter(|addr| inner.running.contains_key(addr));
        Self::report_local_addr(&inner, state);
        drop(inner);
    }

    /// The local loopback listener `gui` and `rest` clients ask for: bound
    /// at `port`, else at the configured `[daemon] port`, else at a free
    /// port the daemon picks. Whichever binds, it is pinned into the
    /// configuration (the caller persists it) so every later run reuses it.
    ///
    /// # Errors
    /// Fails when the resolved port cannot be bound.
    pub async fn ensure_local(
        &self,
        port: Option<u16>,
        state: &Arc<state::DaemonState>,
    ) -> anyhow::Result<SocketAddr> {
        let mut inner = self.inner.lock().await;
        let addr = match port.or(state.config_snapshot().daemon.port) {
            Some(port) => SocketAddr::from(([127, 0, 0, 1], port)),
            None => {
                if let Some(addr) = inner.local {
                    addr
                } else {
                    let listener =
                        tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                            .await
                            .context("no TCP port is free for the May listener")?;
                    listener
                        .local_addr()
                        .context("the May listener has no address")?
                }
            }
        };
        if let std::collections::hash_map::Entry::Vacant(e) = inner.running.entry(addr) {
            let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| {
                format!(
                    "{addr} is already in use — change [daemon] port in {} or free the port",
                    state.may_dir.join("config.toml").display()
                )
            })?;
            tracing::info!(%addr, "listening");
            e.insert(self.routers().spawn(listener, addr, state));
        }
        inner.local = Some(addr);
        // The pin is in-memory: the caller persists it once it knows the
        // address is served.
        let mut config = (*state.config_snapshot()).clone();
        config.daemon.port = Some(addr.port());
        state.replace_config(config);
        Self::report_local_addr(&inner, state);
        drop(inner);
        Ok(addr)
    }
}

/// Runs the daemon until it is asked to stop (`SIGINT`, `SIGTERM` or
/// `POST /api/daemon/shutdown`).
///
/// # Errors
/// Fails when another daemon holds the lock, or when the server cannot be
/// started or its socket bound.
pub async fn run_daemon(options: DaemonOptions) -> anyhow::Result<()> {
    // Everything the daemon starts shares the session it leads, so a daemon
    // started after an unexpected exit can end what the exited one left.
    let Some(process_session) = process_session::ProcessSession::lead()
        .context("failed to give the daemon a process session of its own")?
    else {
        return process_session::run_in_own_session().await;
    };
    let paths = DaemonPaths::user()?;
    let _instance = acquire_single_instance(&paths)?;
    lifecycle::ensure_tasks_link(&paths)
        .context("failed to link `tasks` to the daemon's executable")?;
    let hashline = crate::config::MayConfig::load()
        .await
        .is_ok_and(|config| config.experimental.hashline);
    lifecycle::ensure_hashline_links(&paths, hashline)
        .context("failed to link the hashline `search`/`edit` binaries")?;

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

    let started = crate::start(options.mode, process_session).await?;
    let state = started.state;
    let socket_router = Arc::new(started.socket_router);
    let remote_router = Arc::new(started.remote_router);
    state.tcp_listeners.set_routers(TcpRouters {
        loopback: Arc::new(started.loopback_router),
        direct: Arc::new(started.direct_router),
    });
    state
        .remote_access_manager
        .set_webhook_router(Arc::new(started.webhook_router));

    // A `rest` run's address must bind: a failure exits the run. `--host`
    // may name a non-loopback address, which gets the identity policy like
    // any other non-loopback bind.
    if let Some(tcp) = options.tcp {
        state.tcp_listeners.bind_explicit(tcp, &state).await?;
    }
    // Remote access needs identity: it forwards the tailnet to the remote
    // listener, bound on loopback because only the tunnel process connects.
    // A cloud home binds the port its fronting daemon forwards to.
    let remote_port = started
        .cloud_home
        .as_ref()
        .map_or(0, |home| home.remote_port);
    let remote_listener =
        tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], remote_port)))
            .await
            .context("failed to bind the remote listener")?;
    let unix_listener = bind_unix(&paths.socket())?;
    state.remote_access_manager.set_remote_listener_addr(
        remote_listener
            .local_addr()
            .context("the daemon's remote listener has no address")?,
    );
    let config = state.config_snapshot();
    state
        .tcp_listeners
        .reconcile_config(&config.daemon, &state)
        .await;
    crate::sync_remote_access(&state).await?;
    tokio::spawn(serve::serve_unix(unix_listener, socket_router));
    tokio::spawn(serve::serve_tcp(remote_listener, remote_router));
    let bound = state.tcp_listeners.bound_addrs().await;
    tracing::info!(?bound, socket = %paths.socket().display(), "May daemon listening");
    eprintln!("May daemon listening on {}", paths.socket().display());
    for addr in bound {
        eprintln!("May daemon listening on http://{addr}");
    }

    chat::watch_aux_settings(&state);
    daemon_sleep::spawn_reminders(&state);
    let config_state = state.clone();
    let mut config_events = state.ui_event_bus.subscribe();
    tokio::spawn(async move {
        loop {
            match config_events.recv().await {
                Ok(topics) if topics.contains(UiEventTopics::CONFIG) => {
                    crate::panic_boundary::round("config watcher", async {
                        config_state
                            .live_limits
                            .apply(&config_state.config_snapshot());
                        config_state
                            .tcp_listeners
                            .reconcile_config(&config_state.config_snapshot().daemon, &config_state)
                            .await;
                    })
                    .await;
                }
                // A lagged receiver may have missed the change.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    crate::panic_boundary::round("config watcher", async {
                        config_state
                            .live_limits
                            .apply(&config_state.config_snapshot());
                        config_state
                            .tcp_listeners
                            .reconcile_config(&config_state.config_snapshot().daemon, &config_state)
                            .await;
                    })
                    .await;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let repo_search = state.services.repo_search.clone();
    let indexer = SearchIndexer::new(
        state.db.clone(),
        RepoSearchCollections::new(repo_search.clone()),
    );
    tokio::spawn(indexer.run(
        state.search_index_wake.clone(),
        state.search_index_status.clone(),
    ));
    tokio::spawn(AutoIndexer::new(state.db.clone(), repo_search.clone()).run());

    wait_for_shutdown(&state).await;
    // Signal-triggered shutdowns never pass through the endpoint's broadcast,
    // so release the background loops now that the drain has begun.
    state.daemon_shutdown.notify_waiters();
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
    // The next daemon tells this stop from an unexpected exit by this record.
    state
        .db()
        .record_daemon_stop()
        .await
        .context("failed to record the daemon's stop")?;
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

async fn wait_for_shutdown(state: &state::DaemonState) {
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    // Register the waiter before reading `draining` so a shutdown that lands
    // between the endpoint's flag set and its broadcast cannot slip past.
    let requested = state.daemon_shutdown.notified();
    tokio::pin!(requested);
    requested.as_mut().enable();
    if state.draining.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = async {
            match terminate.as_mut() {
                Some(signal) => { signal.recv().await; }
                None => std::future::pending::<()>().await,
            }
        } => {}
        () = requested => {}
    }
}

/// The search-index state the daemon reports, for `may daemon status`.
#[must_use]
pub fn indexer_status_snapshot(
    status: &IndexerStatus,
) -> crate::repo_search::indexer::IndexerSnapshot {
    status.snapshot()
}

#[cfg(all(test, unix))]
mod fs_permissions_tests {
    use super::{DaemonPaths, acquire_single_instance, bind_unix};
    use std::os::unix::fs::PermissionsExt as _;

    #[tokio::test]
    async fn daemon_secures_may_dir_0700_and_socket_0600_before_exposing_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let may_dir = temp.path().join(".may");
        std::fs::create_dir_all(&may_dir).expect("create may_dir");
        std::fs::set_permissions(&may_dir, std::fs::Permissions::from_mode(0o755))
            .expect("initial world-rx mode");

        let paths = DaemonPaths::new(may_dir.clone());
        let _single = acquire_single_instance(&paths).expect("single instance lock");
        let dir_mode = std::fs::metadata(&may_dir)
            .expect("may_dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "~/.may must be tightened to 0700");

        let _listener = bind_unix(&paths.socket()).expect("bind unix socket");
        let sock_mode = std::fs::metadata(paths.socket())
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(sock_mode, 0o600, "daemon.sock must be 0600");

        let fresh_may = temp.path().join("fresh-may");
        crate::session::initialize_session_persistence(&fresh_may)
            .await
            .expect("persistence init");
        let fresh_mode = std::fs::metadata(&fresh_may)
            .expect("fresh_may metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(fresh_mode, 0o700, "persistence init must enforce 0700");
    }
}
