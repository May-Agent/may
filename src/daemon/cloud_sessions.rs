//! Cloud sessions, as the daemon fronting them runs them.
//!
//! A cloud session's home environment is a codespace whose own daemon runs
//! the session. This daemon keeps the session's row and the codespace it
//! lives on (`cloud_sessions`), and its clients stay unchanged: every
//! request under `/api/sessions/{id}` of a cloud session is relayed
//! ([`CloudSessionRelay`]) to the codespace daemon's remote listener,
//! through `gh codespace ports forward`, with the bearer token that daemon
//! admits — never its loopback listener, which would trust forwarded
//! traffic as the machine's own user.
//!
//! Each cloud session has a host ([`CloudHost`]), one actor owning the
//! session's forward. A message to a suspended session starts its
//! codespace, bootstraps it (May installed, its daemon started) and opens
//! the forward, each step published as a `cloud_session` event so every
//! client shows the wait.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use arc_swap::ArcSwap;
use bytes::Bytes;
use dashmap::DashMap;
use futures_util::TryStreamExt as _;
use http_body_util::{BodyDataStream, BodyExt as _, Full, StreamBody};
use hyper::body::Frame;
use skyzen::middleware::{Middleware, Next};
use skyzen::{BodyError, Error, Request, Response, StatusCode};
use tokio::sync::broadcast;

use super::types::{CloudProviderName, CloudSessionEvent, CloudSessionPhase, SessionLocation};
use crate::cloud::access::{AccessError, AccessToken, ProviderPush, local_access};
use crate::cloud::bootstrap::{Bootstrap, MayBuild, RemoteShell};
use crate::cloud::codespaces::{CodespaceSource, Codespaces, CodespacesAccount};
use crate::cloud::gh::{CodespaceShell, PortForward};
use crate::cloud::home::{CloudHome, FrontingToken, REMOTE_PORT};
use crate::cloud::{
    CloudError, CloudMachine, CloudProvider, MachineId, MachineState, NewMachine, RunningWait,
    until_running,
};
use crate::config::{MayConfig, ProviderType};
use crate::db::{ChatDb, CloudSessionRow};
use crate::issue::GitHubTokenSource as _;
use crate::issue::github::GhAccountTokenSource;
use crate::ui_event_bus::{UiEventBus, UiEventTopics};
use crate::workspace::GitHubRepository;

/// How a codespace is waited on to run. GitHub publishes neither a start
/// nor a build time; a first start builds the devcontainer image, so the
/// deadline covers an image build, while the polls follow flyco's: one
/// second at first, doubling up to ten.
const RUNNING_WAIT: RunningWait = RunningWait {
    deadline: Duration::from_secs(600),
    first_poll: Duration::from_secs(1),
    longest_poll: Duration::from_secs(10),
};

/// How long the forward may take to answer once the codespace daemon runs:
/// `may daemon start` returns once that daemon answers, so only `gh`
/// connecting the relay remains, which takes seconds.
const CONNECT_DEADLINE: Duration = Duration::from_secs(60);

/// The wait between health probes of a forward being connected.
const CONNECT_POLL: Duration = Duration::from_millis(250);

/// The longest a codespace display name may be (GitHub's
/// `display_name`, 48 characters).
const DISPLAY_NAME_LIMIT: usize = 48;

/// Events buffered for a slow `/api/events` client.
const EVENT_BUFFER: usize = 64;

