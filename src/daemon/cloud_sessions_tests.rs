//! A cloud session against a recorded GitHub and a real local daemon as
//! its codespace daemon: the codespace's states come from GitHub's
//! documented codespace, the bootstrap's commands from a recorded machine,
//! and the forward points at a daemon of this build serving its remote
//! listener behind the fronting token, as a cloud home does.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::Digest as _;

use aither_codex::auth::CodexCredentials;
use arc_swap::ArcSwap;

use super::{CloudBackend, CloudSessionError, CloudSessions, Endpoint, Forward};
use crate::cloud::access::{AccessError, AccessToken};
use crate::cloud::bootstrap::{MayBuild, RemoteShell};
use crate::cloud::codespaces::{Codespaces, CodespacesAccount};
use crate::cloud::{CloudError, MachineId, MachineState};
use crate::config::{MayConfig, ProviderInstance, ProviderType};
use crate::daemon::auth::{FrontingTokenIdentitySource, RequireIdentity};
use crate::daemon::server::{DaemonMode, build_router};
use crate::daemon::state::DaemonState;
use crate::daemon::types::{CloudSessionEvent, CloudSessionPhase};
use crate::db::{ChatDb, CloudSessionRow};
use crate::issue::github::GitHubRest;
use crate::model_selector::ModelSelector;
use crate::pr::PrTokenSource;
use crate::test_support::{ApiReply, serve_api};
use crate::ui_event_bus::{UiEventBus, UiEventTopics};

/// GitHub's documented codespace.
const CODESPACE: &str = include_str!("../cloud/fixtures/codespace.json");
const CODESPACE_NAME: &str = "monalisa-octocat-hello-world-g4wpq6h95q";
const SESSION: &str = "quiet-otter";
const TOKEN: &str = "fronting-token";
const VERSION: &str = "0.1.0";
const PROVIDER: &str = "chatgpt";
/// The refresh token of this machine's `ChatGPT` sign-in, which never
/// leaves it.
const REFRESH_TOKEN: &str = "refresh-token-that-stays-here";

