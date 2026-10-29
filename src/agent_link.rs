//! Main agents on one machine discover each other and exchange messages.
//!
//! Every live main agent listens on its own Unix socket under `~/.may/agents/`.
//! A socket that does not accept a connection belongs to a dead process and is
//! removed on sight, so a crashed session never shows up as live. A message is
//! stored in the recipient's pending-message queue in `history.db` first, so a
//! busy or restarting recipient does not lose it; the socket then only wakes
//! the recipient to deliver what is already stored.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aither_core::llm::{Tool, ToolContext, ToolResult};
use askama::Template;
use async_net::unix::{UnixListener, UnixStream};
use executor_core::AnyExecutorTask;
use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use futures_util::StreamExt as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::ChatDbError;
use crate::session::{
    MayHomePaths, PendingUserMessage, PendingUserMessageMode, SessionPaths, SessionRuntimeService,
};
use crate::xml::{XmlBlock, XmlBlockError};

/// Longest a single request to an agent's socket may take, accept to reply.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5);
/// Sockets probed at the same time by one listing.
const PROBE_CONCURRENCY: usize = 16;
/// Extension of an agent's socket file.
const SOCKET_EXTENSION: &str = "sock";
/// Heading of the `tasks.md` section that lists a session's roots.
const ROOTS_HEADING: &str = "## Roots";

/// Whether an agent is working or waiting for input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// A turn is in flight or the session is blocked on an interaction.
    Running,
    /// Nothing is in flight.
    Idle,
}

impl std::fmt::Display for Activity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Running => "running",
            Self::Idle => "idle",
        })
    }
}

/// What a live main agent reports about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    /// Session the agent runs.
    pub session_id: String,
    /// Session title, or the session id while it has none.
    pub name: String,
    /// Directory the agent was started in.
    pub cwd: PathBuf,
    /// Whether the agent is running or idle.
    pub activity: Activity,
    /// Roots listed in the session's `tasks.md`.
    pub roots: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
enum Request {
    Status,
    Wake,
}

#[derive(Debug, Serialize, Deserialize)]
enum Response {
    Status(AgentStatus),
    Woken,
}