/// Why a cloud session cannot be served now.
#[derive(Debug, thiserror::Error)]
pub enum CloudSessionError {
    /// The codespace is suspended, and the request is no message.
    #[error(
        "the session's codespace is suspended; it starts again when the session gets a message"
    )]
    Suspended,
    /// The provider, the machine or the forward failed.
    #[error(transparent)]
    Cloud(#[from] CloudError),
    /// The machine's daemon refused or did not answer as documented.
    #[error("the codespace's May daemon {0}")]
    Daemon(String),
    /// The session's records could not be read or written.
    #[error("cloud session records: {0}")]
    Records(#[from] crate::db::ChatDbError),
    /// The session model's provider cannot be pushed to the codespace.
    #[error("the session's model provider: {0}")]
    Provider(String),
}

impl CloudSessionError {
    /// The HTTP error a relayed request gets.
    fn into_http(self) -> Error {
        let status = match &self {
            Self::Suspended => StatusCode::SERVICE_UNAVAILABLE,
            Self::Cloud(CloudError::Gone(_)) => StatusCode::GONE,
            Self::Records(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Provider(_) => StatusCode::CONFLICT,
            Self::Cloud(_) | Self::Daemon(_) => StatusCode::BAD_GATEWAY,
        };
        Error::msg(self.to_string()).set_status(status)
    }
}

/// The codespace daemon's remote listener, through the forward.
#[derive(Debug, Clone)]
pub struct Endpoint {
    addr: SocketAddr,
    token: Arc<str>,
}

impl Endpoint {
    /// The listener at `addr`, admitting `token`.
    pub(crate) fn new(addr: SocketAddr, token: &str) -> Self {
        Self {
            addr,
            token: token.into(),
        }
    }

    /// Sends `request` (its path, method, headers and body) with this
    /// daemon's token, answering with the codespace daemon's response.
    async fn send(
        &self,
        request: hyper::Request<RelayBody>,
    ) -> Result<hyper::Response<hyper::body::Incoming>, CloudSessionError> {
        let stream = async_net::TcpStream::connect(self.addr)
            .await
            .map_err(|error| CloudSessionError::Daemon(format!("is not reachable: {error}")))?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(smol_hyper::rt::FuturesIo::new(stream))
                .await
                .map_err(|error| CloudSessionError::Daemon(format!("refused HTTP: {error}")))?;
        executor_core::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!(%error, "cloud session connection closed");
            }
        })
        .detach();
        let (mut parts, body) = request.into_parts();
        for name in [
            hyper::header::HOST,
            hyper::header::AUTHORIZATION,
            hyper::header::ORIGIN,
            hyper::header::COOKIE,
        ] {
            parts.headers.remove(name);
        }
        // Header names are lowercase; the tailnet's identity headers are
        // this listener's to trust, never the codespace's.
        let tailnet: Vec<_> = parts
            .headers
            .keys()
            .filter(|name| name.as_str().starts_with("tailscale-"))
            .cloned()
            .collect();
        for name in tailnet {
            parts.headers.remove(name);
        }
        let host = hyper::header::HeaderValue::from_str(&self.addr.to_string())
            .expect("a socket address is a valid Host");
        parts.headers.insert(hyper::header::HOST, host);
        let bearer = hyper::header::HeaderValue::from_str(&format!("Bearer {}", self.token))
            .expect("a hex token is a valid header value");
        parts.headers.insert(hyper::header::AUTHORIZATION, bearer);
        sender
            .send_request(hyper::Request::from_parts(parts, body))
            .await
            .map_err(|error| CloudSessionError::Daemon(format!("dropped the request: {error}")))
    }

    /// `method path` with an optional JSON body, answering with the status
    /// and the body's bytes.
    async fn exchange(
        &self,
        method: hyper::Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, Bytes), CloudSessionError> {
        let mut request = hyper::Request::builder().method(method).uri(path);
        if body.is_some() {
            request = request.header(hyper::header::CONTENT_TYPE, "application/json");
        }
        let request = request
            .body(full(body.unwrap_or_default()))
            .expect("a relay request always builds");
        let response = self.send(request).await?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|error| CloudSessionError::Daemon(format!("cut its answer off: {error}")))?
            .to_bytes();
        Ok((status, bytes))
    }

    /// Whether the codespace daemon answers its health check.
    async fn healthy(&self) -> bool {
        self.exchange(hyper::Method::GET, "/api/health", None)
            .await
            .is_ok_and(|(status, _)| status.is_success())
    }
}

/// The body of a relayed request.
type RelayBody = http_body_util::combinators::BoxBody<Bytes, BodyError>;

fn full(bytes: Vec<u8>) -> RelayBody {
    Full::new(Bytes::from(bytes))
        .map_err(|never| match never {})
        .boxed()
}

/// What a fronting daemon needs from the world to run cloud sessions: the
/// provider, the account's token, the machine's command channel and the
/// forward.
pub trait CloudBackend: Send + Sync + 'static {
    /// The provider of the account's machines.
    type Provider: CloudProvider<Source = CodespaceSource> + 'static;
    /// The command channel to a machine.
    type Shell: RemoteShell + 'static;
    /// A forward of the machine daemon's port.
    type Forward: Forward + 'static;

    /// The provider for `account`.
    fn provider(&self, account: &CodespacesAccount) -> Self::Provider;

    /// The GitHub token of `account`.
    fn github_token(
        &self,
        account: &CodespacesAccount,
    ) -> impl Future<Output = Result<String, CloudError>> + Send;

    /// Commands on `machine`, as the account `token` belongs to.
    fn shell(&self, machine: &MachineId, token: &str) -> Self::Shell;

    /// Forwards `machine`'s cloud home port, as the account `token`
    /// belongs to.
    ///
    /// # Errors
    /// Fails when the forward cannot be opened.
    fn forward(&self, machine: &MachineId, token: &str) -> Result<Self::Forward, CloudError>;

    /// The build the machine gets.
    ///
    /// # Errors
    /// Fails when this build cannot be found for the machine.
    fn build(&self) -> Result<MayBuild, CloudError>;

    /// This machine's access token of an OAuth provider of
    /// `provider_type`, refreshed here when due; `None` for a provider that
    /// takes an API key.
    fn access(
        &self,
        provider_type: ProviderType,
    ) -> impl Future<Output = Result<Option<AccessToken>, AccessError>> + Send;
}

/// A forward of a machine daemon's port to this machine.
pub trait Forward: Send {
    /// The loopback address the port answers on.
    fn local(&self) -> SocketAddr;
    /// How the forward ended, once it has.
    fn exited(&mut self) -> Option<String>;
}

impl Forward for PortForward {
    fn local(&self) -> SocketAddr {
        Self::local(self)
    }

    fn exited(&mut self) -> Option<String> {
        Self::exited(self)
    }
}

/// GitHub Codespaces through the REST API and the `gh` login.
#[derive(Debug, Clone, Copy, Default)]
pub struct GitHubCloud;

impl CloudBackend for GitHubCloud {
    type Provider = Codespaces<GhAccountTokenSource>;
    type Shell = CodespaceShell;
    type Forward = PortForward;

    fn provider(&self, account: &CodespacesAccount) -> Self::Provider {
        Codespaces::for_account(account.clone())
    }

    async fn github_token(&self, account: &CodespacesAccount) -> Result<String, CloudError> {
        Ok(GhAccountTokenSource::new(account.login.clone())
            .token()
            .await?)
    }

    fn shell(&self, machine: &MachineId, token: &str) -> Self::Shell {
        CodespaceShell::new(machine.clone(), token.to_owned())
    }