/// A codespace daemon of this build: its remote listener on loopback,
/// admitting only the fronting token, and the directory it keeps its files
/// in.
async fn codespace_daemon() -> (SocketAddr, Arc<DaemonState>, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let state = Arc::new(
        DaemonState::for_test(dir.path(), MayConfig::default())
            .await
            .expect("daemon state"),
    );
    let home = dir.keep();
    state
        .db()
        .create_profile(crate::db::CreateProfileParams {
            id: &state.default_profile_id,
            name: "Test",
            environment: crate::db::ProfileEnvironmentKind::Local,
            slug: None,
            icon: None,
            persona: None,
            preferences: None,
            location_aware: false,
        })
        .await
        .expect("profile");
    let digest: [u8; 32] = sha2::Sha256::digest(TOKEN.as_bytes()).into();
    let router = build_router(
        state.clone(),
        DaemonMode::Rest,
        RequireIdentity::new(vec![Box::new(FrontingTokenIdentitySource::new(
            digest,
            "octocat".into(),
        ))]),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("address");
    tokio::spawn(crate::daemon::serve::serve_tcp(listener, Arc::new(router)));
    (addr, state, home)
}

/// The documented codespace in `state`.
fn codespace(state: &str) -> String {
    let mut codespace: Value = serde_json::from_str(CODESPACE).expect("codespace fixture");
    codespace["state"] = json!(state);
    codespace.to_string()
}

/// A GitHub whose codespace reads as the states queued in `reads`, the
/// last one repeating, and answers a start as `Starting`.
struct RecordedGitHub {
    base: String,
    reads: Arc<Mutex<VecDeque<&'static str>>>,
    starts: Arc<Mutex<usize>>,
}

impl RecordedGitHub {
    async fn serve() -> Self {
        let reads: Arc<Mutex<VecDeque<&'static str>>> = Arc::default();
        let starts: Arc<Mutex<usize>> = Arc::default();
        let (served_reads, served_starts) = (reads.clone(), starts.clone());
        let codespace_path = format!("/user/codespaces/{CODESPACE_NAME}");
        let start_path = format!("{codespace_path}/start");
        let (base, _) = serve_api(move |request, _| {
            if request.method == "GET" && request.path == codespace_path {
                let mut reads = served_reads.lock().expect("reads");
                let state = if reads.len() > 1 {
                    reads.pop_front().expect("a state")
                } else {
                    *reads.front().expect("a state")
                };
                ApiReply::json(200, codespace(state))
            } else if request.method == "POST" && request.path == start_path {
                *served_starts.lock().expect("starts") += 1;
                ApiReply::json(200, codespace("Starting"))
            } else {
                ApiReply::json(500, r#"{"message":"off the script"}"#)
            }
        })
        .await;
        Self {
            base,
            reads,
            starts,
        }
    }

    fn reads_as(&self, states: &[&'static str]) {
        *self.reads.lock().expect("reads") = states.iter().copied().collect();
    }

    fn starts(&self) -> usize {
        *self.starts.lock().expect("starts")
    }
}

/// A machine whose `installed` record is this build, so a bootstrap probes,
/// writes the record and starts the daemon, recording each command.
#[derive(Clone, Default)]
struct RecordedShell {
    steps: Arc<Mutex<Vec<&'static str>>>,
}

impl RemoteShell for RecordedShell {
    fn run(
        &self,
        step: &'static str,
        _script: &str,
        _input: Option<Vec<u8>>,
        _deadline: Duration,
    ) -> impl Future<Output = Result<String, CloudError>> + Send {
        self.steps.lock().expect("steps").push(step);
        std::future::ready(Ok(format!("release:{VERSION}")))
    }
}

/// A forward straight to the codespace daemon, ended when `ended` is set:
/// a suspended codespace takes its forward down.
struct DirectForward {
    addr: SocketAddr,
    ended: Arc<AtomicBool>,
}

impl Forward for DirectForward {
    fn local(&self) -> SocketAddr {
        self.addr
    }

    fn exited(&mut self) -> Option<String> {
        self.ended
            .load(Ordering::SeqCst)
            .then(|| "the codespace stopped".to_owned())
    }
}

struct TestCloud {
    github: String,
    daemon: SocketAddr,
    shell: RecordedShell,
    forward_ended: Arc<AtomicBool>,
    /// This machine's `ChatGPT` sign-in, refresh token included.
    chatgpt: Arc<Mutex<CodexCredentials>>,
}

impl TestCloud {
    fn new(github: &RecordedGitHub, daemon: SocketAddr) -> Self {
        Self {
            github: github.base.clone(),
            daemon,
            shell: RecordedShell::default(),
            forward_ended: Arc::default(),
            chatgpt: Arc::new(Mutex::new(signed_in("access-1"))),
        }
    }
}

/// A `ChatGPT` sign-in whose access token is `access`.
fn signed_in(access: &str) -> CodexCredentials {
    CodexCredentials {
        access_token: access.into(),
        refresh_token: REFRESH_TOKEN.into(),
        id_token: None,
        account_id: Some("account-日本語-한국어-中文".into()),
        residency: None,
        expires_at: Some(4_102_444_800),
        earliest_refresh_at: None,
    }
}

impl CloudBackend for TestCloud {
    type Provider = Codespaces<PrTokenSource>;
    type Shell = RecordedShell;
    type Forward = DirectForward;

    fn provider(&self, account: &CodespacesAccount) -> Self::Provider {
        Codespaces::new(
            GitHubRest::with_api_base(PrTokenSource::Fixed("account-token".into()), &self.github),
            account.clone(),
        )
    }

    fn github_token(
        &self,
        _account: &CodespacesAccount,
    ) -> impl Future<Output = Result<String, CloudError>> + Send {
        std::future::ready(Ok("account-token".into()))
    }

    fn shell(&self, _machine: &MachineId, _token: &str) -> Self::Shell {
        self.shell.clone()
    }

    fn forward(&self, _machine: &MachineId, _token: &str) -> Result<Self::Forward, CloudError> {
        self.forward_ended.store(false, Ordering::SeqCst);
        Ok(DirectForward {
            addr: self.daemon,
            ended: self.forward_ended.clone(),
        })
    }

    fn build(&self) -> Result<MayBuild, CloudError> {
        Ok(MayBuild::Release {
            version: VERSION.into(),
        })
    }

    fn access(
        &self,
        provider_type: ProviderType,
    ) -> impl Future<Output = Result<Option<AccessToken>, AccessError>> + Send {
        let credentials = self.chatgpt.lock().expect("sign-in");
        std::future::ready(Ok(
            (provider_type == ProviderType::Chatgpt).then(|| AccessToken::of_codex(&credentials))
        ))
    }
}

/// The fronting side: its database holding the session and its record.
async fn fronting(backend: TestCloud) -> Arc<CloudSessions<TestCloud>> {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = ChatDb::open(&dir.path().join("history.db"))
        .await
        .expect("database");
    std::mem::forget(dir);
    db.create_session_in_profile(SESSION, Some("日本語 한국어 中文"), Some(&model()), None)
        .await
        .expect("session");
    db.insert_cloud_session(&CloudSessionRow {
        session_id: SESSION.into(),
        account: "octocat".into(),
        machine_id: CODESPACE_NAME.into(),
        machine_type: "standardLinux".into(),
        token: TOKEN.into(),
        clone_repository: None,
        workspace: "/workspaces/Hello-World".into(),
    })
    .await
    .expect("record");
    let mut config = MayConfig::default();
    config.provider.insert(
        PROVIDER.into(),
        ProviderInstance::new(ProviderType::Chatgpt),
    );
    Arc::new(
        CloudSessions::load(
            db,
            backend,
            Arc::new(ArcSwap::from_pointee(config)),
            UiEventBus::new(),
        )
        .await
        .expect("loaded"),
    )
}

/// The session's model, on this machine's `ChatGPT` sign-in.
fn model() -> String {
    ModelSelector::new(PROVIDER, "gpt-5").to_string()
}

/// Every event published so far.
fn drain(
    events: &mut tokio::sync::broadcast::Receiver<CloudSessionEvent>,
) -> Vec<CloudSessionEvent> {
    std::iter::from_fn(|| events.try_recv().ok()).collect()
}

fn phases(events: &[CloudSessionEvent]) -> Vec<CloudSessionPhase> {
    events.iter().map(|event| event.phase).collect()
}

#[tokio::test]
async fn a_cloud_session_is_served_through_suspension_and_resumption() {
    let github = RecordedGitHub::serve().await;
    let (daemon, remote, _) = codespace_daemon().await;
    let backend = TestCloud::new(&github, daemon);
    let (shell, forward_ended) = (backend.shell.clone(), backend.forward_ended.clone());
    let sessions = fronting(backend).await;
    let host = sessions.host(SESSION).expect("a cloud session");
    let mut events = sessions.subscribe();

    // Suspended: a read does not start it.
    github.reads_as(&["Shutdown"]);
    let suspended = host.endpoint(false).await.expect_err("suspended");
    assert!(
        matches!(suspended, CloudSessionError::Suspended),
        "{suspended:?}"
    );
    assert_eq!(github.starts(), 0);

    // A message starts it, bootstraps it and connects.
    github.reads_as(&["Shutdown", "Shutdown", "Starting", "Available"]);
    let endpoint = host.endpoint(true).await.expect("served");
    assert_eq!(github.starts(), 1);
    let seen = drain(&mut events);
    assert_eq!(
        phases(&seen),
        [
            // Suspended, the start's answer, Starting, Available.
            CloudSessionPhase::Starting,
            CloudSessionPhase::Starting,
            CloudSessionPhase::Starting,
            CloudSessionPhase::Starting,
            CloudSessionPhase::Checking,
            CloudSessionPhase::StartingDaemon,
            CloudSessionPhase::Connecting,
            CloudSessionPhase::Ready,
        ]
    );
    assert_eq!(seen[0].machine_state, Some(MachineState::Suspended));
    assert!(seen.iter().all(|event| event.session_id == SESSION));
    assert_eq!(
        *shell.steps.lock().expect("steps"),
        [
            "check the installed May",
            "write the cloud home record",
            "start the May daemon",
        ]
    );

    // The codespace daemon holds the session under the same name.
    let remote_session = remote
        .db()
        .get_session(SESSION)
        .await
        .expect("read")
        .expect("the session on the codespace");
    assert_eq!(remote_session.model, Some(model()));
    assert_eq!(
        remote_session.cwd.as_deref(),
        Some("/workspaces/Hello-World")
    );

    // While the forward stands, the next request reuses it.
    let again = host.endpoint(false).await.expect("served");
    assert_eq!(again.addr, endpoint.addr);
    assert_eq!(drain(&mut events), []);

    // GitHub suspends it: the forward ends, a read waits for a message.
    forward_ended.store(true, Ordering::SeqCst);
    github.reads_as(&["Shutdown"]);
    assert!(matches!(
        host.endpoint(false).await,
        Err(CloudSessionError::Suspended)
    ));
    github.reads_as(&["Shutdown", "Shutdown", "Available"]);
    host.endpoint(true).await.expect("resumed");
    assert_eq!(github.starts(), 2);
    assert_eq!(
        phases(&drain(&mut events)).last(),
        Some(&CloudSessionPhase::Ready)
    );
}

#[tokio::test]
async fn a_codespace_github_no_longer_has_fails_the_session_visibly() {
    let github = RecordedGitHub::serve().await;
    let (daemon, _remote, _) = codespace_daemon().await;
    let sessions = fronting(TestCloud::new(&github, daemon)).await;
    let mut events = sessions.subscribe();
    github.reads_as(&["Deleted"]);

    let error = sessions
        .host(SESSION)
        .expect("a cloud session")
        .endpoint(true)
        .await
        .expect_err("gone");

    assert!(
        matches!(error, CloudSessionError::Cloud(CloudError::Gone(_))),
        "{error:?}"
    );
    let failed = drain(&mut events);
    assert_eq!(phases(&failed), [CloudSessionPhase::Failed]);
    assert!(failed[0].error.is_some());
}

#[tokio::test]
async fn the_relay_carries_the_fronting_token_and_streams_the_answer() {
    let (daemon, _remote, _) = codespace_daemon().await;
    let request = |uri: &str| {
        hyper::Request::builder()
            .method("GET")
            .uri(uri)
            .header("Host", "127.0.0.1:8787")
            .header("Origin", "http://127.0.0.1:8787")
            .header("Tailscale-User-Login", "someone@example.com")
            .body(skyzen::Body::empty())
            .expect("request")
    };

    let mut health = request("/api/health");
    let answered = super::relay(&mut health, &Endpoint::new(daemon, TOKEN))
        .await
        .expect("relayed");
    assert_eq!(answered.status(), skyzen::StatusCode::OK);

    let mut refused = request("/api/health");
    let refused = super::relay(&mut refused, &Endpoint::new(daemon, "another-token"))
        .await
        .expect("relayed");
    assert_eq!(refused.status(), skyzen::StatusCode::UNAUTHORIZED);
}

/// The access token the codespace daemon's `chatgpt` provider holds.
fn pushed(remote: &DaemonState) -> crate::cloud::access::AccessCell {
    remote
        .config_snapshot()
        .provider
        .get(PROVIDER)
        .expect("the pushed provider")
        .pushed_access
        .clone()
        .expect("an access token")
}

/// Every file under `dir` holding `needle`.
fn files_holding(dir: &std::path::Path, needle: &str) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read dir").flatten() {
            let path = entry.path();
            let kind = entry.file_type().expect("file type");
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && std::fs::read(&path).is_ok_and(|bytes| {
                    bytes
                        .windows(needle.len())
                        .any(|window| window == needle.as_bytes())
                })
            {
                found.push(path);
            }
        }
    }
    found
}