/// Failure of the agent socket layer or of a message exchange.
#[derive(Debug, thiserror::Error)]
pub enum AgentLinkError {
    /// Socket or directory IO failed.
    #[error("agent socket {path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The socket path does not fit the OS limit on Unix socket paths.
    #[error(
        "agent socket path {path} is {bytes} bytes, over the {limit}-byte limit of Unix socket paths; use a shorter home directory"
    )]
    SocketPathTooLong {
        /// The path that cannot be bound.
        path: PathBuf,
        /// Its length in bytes.
        bytes: usize,
        /// Longest path the OS accepts.
        limit: usize,
    },
    /// A live agent already listens for the session.
    #[error("session {0} already has a live listener")]
    AlreadyListening(String),
    /// The peer did not answer within the exchange bound.
    #[error("agent at {0} did not answer within {EXCHANGE_TIMEOUT:?}")]
    Timeout(PathBuf),
    /// The peer sent something that is not a protocol response.
    #[error("agent at {path} sent an invalid response: {source}")]
    Protocol {
        /// Socket path.
        path: PathBuf,
        /// Decode failure.
        #[source]
        source: serde_json::Error,
    },
    /// The peer answered with a response that does not match the request.
    #[error("agent at {0} answered a different request")]
    UnexpectedResponse(PathBuf),
    /// Nothing listens on the socket.
    #[error("no live agent at {0}")]
    Unreachable(PathBuf),
    /// Storage failed.
    #[error(transparent)]
    Db(#[from] ChatDbError),
    /// `tasks.md` could not be read.
    #[error("failed to read tasks.md: {0}")]
    Tasks(#[source] std::io::Error),
    /// No live agent matches the name.
    #[error("no live agent named \"{0}\"; run `agent list`")]
    NoSuchAgent(String),
    /// Several live agents match the name.
    #[error("several live agents are named \"{0}\"; address one by its id from `agent list`")]
    AmbiguousAgent(String),
    /// The sender addressed itself.
    #[error("an agent cannot send a message to itself")]
    SendToSelf,
    /// A container session named a file outside its mounted sandbox.
    #[error("{0} is outside the session workspace")]
    OutsideSandbox(String),
    /// The message body is empty.
    #[error("the message in {0} is empty")]
    EmptyMessage(PathBuf),
    /// The prompt file could not be read.
    #[error("failed to read prompt file {path}: {source}")]
    PromptFile {
        /// File path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The delivery text could not be wrapped as hidden context.
    #[error(transparent)]
    Context(#[from] XmlBlockError),
    /// The message is stored but the recipient could not be woken.
    #[error(
        "message stored for \"{recipient}\" but waking it failed: {source}; it is delivered when the agent next runs"
    )]
    WakeFailed {
        /// Recipient name.
        recipient: String,
        /// Why waking failed.
        #[source]
        source: Box<Self>,
    },
}

/// Bytes of `sockaddr_un.sun_path`, including the terminating NUL, fixed by the OS ABI.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
const SUN_PATH_BYTES: usize = 104;
/// Bytes of `sockaddr_un.sun_path`, including the terminating NUL, fixed by the OS ABI.
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
const SUN_PATH_BYTES: usize = 108;

/// Hex digits of the session-id digest naming a socket file.
const SOCKET_NAME_DIGITS: usize = 16;

/// Socket path of a session: a fixed-length digest of its id, so long session
/// ids never overflow `sun_path`. The agent's identity comes from its Status reply.
fn socket_path(dir: &Path, session_id: &str) -> Result<PathBuf, AgentLinkError> {
    use std::fmt::Write as _;

    let digest = Sha256::digest(session_id.as_bytes());
    let mut name = String::with_capacity(SOCKET_NAME_DIGITS + 1 + SOCKET_EXTENSION.len());
    for byte in &digest[..SOCKET_NAME_DIGITS / 2] {
        write!(name, "{byte:02x}").expect("writing to a String cannot fail");
    }
    name.push('.');
    name.push_str(SOCKET_EXTENSION);
    let path = dir.join(name);
    let bytes = path.as_os_str().len();
    if bytes >= SUN_PATH_BYTES {
        return Err(AgentLinkError::SocketPathTooLong {
            path,
            bytes,
            limit: SUN_PATH_BYTES - 1,
        });
    }
    Ok(path)
}

/// Bound the wait for `future` to [`EXCHANGE_TIMEOUT`].
async fn bounded<T>(
    path: &Path,
    future: impl std::future::Future<Output = Result<T, AgentLinkError>>,
) -> Result<T, AgentLinkError> {
    futures_lite::future::or(future, async {
        async_io::Timer::after(EXCHANGE_TIMEOUT).await;
        Err(AgentLinkError::Timeout(path.to_path_buf()))
    })
    .await
}

/// Send one request to the socket at `path` and read its response.
async fn exchange(path: &Path, request: &Request) -> Result<Response, AgentLinkError> {
    let io_error = |source| AgentLinkError::Io {
        path: path.to_path_buf(),
        source,
    };
    bounded(path, async {
        let mut stream = match UnixStream::connect(path).await {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionRefused | ErrorKind::NotFound
                ) =>
            {
                return Err(AgentLinkError::Unreachable(path.to_path_buf()));
            }
            Err(error) => return Err(io_error(error)),
        };
        let mut line = serde_json::to_string(request).expect("a request serializes");
        line.push('\n');
        stream.write_all(line.as_bytes()).await.map_err(io_error)?;
        let mut reply = String::new();
        BufReader::new(stream.clone())
            .read_line(&mut reply)
            .await
            .map_err(io_error)?;
        serde_json::from_str(&reply).map_err(|source| AgentLinkError::Protocol {
            path: path.to_path_buf(),
            source,
        })
    })
    .await
}

/// Who a listener speaks for.
#[derive(Debug, Clone)]
pub struct AgentIdentity {
    /// Session the agent runs.
    pub session_id: String,
    /// Directory the agent was started in.
    pub cwd: PathBuf,
    /// The session sandbox, which holds `tasks.md`.
    pub sandbox_dir: PathBuf,
}