    fn forward(&self, machine: &MachineId, token: &str) -> Result<Self::Forward, CloudError> {
        PortForward::open(machine, token, REMOTE_PORT)
    }

    fn build(&self) -> Result<MayBuild, CloudError> {
        MayBuild::this_build()
    }

    fn access(
        &self,
        provider_type: ProviderType,
    ) -> impl Future<Output = Result<Option<AccessToken>, AccessError>> + Send {
        local_access(provider_type)
    }
}

/// One request to a session's host.
enum HostRequest {
    /// A client's request needs the codespace daemon's endpoint.
    Endpoint {
        /// Whether a suspended codespace is started for it: a message.
        wake: bool,
        reply: async_channel::Sender<Result<Endpoint, CloudSessionError>>,
    },
    /// The codespace's session started or stopped running a turn.
    Running(bool),
    /// The access token pushed last is due to be replaced.
    RefreshDue,
}

/// A cloud session's host: the actor owning the session's forward.
#[derive(Debug, Clone)]
pub struct CloudHost {
    row: Arc<CloudSessionRow>,
    requests: async_channel::Sender<HostRequest>,
}

impl CloudHost {
    /// Where the session's location is, for its API object.
    pub(crate) fn location(&self) -> SessionLocation {
        SessionLocation {
            provider: CloudProviderName::Codespaces,
            machine: self.row.machine_id.clone(),
            machine_type: self.row.machine_type.clone(),
        }
    }

    /// The codespace daemon's endpoint once it serves the session; with
    /// `wake`, a suspended codespace is started first.
    async fn endpoint(&self, wake: bool) -> Result<Endpoint, CloudSessionError> {
        let (reply, answer) = async_channel::bounded(1);
        self.requests
            .send(HostRequest::Endpoint { wake, reply })
            .await
            .map_err(|_| CloudSessionError::Daemon("host has stopped".to_owned()))?;
        answer
            .recv()
            .await
            .map_err(|_| CloudSessionError::Daemon("host has stopped".to_owned()))?
    }
}

/// Every cloud session this daemon fronts.
pub struct CloudSessions<B: CloudBackend = GitHubCloud> {
    backend: Arc<B>,
    db: ChatDb,
    /// This daemon's live configuration, whose providers are pushed.
    config: Arc<ArcSwap<MayConfig>>,
    mirror: Mirror,
    hosts: DashMap<String, CloudHost>,
    events: broadcast::Sender<CloudSessionEvent>,
}

impl<B: CloudBackend> std::fmt::Debug for CloudSessions<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudSessions")
            .field("sessions", &self.hosts.len())
            .finish_non_exhaustive()
    }
}