#[tokio::test]
async fn the_codespace_gets_an_access_token_and_never_the_refresh_token() {
    let github = RecordedGitHub::serve().await;
    let (daemon, remote, remote_home) = codespace_daemon().await;
    let backend = TestCloud::new(&github, daemon);
    let chatgpt = backend.chatgpt.clone();
    let sessions = fronting(backend).await;
    let host = sessions.host(SESSION).expect("a cloud session");
    github.reads_as(&["Available"]);

    // Bootstrap pushes the session model's provider with its access token.
    host.endpoint(true).await.expect("served");
    let cell = pushed(&remote);
    assert_eq!(
        *cell.current(),
        AccessToken::of_codex(&signed_in("access-1"))
    );

    // This machine refreshed: the next message pushes the new access token
    // into the cell the codespace's models already read.
    *chatgpt.lock().expect("sign-in") = signed_in("access-2");
    host.endpoint(true).await.expect("served");
    assert_eq!(
        *cell.current(),
        AccessToken::of_codex(&signed_in("access-2"))
    );

    // A read pushes nothing.
    *chatgpt.lock().expect("sign-in") = signed_in("access-3");
    host.endpoint(false).await.expect("served");
    assert_eq!(
        *cell.current(),
        AccessToken::of_codex(&signed_in("access-2"))
    );

    // The refresh token reached nothing the codespace daemon keeps: not its
    // configuration as served or saved, and no file it wrote.
    let config = serde_json::to_string(&*remote.config_snapshot()).expect("config");
    assert!(!config.contains(REFRESH_TOKEN), "{config}");
    assert!(!config.contains("access-2"), "{config}");
    assert_eq!(
        files_holding(&remote_home, REFRESH_TOKEN),
        Vec::<std::path::PathBuf>::new()
    );
}