/// Wakes the host so it dispatches a message that was just stored for a session.
pub trait WakeAgent: Send + Sync + 'static {
    /// Called once per wake request, after the message is in the queue.
    fn wake(&self);
}

impl<F: Fn() + Send + Sync + 'static> WakeAgent for F {
    fn wake(&self) {
        self();
    }
}

/// One main agent's listening socket. Dropping it removes the socket file.
pub struct AgentListener<W> {
    listener: UnixListener,
    path: PathBuf,
    identity: AgentIdentity,
    runtime_service: Arc<SessionRuntimeService>,
    wake: Arc<W>,
}

impl<W> std::fmt::Debug for AgentListener<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentListener")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl<W> Drop for AgentListener<W> {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), %error, "failed to remove agent socket");
        }
    }
}

impl<W: WakeAgent> AgentListener<W> {
    /// Listen for `identity` under `dir`, replacing the socket of a dead process.
    ///
    /// # Errors
    /// [`AgentLinkError::AlreadyListening`] when a live agent holds the session's
    /// socket; [`AgentLinkError::Io`] when the socket cannot be created.
    pub async fn bind(
        dir: &Path,
        identity: AgentIdentity,
        runtime_service: Arc<SessionRuntimeService>,
        wake: W,
    ) -> Result<Self, AgentLinkError> {
        async_fs::create_dir_all(dir)
            .await
            .map_err(|source| AgentLinkError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        let path = socket_path(dir, &identity.session_id)?;
        match exchange(&path, &Request::Status).await {
            Ok(_) => return Err(AgentLinkError::AlreadyListening(identity.session_id)),
            Err(AgentLinkError::Unreachable(_)) => remove_socket(&path).await?,
            Err(other) => return Err(other),
        }
        let listener = UnixListener::bind(&path).map_err(|source| AgentLinkError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(Self {
            listener,
            path,
            identity,
            runtime_service,
            wake: Arc::new(wake),
        })
    }

    /// Path of the socket this listener owns.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Answer requests until the listener is dropped or accepting fails.
    pub async fn serve(self) {
        loop {
            let stream = match self.listener.accept().await {
                Ok((stream, _)) => stream,
                Err(error) => {
                    tracing::error!(path = %self.path.display(), %error, "agent socket stopped accepting");
                    return;
                }
            };
            let identity = self.identity.clone();
            let runtime_service = self.runtime_service.clone();
            let wake = self.wake.clone();
            let path = self.path.clone();
            executor_core::spawn(async move {
                if let Err(error) = handle(stream, &path, &identity, &runtime_service, &*wake).await
                {
                    tracing::warn!(path = %path.display(), %error, "agent socket request failed");
                }
            })
            .detach();
        }
    }
}

async fn handle(
    mut stream: UnixStream,
    path: &Path,
    identity: &AgentIdentity,
    runtime_service: &SessionRuntimeService,
    wake: &impl WakeAgent,
) -> Result<(), AgentLinkError> {
    let io_error = |source| AgentLinkError::Io {
        path: path.to_path_buf(),
        source,
    };
    bounded(path, async {
        let mut line = String::new();
        BufReader::new(stream.clone())
            .read_line(&mut line)
            .await
            .map_err(io_error)?;
        let request: Request =
            serde_json::from_str(&line).map_err(|source| AgentLinkError::Protocol {
                path: path.to_path_buf(),
                source,
            })?;
        let response = match request {
            Request::Status => Response::Status(status(identity, runtime_service).await?),
            Request::Wake => {
                wake.wake();
                Response::Woken
            }
        };
        let mut reply = serde_json::to_string(&response).expect("a response serializes");
        reply.push('\n');
        stream.write_all(reply.as_bytes()).await.map_err(io_error)
    })
    .await
}

async fn status(
    identity: &AgentIdentity,
    runtime_service: &SessionRuntimeService,
) -> Result<AgentStatus, AgentLinkError> {
    let name = session_name(runtime_service, &identity.session_id).await?;
    let state = runtime_service.get(&identity.session_id).await?.state;
    let tasks = SessionPaths::new(identity.sandbox_dir.clone())
        .tasks_content()
        .await
        .map_err(AgentLinkError::Tasks)?;
    Ok(AgentStatus {
        session_id: identity.session_id.clone(),
        name,
        cwd: identity.cwd.clone(),
        activity: if state.is_idle() {
            Activity::Idle
        } else {
            Activity::Running
        },
        roots: tasks.as_deref().map(parse_roots).unwrap_or_default(),
    })
}

async fn session_name(
    runtime_service: &SessionRuntimeService,
    session_id: &str,
) -> Result<String, AgentLinkError> {
    Ok(runtime_service
        .db()
        .get_session(session_id)
        .await?
        .and_then(|session| session.title)
        .unwrap_or_else(|| session_id.to_string()))
}

/// The list items of the `## Roots` section of a `tasks.md`.
fn parse_roots(tasks: &str) -> Vec<String> {
    tasks
        .lines()
        .skip_while(|line| line.trim() != ROOTS_HEADING)
        .skip(1)
        .take_while(|line| !line.starts_with("## "))
        .filter_map(|line| line.trim().strip_prefix("- "))
        .map(|root| root.trim().to_string())
        .filter(|root| !root.is_empty())
        .collect()
}

async fn remove_socket(path: &Path) -> Result<(), AgentLinkError> {
    match async_fs::remove_file(path).await {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(AgentLinkError::Io {
            path: path.to_path_buf(),
            source: error,
        }),
        _ => Ok(()),
    }
}

/// The directory of agent sockets: who is live, and how to reach them.
#[derive(Debug, Clone)]
pub struct AgentDirectory {
    dir: PathBuf,
}

impl AgentDirectory {
    /// Directory holding the agents' sockets, usually `~/.may/agents`.
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Every live agent, sorted by name. A socket that refuses connections is
    /// removed; one that accepts but does not answer is skipped and kept.
    ///
    /// # Errors
    /// [`AgentLinkError::Io`] when the directory cannot be read.
    pub async fn live(&self) -> Result<Vec<AgentStatus>, AgentLinkError> {
        let io_error = |source| AgentLinkError::Io {
            path: self.dir.clone(),
            source,
        };
        let mut entries = match async_fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_error(error)),
        };
        let mut sockets = Vec::new();
        while let Some(entry) = futures_lite::StreamExt::next(&mut entries).await {
            let path = entry.map_err(io_error)?.path();
            if path.extension().is_some_and(|ext| ext == SOCKET_EXTENSION) {
                sockets.push(path);
            }
        }
        let probes = futures_util::stream::iter(sockets)
            .map(probe)
            .buffered(PROBE_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        let mut live = probes.into_iter().flatten().collect::<Vec<_>>();
        live.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        Ok(live)
    }

    /// Ask the agent of `session_id` to dispatch what is stored for it.
    ///
    /// # Errors
    /// [`AgentLinkError::Unreachable`] when nothing listens, or the exchange error.
    pub async fn wake(&self, session_id: &str) -> Result<(), AgentLinkError> {
        let path = socket_path(&self.dir, session_id)?;
        match exchange(&path, &Request::Wake).await? {
            Response::Woken => Ok(()),
            Response::Status(_) => Err(AgentLinkError::UnexpectedResponse(path)),
        }
    }
}

/// Status of the agent at `path`; `None` when it is dead (its socket is removed)
/// or does not answer.
async fn probe(path: PathBuf) -> Option<AgentStatus> {
    match exchange(&path, &Request::Status).await {
        Ok(Response::Status(status)) => Some(status),
        Ok(Response::Woken) => {
            tracing::warn!(path = %path.display(), "agent socket answered a status request with a wake");
            None
        }
        Err(AgentLinkError::Unreachable(_)) => {
            if let Err(error) = remove_socket(&path).await {
                tracing::warn!(%error, "failed to remove a dead agent socket");
            }
            None
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "live agent socket gave no status");
            None
        }
    }
}