impl<B: CloudBackend> CloudSessions<B> {
    /// The cloud sessions `db` records, served through `backend` with the
    /// providers of `config`.
    ///
    /// # Errors
    /// Fails when the records cannot be read.
    pub(crate) async fn load(
        db: ChatDb,
        backend: B,
        config: Arc<ArcSwap<MayConfig>>,
        bus: UiEventBus,
    ) -> Result<Self, crate::db::ChatDbError> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let sessions = Self {
            backend: Arc::new(backend),
            db: db.clone(),
            config,
            mirror: Mirror {
                db: db.clone(),
                bus,
                runtimes: Arc::default(),
            },
            hosts: DashMap::new(),
            events,
        };
        for row in db.list_cloud_sessions().await? {
            sessions.host_for(row);
        }
        Ok(sessions)
    }

    /// Every step of every cloud session's machine, as it happens.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<CloudSessionEvent> {
        self.events.subscribe()
    }

    /// The host of `session_id`, when it is a cloud session.
    pub(crate) fn host(&self, session_id: &str) -> Option<CloudHost> {
        self.hosts.get(session_id).map(|host| host.clone())
    }

    /// The runtime state the codespace daemon reports for `session_id`,
    /// while its events are followed and it is not idle.
    pub(crate) fn mirrored_runtime(&self, session_id: &str) -> Option<serde_json::Value> {
        self.mirror
            .runtimes
            .get(session_id)
            .map(|runtime| runtime.clone())
    }

    fn host_for(&self, row: CloudSessionRow) -> CloudHost {
        let (requests, inbox) = async_channel::unbounded();
        let row = Arc::new(row);
        let host = CloudHost {
            row: row.clone(),
            requests,
        };
        let actor = Host {
            backend: self.backend.clone(),
            db: self.db.clone(),
            config: self.config.clone(),
            mirror: self.mirror.clone(),
            publisher: Publisher {
                session_id: row.session_id.clone(),
                events: self.events.clone(),
            },
            row: row.clone(),
            // Weak: the host stops once nothing but itself could reach it.
            own: host.requests.downgrade(),
            forward: None,
            follower: None,
            suspended: false,
            running: false,
            refresh_at: None,
            refresh_timer: None,
        };
        executor_core::spawn(actor.run(inbox)).detach();
        self.hosts.insert(row.session_id.clone(), host.clone());
        host
    }

    /// The machine types a cloud session of `project` may start on.
    ///
    /// # Errors
    /// Fails when the provider cannot be reached or refuses.
    pub(crate) async fn catalog(
        &self,
        account: &CodespacesAccount,
        project: Option<&GitHubRepository>,
    ) -> Result<Vec<crate::cloud::MachineType>, CloudError> {
        let provider = self.backend.provider(account);
        let source = provider.source(project).await?;
        provider.catalog(&source).await
    }

    /// Creates the codespace session `session_id` lives on, of
    /// `machine_type`, and records it. The codespace starts on its own;
    /// the host brings it to serve the session in the background, so the
    /// session's first message finds it ready or waits for it.
    ///
    /// # Errors
    /// Fails when the codespace cannot be created or recorded.
    pub(crate) async fn create(
        &self,
        session_id: &str,
        account: &CodespacesAccount,
        project: Option<&GitHubRepository>,
        machine_type: &str,
    ) -> Result<SessionLocation, CloudSessionError> {
        let publisher = Publisher {
            session_id: session_id.to_owned(),
            events: self.events.clone(),
        };
        publisher.phase(CloudSessionPhase::Creating, None);
        let created = self
            .create_machine(session_id, account, project, machine_type)
            .await;
        let row = match created {
            Ok(row) => row,
            Err(error) => {
                publisher.failed(&error);
                return Err(error);
            }
        };
        self.db.insert_cloud_session(&row).await?;
        let host = self.host_for(row);
        let location = host.location();
        executor_core::spawn(async move {
            if let Err(error) = host.endpoint(true).await {
                tracing::warn!(%error, "a new cloud session's codespace did not come up");
            }
        })
        .detach();
        Ok(location)
    }

    async fn create_machine(
        &self,
        session_id: &str,
        account: &CodespacesAccount,
        project: Option<&GitHubRepository>,
        machine_type: &str,
    ) -> Result<CloudSessionRow, CloudSessionError> {
        let provider = self.backend.provider(account);
        let source = provider.source(project).await?;
        let machine = provider
            .create(
                &source,
                &NewMachine {
                    machine_type: machine_type.to_owned(),
                    display_name: display_name(session_id),
                },
            )
            .await?;
        let workspace = crate::cloud::bootstrap::workspace_of(
            source.clone.as_ref().unwrap_or(&source.repository),
        );
        Ok(CloudSessionRow {
            session_id: session_id.to_owned(),
            account: account.login.clone(),
            machine_id: machine.id.as_str().to_owned(),
            machine_type: machine_type.to_owned(),
            token: FrontingToken::generate().expose().to_owned(),
            clone_repository: source.clone.map(|repository| repository.to_string()),
            workspace,
        })
    }

    /// Deletes `session_id`'s codespace and forgets it; its host stops.
    ///
    /// # Errors
    /// Fails when the provider refuses the deletion or the record cannot be
    /// removed.
    pub(crate) async fn delete(&self, session_id: &str) -> Result<(), CloudSessionError> {
        let Some((_, host)) = self.hosts.remove(session_id) else {
            return Ok(());
        };
        let provider = self.backend.provider(&CodespacesAccount {
            login: host.row.account.clone(),
        });
        provider
            .delete(&MachineId::new(host.row.machine_id.clone()))
            .await?;
        self.db.delete_cloud_session(session_id).await?;
        Ok(())
    }
}

/// The codespace display name of a session's machine.
fn display_name(session_id: &str) -> String {
    let name = format!("May {session_id}");
    // GitHub counts characters; a byte bound on a character boundary keeps
    // within that count.
    name.get(..name.floor_char_boundary(DISPLAY_NAME_LIMIT))
        .unwrap_or(&name)
        .to_owned()
}

/// Publishes a session's steps.
#[derive(Clone)]
struct Publisher {
    session_id: String,
    events: broadcast::Sender<CloudSessionEvent>,
}

impl Publisher {
    fn phase(&self, phase: CloudSessionPhase, machine_state: Option<MachineState>) {
        // No subscriber is no client to show the step to.
        let _ = self.events.send(CloudSessionEvent {
            session_id: self.session_id.clone(),
            phase,
            machine_state,
            error: None,
        });
    }

    fn failed(&self, error: &CloudSessionError) {
        let _ = self.events.send(CloudSessionEvent {
            session_id: self.session_id.clone(),
            phase: CloudSessionPhase::Failed,
            machine_state: None,
            error: Some(error.to_string()),
        });
    }
}

/// A session's host, the actor owning its forward. It answers each request
/// with the endpoint, bringing the codespace up when the forward is gone.
/// Once the codespace daemon serves the session, and before each message,
/// it pushes the session model's provider; while the session runs a turn,
/// it pushes a fresh access token before the one pushed last expires.
struct Host<B: CloudBackend> {
    backend: Arc<B>,
    db: ChatDb,
    config: Arc<ArcSwap<MayConfig>>,
    mirror: Mirror,
    publisher: Publisher,
    row: Arc<CloudSessionRow>,
    /// This host's own requests, for its timer and its event follower.
    own: async_channel::WeakSender<HostRequest>,
    forward: Option<B::Forward>,
    /// Follows the codespace daemon's events while the forward stands; a
    /// new forward replaces it, dropping the old one.
    follower: Option<executor_core::AnyExecutorTask<()>>,
    /// Only a message starts a suspended codespace, so once it is seen
    /// suspended every other request is answered so without asking GitHub
    /// again, however often a client retries.
    suspended: bool,
    /// Whether the codespace's session runs a turn, as its events say.
    running: bool,
    /// When the access token pushed last is due to be replaced.
    refresh_at: Option<SystemTime>,
    /// Wakes the host at `refresh_at`; replaced by each push.
    refresh_timer: Option<executor_core::AnyExecutorTask<()>>,
}