#[tokio::test]
async fn only_the_fronting_daemon_pushes_a_provider() {
    let (daemon, remote, _) = codespace_daemon().await;
    let push = serde_json::to_vec(&crate::cloud::access::ProviderPush {
        instance: ProviderInstance::new(ProviderType::Chatgpt),
        access: Some(AccessToken::of_codex(&signed_in("forged"))),
    })
    .expect("push");
    let path = format!("/api/cloud-home/providers/{PROVIDER}");

    // Through the remote listener without the fronting token.
    let (status, _) = Endpoint::new(daemon, "another-token")
        .exchange(hyper::Method::PUT, &path, Some(push.clone()))
        .await
        .expect("answered");
    assert_eq!(status, skyzen::StatusCode::UNAUTHORIZED);

    // On the loopback listener and the unix socket, which trust the
    // machine's own user but are no fronting daemon.
    for router in [
        build_router(
            remote.clone(),
            DaemonMode::Rest,
            crate::daemon::auth::LoopbackOnly,
        ),
        build_router(
            remote.clone(),
            DaemonMode::Rest,
            crate::daemon::auth::SocketOwner,
        ),
    ] {
        let request = hyper::Request::builder()
            .method("PUT")
            .uri(&path)
            .header("Host", "127.0.0.1:8787")
            .header("Content-Type", "application/json")
            .body(skyzen::Body::from(push.clone()))
            .expect("request");
        let status = match router.go(request).await {
            Ok(response) => response.status(),
            Err(error) => skyzen::error_response(&*error).status(),
        };
        assert_eq!(status, skyzen::StatusCode::FORBIDDEN);
    }
    assert!(!remote.config_snapshot().provider.contains_key(PROVIDER));

    // The fronting daemon's push is taken.
    let (status, _) = Endpoint::new(daemon, TOKEN)
        .exchange(hyper::Method::PUT, &path, Some(push))
        .await
        .expect("answered");
    assert_eq!(status, skyzen::StatusCode::NO_CONTENT);
    assert_eq!(
        *pushed(&remote).current(),
        AccessToken::of_codex(&signed_in("forged"))
    );
}