/// Listener tasks of one process, keyed by session. Dropping a task drops its
/// listener, which removes the socket.
pub struct AgentListeners {
    /// Runtime the listener tasks run on. Hosts such as the web server call
    /// from threads outside any Tokio context, where the global executor
    /// cannot spawn.
    runtime: tokio::runtime::Handle,
    tasks: Mutex<HashMap<String, AnyExecutorTask<()>>>,
}

impl std::fmt::Debug for AgentListeners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentListeners").finish_non_exhaustive()
    }
}

impl AgentListeners {
    /// Listeners whose tasks run on `runtime`.
    #[must_use]
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        Self {
            runtime,
            tasks: Mutex::default(),
        }
    }

    fn tasks(&self) -> std::sync::MutexGuard<'_, HashMap<String, AnyExecutorTask<()>>> {
        self.tasks
            .lock()
            .expect("agent listener map is never poisoned")
    }

    /// Start listening for the session unless this process already does.
    /// Callers serialize calls for one session; the run lease does for chat turns.
    ///
    /// # Errors
    /// The [`AgentListener::bind`] error.
    pub async fn ensure<W: WakeAgent>(
        &self,
        dir: &Path,
        identity: AgentIdentity,
        runtime_service: Arc<SessionRuntimeService>,
        wake: W,
    ) -> Result<(), AgentLinkError> {
        if self.tasks().contains_key(&identity.session_id) {
            return Ok(());
        }
        let session_id = identity.session_id.clone();
        let listener = AgentListener::bind(dir, identity, runtime_service, wake).await?;
        let task = {
            let _context = self.runtime.enter();
            executor_core::spawn(listener.serve())
        };
        self.tasks().insert(session_id, task);
        Ok(())
    }

    /// Listen for a stored session, which can receive a turn from now on.
    ///
    /// # Errors
    /// The [`AgentListener::bind`] error.
    pub async fn open_session<W: WakeAgent>(
        &self,
        host: &SessionHost,
        session_id: &str,
        profile_id: Option<&str>,
        wake: W,
    ) -> Result<(), AgentLinkError> {
        let home = MayHomePaths::new(host.may_dir.clone());
        let paths = profile_id.map_or_else(
            || home.session_paths(session_id),
            |profile_id| home.profile_home(profile_id).session_paths(session_id),
        );
        self.ensure(
            &home.agents_dir(),
            AgentIdentity {
                session_id: session_id.to_string(),
                cwd: host.cwd.clone(),
                sandbox_dir: paths.root().to_path_buf(),
            },
            host.runtime_service.clone(),
            wake,
        )
        .await
    }

    /// Listen for every stored session that is not archived.
    ///
    /// # Errors
    /// A storage error, or the [`AgentListener::bind`] error.
    pub async fn open_stored_sessions<W: WakeAgent>(
        &self,
        host: &SessionHost,
        wake_for: impl Fn(&str) -> W,
    ) -> Result<(), AgentLinkError> {
        for session in host.runtime_service.db().list_sessions().await? {
            if !session.archived {
                let wake = wake_for(&session.id);
                match self
                    .open_session(host, &session.id, session.profile_id.as_deref(), wake)
                    .await
                {
                    // Another process, such as a TUI, already runs this session.
                    Ok(()) | Err(AgentLinkError::AlreadyListening(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }

    /// Stop listening for `session_id`.
    pub fn remove(&self, session_id: &str) {
        self.tasks().remove(session_id);
    }

    /// Stop listening for every session.
    pub fn clear(&self) {
        self.tasks().clear();
    }

    /// Stop listening for every session but `session_id`.
    pub fn retain_only(&self, session_id: &str) {
        self.tasks().retain(|id, _| id == session_id);
    }
}

/// The process that hosts sessions: where its data lives and what it runs in.
#[derive(Debug, Clone)]
pub struct SessionHost {
    /// `~/.may`.
    pub may_dir: PathBuf,
    /// Directory the host was started in.
    pub cwd: PathBuf,
    /// Queue and runtime state of the shared `history.db`.
    pub runtime_service: Arc<SessionRuntimeService>,
}

/// What the `agent` command needs from the hosting session.
#[derive(Debug, Clone)]
pub struct AgentMessaging {
    /// Directory of agent sockets.
    pub directory: AgentDirectory,
    /// Queue and runtime state of the shared `history.db`.
    pub runtime_service: Arc<SessionRuntimeService>,
}

/// Tag names that frame text for the recipient; text from another agent must
/// not be able to open or close them.
const FRAMING_TAGS: [&str; 2] = ["system-reminder", "agent-message"];

/// Text written by another agent. Displaying it turns the `<` of any opening
/// or closing `system-reminder` or `agent-message` tag into `&lt;`, so it can
/// neither close its own block nor open a harness one; all other text,
/// code included, is verbatim.
#[derive(Debug, Clone, Copy)]
struct UntrustedText<'a>(&'a str);

impl std::fmt::Display for UntrustedText<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut rest = self.0;
        while let Some(at) = rest.find('<') {
            let (before, tag) = rest.split_at(at);
            f.write_str(before)?;
            let name = tag[1..].strip_prefix('/').unwrap_or_else(|| &tag[1..]);
            let framing = FRAMING_TAGS.iter().any(|framing| {
                name.get(..framing.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(framing))
            });
            f.write_str(if framing { "&lt;" } else { "<" })?;
            rest = &tag[1..];
        }
        f.write_str(rest)
    }
}

/// A session name rendered safe inside a double-quoted attribute.
#[derive(Debug, Clone, Copy)]
struct AttributeText<'a>(&'a str);