impl<B: CloudBackend> Host<B> {
    async fn run(mut self, inbox: async_channel::Receiver<HostRequest>) {
        while let Ok(request) = inbox.recv().await {
            match request {
                HostRequest::Endpoint { wake, reply } => {
                    let answer = self.endpoint(wake).await;
                    // The requester may have gone; the forward stays for
                    // the next one.
                    let _ = reply.send(answer).await;
                }
                HostRequest::Running(running) => {
                    self.running = running;
                    // A turn that starts after the token came due, without
                    // a message (a background command's), gets a fresh one.
                    if running && self.refresh_at.is_some_and(|at| SystemTime::now() >= at) {
                        self.refresh().await;
                    }
                }
                HostRequest::RefreshDue => {
                    if self.running {
                        self.refresh().await;
                    }
                }
            }
        }
    }

    async fn endpoint(&mut self, wake: bool) -> Result<Endpoint, CloudSessionError> {
        if self.suspended && !wake {
            return Err(CloudSessionError::Suspended);
        }
        let answer = serve(
            &*self.backend,
            &self.db,
            &self.row,
            &self.publisher,
            &mut self.forward,
            wake,
        )
        .await;
        self.suspended = matches!(answer, Err(CloudSessionError::Suspended));
        let answer = match answer {
            Ok(served) => {
                if served.fresh {
                    let following = executor_core::spawn(self.mirror.clone().follow(
                        served.endpoint.clone(),
                        self.row.session_id.clone(),
                        self.own.clone(),
                    ));
                    self.follower = Some(following);
                }
                if wake || served.fresh {
                    self.push(&served.endpoint).await.map(|()| served.endpoint)
                } else {
                    Ok(served.endpoint)
                }
            }
            Err(error) => Err(error),
        };
        if let Err(error) = &answer
            && !matches!(error, CloudSessionError::Suspended)
        {
            self.publisher.failed(error);
        }
        answer
    }

    /// Pushes the session model's provider and arms the timer for its
    /// access token's replacement.
    async fn push(&mut self, endpoint: &Endpoint) -> Result<(), CloudSessionError> {
        let refresh_at = push_provider(
            &*self.backend,
            &self.config.load_full(),
            &self.row.session_id,
            endpoint,
        )
        .await?;
        self.refresh_at = refresh_at;
        self.refresh_timer = refresh_at.map(|at| {
            let own = self.own.clone();
            executor_core::spawn(async move {
                // A time already past is due now.
                let wait = at.duration_since(SystemTime::now()).unwrap_or_default();
                async_io::Timer::after(wait).await;
                if let Some(own) = own.upgrade() {
                    let _ = own.send(HostRequest::RefreshDue).await;
                }
            })
        });
        Ok(())
    }

    /// Pushes a fresh access token over the standing forward; with none, a
    /// suspended codespace is left as it is and gets one with its next
    /// message.
    async fn refresh(&mut self) {
        let Some(open) = self.forward.as_mut() else {
            return;
        };
        if open.exited().is_some() {
            return;
        }
        let endpoint = Endpoint::new(open.local(), &self.row.token);
        if let Err(error) = self.push(&endpoint).await {
            tracing::warn!(
                %error,
                session_id = self.row.session_id,
                "could not replace a cloud session's access token before it expires"
            );
            self.publisher.failed(&error);
        }
    }
}

/// The endpoint a host request is answered with.
struct Served {
    endpoint: Endpoint,
    /// Whether the forward was opened for this request, the codespace
    /// daemon bootstrapped or reconnected.
    fresh: bool,
}

async fn serve<B: CloudBackend>(
    backend: &B,
    db: &ChatDb,
    row: &CloudSessionRow,
    publisher: &Publisher,
    forward: &mut Option<B::Forward>,
    wake: bool,
) -> Result<Served, CloudSessionError> {
    if let Some(open) = forward.as_mut() {
        let endpoint = Endpoint::new(open.local(), &row.token);
        if open.exited().is_none() && endpoint.healthy().await {
            return Ok(Served {
                endpoint,
                fresh: false,
            });
        }
        *forward = None;
    }
    let account = CodespacesAccount {
        login: row.account.clone(),
    };
    let provider = backend.provider(&account);
    let id = MachineId::new(row.machine_id.clone());
    let machine = provider
        .machine(&id)
        .await?
        .ok_or_else(|| CloudError::Gone(id.clone()))?;
    bring_up(&provider, &id, &machine, wake, publisher).await?;
    let token = backend.github_token(&account).await?;
    let clone = row
        .clone_repository
        .as_deref()
        .map(str::parse::<GitHubRepository>)
        .transpose()
        .map_err(|error| CloudSessionError::Daemon(format!("records a bad repository: {error}")))?;
    let fronting = FrontingToken::from_stored(row.token.clone());
    Bootstrap {
        build: &backend.build()?,
        home: &CloudHome::for_token(&fronting, row.account.clone()),
        clone: clone.as_ref(),
        github_token: &token,
    }
    .run(&backend.shell(&id, &token), |step| {
        publisher.phase(step.into(), None);
    })
    .await?;
    publisher.phase(CloudSessionPhase::Connecting, None);
    let mut opened = backend.forward(&id, &token)?;
    let endpoint = Endpoint::new(opened.local(), &row.token);
    connected(&mut opened, &endpoint).await?;
    ensure_remote_session(db, row, &endpoint).await?;
    publisher.phase(CloudSessionPhase::Ready, Some(MachineState::Running));
    *forward = Some(opened);
    Ok(Served {
        endpoint,
        fresh: true,
    })
}