/// Waits up to five seconds for `check` to hold.
async fn eventually<F: Future<Output = bool>>(what: &str, check: impl Fn() -> F) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !check().await {
        assert!(
            std::time::Instant::now() < deadline,
            "{what} did not happen"
        );
        async_io::Timer::after(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn the_local_session_list_follows_the_codespace_sessions_title() {
    let github = RecordedGitHub::serve().await;
    let (daemon, remote, _) = codespace_daemon().await;
    let sessions = fronting(TestCloud::new(&github, daemon)).await;
    github.reads_as(&["Available"]);
    sessions
        .host(SESSION)
        .expect("a cloud session")
        .endpoint(true)
        .await
        .expect("served");

    // The codespace daemon names the session, as its title worker does.
    let title = "東京の天気 · 서울 날씨 · 北京天气";
    remote
        .db()
        .update_session(SESSION, Some(title), None, None)
        .await
        .expect("titled");
    remote.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);

    let local = sessions.mirror.db.clone();
    eventually("the local row taking the title", || {
        let local = local.clone();
        async move {
            local
                .get_session(SESSION)
                .await
                .expect("read")
                .and_then(|session| session.title)
                .as_deref()
                == Some(title)
        }
    })
    .await;
}

#[tokio::test]
async fn a_cloud_sessions_runtime_is_the_codespaces_while_it_is_followed() {
    let github = RecordedGitHub::serve().await;
    let (daemon, _remote, _) = codespace_daemon().await;
    let sessions = fronting(TestCloud::new(&github, daemon)).await;
    let mut topics = sessions.mirror.bus.subscribe();
    let running = json!({
        "sessions": [
            {"sessionId": "another", "runtime": {"state": "idle"}},
            {"sessionId": SESSION, "runtime": {"state": "running", "queuedCount": 1}},
        ]
    });

    sessions.mirror.runtime(SESSION, &running.to_string());
    assert_eq!(
        sessions.mirrored_runtime(SESSION),
        Some(json!({"state": "running", "queuedCount": 1}))
    );
    assert!(
        topics
            .try_recv()
            .expect("published")
            .contains(UiEventTopics::SESSION_RUNTIMES)
    );

    // The same report again changes nothing.
    sessions.mirror.runtime(SESSION, &running.to_string());
    assert!(topics.try_recv().is_err());

    // Left out of the report: idle, as the local row says.
    sessions
        .mirror
        .runtime(SESSION, &json!({"sessions": []}).to_string());
    assert_eq!(sessions.mirrored_runtime(SESSION), None);
}

#[test]
fn a_client_reads_the_suspended_answer_as_waiting_for_a_message() {
    let answer = |status, code: &str| crate::client::DaemonClientError::Status {
        path: "/api/sessions/quiet-otter".into(),
        status,
        message: json!({"code": code, "error": "suspended"}).to_string(),
    };
    let unavailable = hyper::StatusCode::SERVICE_UNAVAILABLE;
    assert!(answer(unavailable, "session_suspended").is_suspended());
    assert!(!answer(unavailable, "internal_error").is_suspended());
    assert!(!answer(hyper::StatusCode::BAD_GATEWAY, "session_suspended").is_suspended());
}

#[test]
fn a_title_is_found_wherever_the_profile_lists_the_session() {
    let listed = json!({
        "profileSessions": {
            "pinned": [
                {"type": "folder", "sessions": [{"id": "quiet-otter", "title": "日本語 한국어 中文"}]},
            ],
            "regular": [{"id": "another", "title": "Another"}],
            "archived": [],
        }
    });
    assert_eq!(
        super::title_in(&listed, "quiet-otter"),
        Some("日本語 한국어 中文")
    );
    assert_eq!(super::title_in(&listed, "missing"), None);
}

#[test]
fn only_a_message_starts_a_suspended_codespace() {
    use hyper::Method;

    for (method, tail, wakes) in [
        (Method::POST, "/turns", true),
        (Method::POST, "/attachments", true),
        (Method::POST, "/actions/compact", true),
        (Method::POST, "/history", true),
        (Method::GET, "/transcript", false),
        (Method::GET, "/events", false),
        (Method::DELETE, "/turns/current", false),
        (Method::GET, "", false),
    ] {
        assert_eq!(super::is_message(&method, tail), wakes, "{method} {tail}");
    }
    assert_eq!(
        super::session_of("/api/sessions/quiet-otter/turns"),
        Some(("quiet-otter", "/turns"))
    );
    assert_eq!(
        super::session_of("/api/sessions/quiet-otter"),
        Some(("quiet-otter", ""))
    );
    assert_eq!(super::session_of("/api/sessions"), None);
    assert_eq!(super::session_of("/api/profiles/test"), None);
}

#[test]
fn a_display_name_stays_within_githubs_limit() {
    assert_eq!(super::display_name("quiet-otter"), "May quiet-otter");
    let long = "日本語한국어中文".repeat(8);
    let name = super::display_name(&long);
    assert!(name.len() <= 48, "{name}");
    assert!(long.starts_with(name.trim_start_matches("May ")));
}