impl std::fmt::Display for AttributeText<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for ch in self.0.chars() {
            match ch {
                '&' => f.write_str("&amp;")?,
                '<' => f.write_str("&lt;")?,
                '>' => f.write_str("&gt;")?,
                '"' => f.write_str("&quot;")?,
                '\n' | '\r' => f.write_str(" ")?,
                other => write!(f, "{other}")?,
            }
        }
        Ok(())
    }
}

#[derive(Template)]
#[template(path = "agent_message.txt", escape = "none")]
struct AgentMessageText<'a> {
    sender: AttributeText<'a>,
    body: UntrustedText<'a>,
}

#[derive(Template)]
#[template(path = "agent_sent.txt", escape = "none")]
struct AgentSentText<'a> {
    recipient: &'a str,
}

struct ListedAgent {
    status: AgentStatus,
    is_self: bool,
}

#[derive(Template)]
#[template(path = "agent_list.txt", escape = "none")]
struct AgentListText {
    agents: Vec<ListedAgent>,
}

impl AgentMessaging {
    /// Store `body` in the queue of the live agent addressed by `target` (name
    /// or session id) and wake it. Returns the recipient's name.
    ///
    /// # Errors
    /// See [`AgentLinkError`]; a failed wake still leaves the message stored.
    ///
    /// # Panics
    /// When the compile-time message template fails to render.
    pub async fn send(
        &self,
        sender_session_id: &str,
        target: &str,
        body: &str,
    ) -> Result<String, AgentLinkError> {
        let live = self.directory.live().await?;
        let mut matches = live
            .iter()
            .filter(|agent| agent.session_id == target || agent.name == target);
        let recipient = matches
            .next()
            .ok_or_else(|| AgentLinkError::NoSuchAgent(target.to_string()))?;
        if matches.next().is_some() {
            return Err(AgentLinkError::AmbiguousAgent(target.to_string()));
        }
        if recipient.session_id == sender_session_id {
            return Err(AgentLinkError::SendToSelf);
        }
        let sender = session_name(&self.runtime_service, sender_session_id).await?;
        let text = AgentMessageText {
            sender: AttributeText(&sender),
            body: UntrustedText(body),
        }
        .render()
        .expect("agent message template must render");
        let message = PendingUserMessage {
            content: String::new(),
            attachments: Vec::new(),
            hidden_context_xml: Some(XmlBlock::parse(text)?),
            created_at: jiff::Timestamp::now().to_string(),
            delivery_mode: PendingUserMessageMode::Steer,
        };
        self.runtime_service
            .enqueue(
                &recipient.session_id,
                message,
                PendingUserMessageMode::Steer,
            )
            .await?;
        self.directory
            .wake(&recipient.session_id)
            .await
            .map_err(|source| AgentLinkError::WakeFailed {
                recipient: recipient.name.clone(),
                source: Box::new(source),
            })?;
        Ok(recipient.name.clone())
    }
}