/// Pushes the provider of `session_id`'s model, as the codespace daemon
/// knows the model, from `config`: its configuration and, for an OAuth
/// provider, an access token this machine refreshed. The refresh token is
/// never part of it. Answers when the pushed access token is due to be
/// replaced; `None` for a token with no expiry or an API key provider.
async fn push_provider<B: CloudBackend>(
    backend: &B,
    config: &MayConfig,
    session_id: &str,
    endpoint: &Endpoint,
) -> Result<Option<SystemTime>, CloudSessionError> {
    let path = crate::client::session_path(session_id, "");
    let (status, body) = endpoint.exchange(hyper::Method::GET, &path, None).await?;
    if !status.is_success() {
        return Err(CloudSessionError::Daemon(format!(
            "answered {status} for the session"
        )));
    }
    let session: RemoteModel = serde_json::from_slice(&body)
        .map_err(|error| CloudSessionError::Daemon(format!("answered no session: {error}")))?;
    let selector = session
        .model
        .parse::<crate::model_selector::ModelSelector>()
        .map_err(|error| {
            CloudSessionError::Provider(format!("model '{}': {error}", session.model))
        })?;
    let id = selector.provider_id();
    let instance = config.get_provider(id).cloned().ok_or_else(|| {
        CloudSessionError::Provider(format!("'{id}' is not configured on this machine"))
    })?;
    let access = backend
        .access(instance.resolve_type(id))
        .await
        .map_err(|error| CloudSessionError::Provider(error.to_string()))?;
    let refresh_at = access.as_ref().and_then(AccessToken::refresh_at);
    let push = ProviderPush { instance, access };
    let (status, answer) = endpoint
        .exchange(
            hyper::Method::PUT,
            &format!("/api/cloud-home/providers/{id}"),
            Some(serde_json::to_vec(&push).expect("a provider push always serializes")),
        )
        .await?;
    if status.is_success() {
        Ok(refresh_at)
    } else {
        Err(CloudSessionError::Daemon(format!(
            "refused the session's provider: {status} {}",
            String::from_utf8_lossy(&answer).trim()
        )))
    }
}

/// Whether a codespace daemon's session `runtime` has a turn in flight,
/// waiting on the user included.
fn runs(runtime: &serde_json::Value) -> bool {
    runtime
        .get("state")
        .and_then(|state| {
            serde_json::from_value::<crate::session::runtime::SessionRuntimeState>(state.clone())
                .ok()
        })
        .is_some_and(|state| state != crate::session::runtime::SessionRuntimeState::Idle)
}

/// The model of a codespace daemon's session.
#[derive(serde::Deserialize)]
struct RemoteModel {
    model: String,
}

/// What the codespace daemons' own `/api/events` say of their sessions
/// that this daemon's session list shows: each session's title, written to
/// its local row, and its runtime state, held while the events are
/// followed.
#[derive(Clone)]
struct Mirror {
    db: ChatDb,
    bus: UiEventBus,
    runtimes: Arc<DashMap<String, serde_json::Value>>,
}

/// A codespace daemon's session, for the profile it lives in.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteProfile {
    profile_id: Option<String>,
}

/// A codespace daemon's `session_runtimes` payload.
#[derive(serde::Deserialize)]
struct RemoteRuntimes {
    sessions: Vec<RemoteRuntime>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteRuntime {
    session_id: String,
    runtime: serde_json::Value,
}

impl Mirror {
    /// Follows `session_id`'s events on `endpoint` until the stream ends;
    /// its runtime is then no longer known.
    /// Tells `host` each time the session starts or stops running a turn.
    async fn follow(
        self,
        endpoint: Endpoint,
        session_id: String,
        host: async_channel::WeakSender<HostRequest>,
    ) {
        if let Err(error) = self.follow_events(&endpoint, &session_id, &host).await {
            tracing::debug!(%error, session_id, "stopped following a cloud session's events");
        }
        if self.runtimes.remove(&session_id).is_some() {
            self.bus.publish(UiEventTopics::SESSION_RUNTIMES);
        }
    }