/// See which other main agents run on this machine and message them.
///
/// Subcommands:
/// - `agent list`
/// - `agent send <name> <prompt-file>` stores the file's text as a message to the
///   agent named `<name>` (or addressed by its id) and returns at once; the agent
///   receives it at its next turn boundary, and an idle agent is woken by it. A
///   reply is a message sent back the same way.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AgentArgs {
    /// List the live agents with their working directory, state and roots.
    List,
    /// Send the text of a file to another agent.
    Send {
        /// Name or id of the recipient, as `agent list` shows it.
        name: String,
        /// File holding the message, as the terminal spells its path.
        prompt_file: String,
    },
}

/// The `agent` terminal command; reserved for the main agent.
#[derive(Debug, Clone)]
pub struct AgentTool {
    messaging: AgentMessaging,
    session_id: String,
    sandbox_dir: PathBuf,
    /// Where the sandbox appears inside the session's container, if it has one.
    workspace_mount: Option<aither_sandbox::MountRoot>,
}

/// The host file behind a path as the session's terminal spells it. In a
/// container session the sandbox is mounted at the container root, so an
/// absolute path must lie beneath it and nothing may climb out of it.
fn resolve_prompt_path(
    sandbox_dir: &Path,
    mount: Option<&aither_sandbox::MountRoot>,
    prompt_file: &str,
) -> Result<PathBuf, AgentLinkError> {
    let Some(mount) = mount else {
        return Ok(sandbox_dir.join(prompt_file));
    };
    let outside = || AgentLinkError::OutsideSandbox(prompt_file.to_string());
    let requested = Path::new(prompt_file);
    let relative = if requested.is_absolute() {
        requested
            .strip_prefix(mount.container_root.as_str())
            .map_err(|_| outside())?
    } else {
        requested
    };
    if relative
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(outside());
    }
    Ok(mount.host_root.join(relative))
}