    async fn follow_events(
        &self,
        endpoint: &Endpoint,
        session_id: &str,
        host: &async_channel::WeakSender<HostRequest>,
    ) -> Result<(), CloudSessionError> {
        let path = crate::client::session_path(session_id, "");
        let (status, body) = endpoint.exchange(hyper::Method::GET, &path, None).await?;
        if !status.is_success() {
            return Err(CloudSessionError::Daemon(format!(
                "answered {status} for the session"
            )));
        }
        let mut running = false;
        let session: RemoteProfile = serde_json::from_slice(&body)
            .map_err(|error| CloudSessionError::Daemon(format!("answered no session: {error}")))?;
        let profile = session.profile_id.ok_or_else(|| {
            CloudSessionError::Daemon("answered a session in no profile".to_owned())
        })?;
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("profileId", &profile)
            .append_pair("topics", "session_runtimes,profile_sessions")
            .finish();
        let request = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(format!("/api/events?{query}"))
            .body(full(Vec::new()))
            .expect("an events request always builds");
        let response = endpoint.send(request).await?;
        if !response.status().is_success() {
            return Err(CloudSessionError::Daemon(format!(
                "answered {} for its events",
                response.status()
            )));
        }
        let mut stream = BodyDataStream::new(response.into_body());
        let mut parser = crate::client::SseParser::default();
        while let Some(chunk) = stream
            .try_next()
            .await
            .map_err(|error| CloudSessionError::Daemon(format!("cut its events off: {error}")))?
        {
            parser.push(&chunk);
            while let Some(event) = parser.next_event() {
                match event.kind.as_str() {
                    "session_runtimes" => {
                        let now = self.runtime(session_id, &event.data);
                        if now != running {
                            running = now;
                            let Some(host) = host.upgrade() else {
                                return Ok(());
                            };
                            let _ = host.send(HostRequest::Running(running)).await;
                        }
                    }
                    "profile_sessions" => self.title(session_id, &event.data).await,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Holds the runtime `data` reports for `session_id`, and answers
    /// whether the session runs a turn; a session the payload leaves out is
    /// idle, as this daemon's own row says.
    fn runtime(&self, session_id: &str, data: &str) -> bool {
        let runtimes: RemoteRuntimes = match serde_json::from_str(data) {
            Ok(runtimes) => runtimes,
            Err(error) => {
                tracing::warn!(%error, session_id, "a codespace daemon sent unreadable runtimes");
                return self
                    .runtimes
                    .get(session_id)
                    .is_some_and(|held| runs(&held));
            }
        };
        let runtime = runtimes
            .sessions
            .into_iter()
            .find(|item| item.session_id == session_id)
            .map(|item| item.runtime);
        let changed = runtime.map_or_else(
            || self.runtimes.remove(session_id).is_some(),
            |runtime| {
                self.runtimes
                    .insert(session_id.to_owned(), runtime.clone())
                    .is_none_or(|held| held != runtime)
            },
        );
        if changed {
            self.bus.publish(UiEventTopics::SESSION_RUNTIMES);
        }
        self.runtimes
            .get(session_id)
            .is_some_and(|held| runs(&held))
    }

    /// Writes the title `data`, a `profile_sessions` payload, gives
    /// `session_id` to its local row when it differs.
    async fn title(&self, session_id: &str, data: &str) {
        let sessions: serde_json::Value = match serde_json::from_str(data) {
            Ok(sessions) => sessions,
            Err(error) => {
                tracing::warn!(%error, session_id, "a codespace daemon sent unreadable sessions");
                return;
            }
        };
        let Some(title) = title_in(&sessions, session_id) else {
            return;
        };
        match self.db.get_session(session_id).await {
            Ok(Some(local)) if local.title.as_deref() == Some(title) => return,
            Ok(Some(_)) => {}
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, session_id, "failed to read a cloud session's row");
                return;
            }
        }
        if let Err(error) = self
            .db
            .update_session(session_id, Some(title), None, None)
            .await
        {
            tracing::warn!(%error, session_id, "failed to write a cloud session's title");
            return;
        }
        self.bus.publish(UiEventTopics::PROFILE_SESSIONS);
    }
}

/// The title of the session `session_id` anywhere in a profile's sessions:
/// regular, archived, pinned or in a pinned folder.
fn title_in<'a>(value: &'a serde_json::Value, session_id: &str) -> Option<&'a str> {
    match value {
        serde_json::Value::Object(object) => {
            if object.get("id").and_then(serde_json::Value::as_str) == Some(session_id) {
                return object.get("title").and_then(serde_json::Value::as_str);
            }
            object
                .values()
                .find_map(|value| title_in(value, session_id))
        }
        serde_json::Value::Array(items) => {
            items.iter().find_map(|value| title_in(value, session_id))
        }
        _ => None,
    }
}

/// Brings `machine` to running when it is not; only a message starts a
/// suspended one.
async fn bring_up<P: CloudProvider>(
    provider: &P,
    id: &MachineId,
    machine: &CloudMachine,
    wake: bool,
    publisher: &Publisher,
) -> Result<(), CloudSessionError> {
    match machine.state {
        MachineState::Running => Ok(()),
        MachineState::Gone => Err(CloudError::Gone(id.clone()).into()),
        MachineState::Suspended if !wake => Err(CloudSessionError::Suspended),
        MachineState::Suspended | MachineState::Provisioning => {
            until_running(provider, id, RUNNING_WAIT, |seen| {
                publisher.phase(CloudSessionPhase::Starting, Some(seen.state));
            })
            .await?;
            Ok(())
        }
    }
}

/// Waits until the forward answers the codespace daemon's health check.
async fn connected(
    forward: &mut impl Forward,
    endpoint: &Endpoint,
) -> Result<(), CloudSessionError> {
    let began = std::time::Instant::now();
    loop {
        if endpoint.healthy().await {
            return Ok(());
        }
        if let Some(status) = forward.exited() {
            return Err(CloudError::Remote {
                step: "forward the May daemon's port",
                message: format!("gh ended with {status}"),
            }
            .into());
        }
        if began.elapsed() >= CONNECT_DEADLINE {
            return Err(CloudError::Remote {
                step: "forward the May daemon's port",
                message: format!(
                    "the daemon did not answer within {}s",
                    CONNECT_DEADLINE.as_secs()
                ),
            }
            .into());
        }
        async_io::Timer::after(CONNECT_POLL).await;
    }
}

/// The codespace daemon's own session of the same name, created when it
/// has none yet, with the session's model and repository and the
/// codespace's workspace.
async fn ensure_remote_session(
    db: &ChatDb,
    row: &CloudSessionRow,
    endpoint: &Endpoint,
) -> Result<(), CloudSessionError> {
    let path = crate::client::session_path(&row.session_id, "");
    let (status, _) = endpoint.exchange(hyper::Method::GET, &path, None).await?;
    if status.is_success() {
        return Ok(());
    }
    if status != StatusCode::NOT_FOUND {
        return Err(CloudSessionError::Daemon(format!(
            "answered {status} for the session"
        )));
    }
    let session = db.get_session(&row.session_id).await?;
    let body = RemoteSession {
        id: &row.session_id,
        model: session
            .as_ref()
            .and_then(|session| session.model.as_deref()),
        cwd: &row.workspace,
        repo: session.as_ref().and_then(|session| session.repo.as_deref()),
    };
    let (status, answer) = endpoint
        .exchange(
            hyper::Method::POST,
            "/api/sessions",
            Some(serde_json::to_vec(&body).expect("a session request always serializes")),
        )
        .await?;
    if status.is_success() {
        Ok(())
    } else {
        Err(CloudSessionError::Daemon(format!(
            "refused the session: {status} {}",
            String::from_utf8_lossy(&answer).trim()
        )))
    }
}

/// The codespace daemon's `POST /api/sessions` body.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteSession<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    cwd: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<&'a str>,
}

/// The session a path under `/api/sessions/{id}` names, and what follows
/// the id.
fn session_of(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/api/sessions/")?;
    let (id, tail) = rest.find('/').map_or((rest, ""), |end| rest.split_at(end));
    (!id.is_empty()).then_some((id, tail))
}

/// Whether a request to a session is a message, which starts a suspended
/// codespace: a turn, an attachment for one, an action or appended
/// history.
fn is_message(method: &hyper::Method, tail: &str) -> bool {
    method == hyper::Method::POST
        && (tail == "/turns"
            || tail == "/attachments"
            || tail == "/history"
            || tail.starts_with("/actions/"))
}

/// Relays every request under `/api/sessions/{id}` of a cloud session to
/// the codespace daemon. Deleting the session deletes its codespace before
/// the local row goes.
pub struct CloudSessionRelay<B: CloudBackend = GitHubCloud> {
    sessions: Arc<CloudSessions<B>>,
}

impl<B: CloudBackend> CloudSessionRelay<B> {
    /// The relay of `sessions`.
    pub(crate) const fn new(sessions: Arc<CloudSessions<B>>) -> Self {
        Self { sessions }
    }
}

impl<B: CloudBackend> Middleware for CloudSessionRelay<B> {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        let path = request.uri().path().to_owned();
        let Some((session_id, tail)) = session_of(&path) else {
            return next.run(request).await;
        };
        let Some(host) = self.sessions.host(session_id) else {
            return next.run(request).await;
        };
        if tail.is_empty() && request.method() == hyper::Method::DELETE {
            self.sessions
                .delete(session_id)
                .await
                .map_err(CloudSessionError::into_http)?;
            return next.run(request).await;
        }
        let wake = is_message(request.method(), tail);
        let endpoint = match host.endpoint(wake).await {
            Ok(endpoint) => endpoint,
            // Clients show a suspended session as waiting for a message,
            // not as a failure.
            Err(error @ CloudSessionError::Suspended) => {
                return Ok(super::error::typed_error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    super::types::ApiErrorCode::SessionSuspended,
                    error,
                ));
            }
            Err(error) => return Err(error.into_http()),
        };
        let describes_session = tail.is_empty() && request.method() == hyper::Method::GET;
        if describes_session {
            // The answer is read to add the location: it comes uncompressed.
            request.headers_mut().remove(hyper::header::ACCEPT_ENCODING);
        }
        let response = relay(request, &endpoint)
            .await
            .map_err(CloudSessionError::into_http)?;
        if describes_session && response.status().is_success() {
            return with_location(response, &host.location()).await;
        }
        Ok(response)
    }
}

/// Sends `request` to `endpoint`, streaming both bodies.
async fn relay(request: &mut Request, endpoint: &Endpoint) -> Result<Response, CloudSessionError> {
    let body = std::mem::replace(request.body_mut(), skyzen::Body::empty());
    let mut builder = hyper::Request::builder()
        .method(request.method().clone())
        .uri(
            request
                .uri()
                .path_and_query()
                .map_or("/", hyper::http::uri::PathAndQuery::as_str),
        );
    for (name, value) in request.headers() {
        builder = builder.header(name, value);
    }
    let stream = StreamBody::new(body.map_ok(Frame::data));
    let relayed = builder
        .body(stream.boxed())
        .map_err(|error| CloudSessionError::Daemon(format!("cannot take the request: {error}")))?;
    let response = endpoint.send(relayed).await?;
    Ok(response.map(|body| {
        skyzen::Body::from_stream(
            BodyDataStream::new(body).map_err(|error| BodyError::Other(Box::new(error))),
        )
    }))
}

/// `response`, a session's JSON, with the session's location added.
async fn with_location(response: Response, location: &SessionLocation) -> Result<Response, Error> {
    let (mut parts, body) = response.into_parts();
    let bytes = body
        .into_bytes()
        .await
        .map_err(|error| Error::msg(error.to_string()).set_status(StatusCode::BAD_GATEWAY))?;
    let mut session: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        Error::msg(format!(
            "the codespace's May daemon answered no session: {error}"
        ))
        .set_status(StatusCode::BAD_GATEWAY)
    })?;
    session["location"] =
        serde_json::to_value(location).expect("a session location always serializes");
    let bytes = serde_json::to_vec(&session).expect("a session always serializes");
    parts.headers.remove(hyper::header::CONTENT_LENGTH);
    Ok(Response::from_parts(parts, skyzen::Body::from(bytes)))
}

#[cfg(test)]
#[path = "cloud_sessions_tests.rs"]
mod tests;