impl AgentTool {
    /// The command for the session `session_id` whose sandbox is `sandbox_dir`.
    #[must_use]
    pub const fn new(
        messaging: AgentMessaging,
        session_id: String,
        sandbox_dir: PathBuf,
        workspace_mount: Option<aither_sandbox::MountRoot>,
    ) -> Self {
        Self {
            messaging,
            session_id,
            sandbox_dir,
            workspace_mount,
        }
    }
}

impl Tool for AgentTool {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("agent")
    }

    type Arguments = AgentArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> aither_core::Result<Self::Res> {
        let text = match args {
            AgentArgs::List => {
                let agents = self
                    .messaging
                    .directory
                    .live()
                    .await?
                    .into_iter()
                    .map(|status| ListedAgent {
                        is_self: status.session_id == self.session_id,
                        status,
                    })
                    .collect();
                AgentListText { agents }
                    .render()
                    .expect("agent list template must render")
            }
            AgentArgs::Send { name, prompt_file } => {
                let path = resolve_prompt_path(
                    &self.sandbox_dir,
                    self.workspace_mount.as_ref(),
                    &prompt_file,
                )?;
                let body = async_fs::read_to_string(&path).await.map_err(|source| {
                    AgentLinkError::PromptFile {
                        path: path.clone(),
                        source,
                    }
                })?;
                if body.trim().is_empty() {
                    return Err(AgentLinkError::EmptyMessage(path).into());
                }
                let recipient = self
                    .messaging
                    .send(&self.session_id, &name, body.trim())
                    .await?;
                AgentSentText {
                    recipient: &recipient,
                }
                .render()
                .expect("agent sent template must render")
            }
        };
        Ok(ToolResult::text(text))
    }
}

#[cfg(test)]
mod tests;
