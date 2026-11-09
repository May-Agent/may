//! Every popup that waits on the user announces itself once, and each
//! announcement becomes one notification: pushed to subscribed browsers and
//! streamed to attached terminals.

use std::sync::Arc;
use std::time::Duration;

use aither_agent::ask_user::{AskUserArgs, AskUserRequest, AskUserRequestPayload};
use aither_agent::workspace_request::RequestWorkspaceArgs;
use aither_core::llm::{Tool, ToolContext};
use aither_sandbox::permission::PermissionHandler;
use aither_sandbox::{ScriptLanguage, TerminalMode};
use base64::Engine;
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast::{Receiver, error::TryRecvError};

use super::state::{AskUserApprover, DaemonState, WorkspaceApprover};
use crate::agent::ask_user_tool::{AskUserArgs as ToolArgs, AskUserTool};
use crate::agent::{OnTimeout, PermissionApprover, PermissionRequestMode};
use crate::config::MayConfig;
use crate::db::ChatDb;
use crate::session::{SessionRuntimeService, UserAsk, UserWait};
use crate::ui_event_bus::UiEventBus;

const WAIT_BOUND: Duration = Duration::from_secs(20);

struct Fixture {
    _dir: tempfile::TempDir,
    runtime: Arc<SessionRuntimeService>,
    waits: Receiver<UserWait>,
}

impl Fixture {
    async fn new() -> Self {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let dir = tempfile::tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("open database");
        db.create_session("session", Some("Session"), Some("test-model"))
            .await
            .expect("create session");
        let runtime = Arc::new(SessionRuntimeService::new(
            db,
            UiEventBus::new(),
            crate::session::MayHomePaths::new(dir.path().join("home")),
        ));
        let waits = runtime.subscribe_user_waits();
        Self {
            _dir: dir,
            runtime,
            waits,
        }
    }

    fn approver(&self) -> PermissionApprover {
        PermissionApprover::new()
            .with_runtime_service(self.runtime.clone())
            .with_approval_timeout(Duration::from_millis(200))
            .for_session("session")
    }

    /// The one announcement the popup made.
    async fn announced(&mut self) -> UserWait {
        let wait = tokio::time::timeout(WAIT_BOUND, self.waits.recv())
            .await
            .expect("the popup announced itself")
            .expect("announcement");
        assert!(
            matches!(self.waits.try_recv(), Err(TryRecvError::Empty)),
            "a popup announces itself once"
        );
        wait
    }
}

#[tokio::test]
async fn an_unsafe_command_approval_announces_the_command() {
    let mut fixture = Fixture::new().await;
    let approver = fixture.approver();
    let _ = approver
        .check(TerminalMode::Unsafe, "make install", ScriptLanguage::Shell)
        .await;

    assert_eq!(
        fixture.announced().await,
        UserWait {
            session_id: "session".to_owned(),
            ask: UserAsk::Command {
                mode: PermissionRequestMode::Unsafe,
                script: "make install".to_owned(),
            },
        }
    );
}

#[tokio::test]
async fn a_publish_approval_announces_what_is_published() {
    let mut fixture = Fixture::new().await;
    let approver = fixture.approver();
    let _ = approver
        .request_publication(
            "bug issue on May-Agent/may\n\nTitle: Crash\n\nbody".to_owned(),
            OnTimeout::Deny,
        )
        .await;

    assert_eq!(
        fixture.announced().await,
        UserWait {
            session_id: "session".to_owned(),
            ask: UserAsk::Publication {
                headline: "bug issue on May-Agent/may".to_owned(),
            },
        }
    );
}

#[tokio::test]
async fn a_workspace_request_announces_the_directory() {
    let mut fixture = Fixture::new().await;
    let may_dir = tempfile::tempdir().expect("may dir");
    let approver = WorkspaceApprover::new()
        .with_runtime_service(fixture.runtime.clone())
        .with_may_dir(may_dir.path().to_path_buf());
    let request = tokio::spawn(async move {
        approver
            .request_and_wait(
                "session",
                RequestWorkspaceArgs {
                    path: "/srv/dataset".to_owned(),
                    reason: "read the dataset".to_owned(),
                },
            )
            .await
    });

    assert_eq!(
        fixture.announced().await,
        UserWait {
            session_id: "session".to_owned(),
            ask: UserAsk::Workspace {
                path: "/srv/dataset".to_owned(),
            },
        }
    );
    request.abort();
}

#[tokio::test]
async fn a_prompt_a_run_blocks_on_announces_its_question() {
    let mut fixture = Fixture::new().await;
    let approver = AskUserApprover::new().with_runtime_service(fixture.runtime.clone());
    let (tx, _rx) = async_channel::bounded(1);
    let request = AskUserRequest::new(
        AskUserRequestPayload {
            session_id: "session".to_owned(),
            request: AskUserArgs {
                question: Some("Proceed?".to_owned()),
                option: vec!["yes".to_owned(), "no".to_owned()],
                multi_select: false,
                questions: Vec::new(),
            },
        },
        tx,
    );
    let waiting = tokio::spawn(async move {
        approver.enqueue(request, Some("session".to_owned())).await;
    });

    assert_eq!(
        fixture.announced().await,
        UserWait {
            session_id: "session".to_owned(),
            ask: UserAsk::Question {
                text: "Proceed?".to_owned(),
            },
        }
    );
    waiting.abort();
}

#[tokio::test]
async fn an_open_question_announces_itself() {
    let mut fixture = Fixture::new().await;
    let tool = AskUserTool::new(fixture.runtime.clone(), "session");
    let args: ToolArgs = serde_json::from_value(serde_json::json!({
        "question": "Which database?",
        "option": ["sqlite", "postgres"]
    }))
    .expect("ask_user arguments");
    tool.call(args, ToolContext::default())
        .await
        .expect("question opens");

    assert_eq!(
        fixture.announced().await,
        UserWait {
            session_id: "session".to_owned(),
            ask: UserAsk::Question {
                text: "Which database?".to_owned(),
            },
        }
    );
}

/// A push service that accepts one message and hands back its headers.
async fn push_service() -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}/push", listener.local_addr().expect("addr"));
    let received = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).await.expect("read");
            assert_ne!(read, 0, "the request ended before its headers");
            buffer.extend_from_slice(&chunk[..read]);
        }
        stream
            .write_all(b"HTTP/1.1 201 Created\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await
            .expect("respond");
        String::from_utf8_lossy(&buffer).to_lowercase()
    });
    (endpoint, received)
}

#[tokio::test]
async fn a_popup_notifies_the_subscribed_browser_and_the_attached_terminal() {
    let home = tempfile::tempdir().expect("home");
    let state = Arc::new(
        DaemonState::for_test(home.path(), MayConfig::default())
            .await
            .expect("daemon state"),
    );
    state
        .db()
        .create_session("session", Some("Fix the build"), Some("m"))
        .await
        .expect("session row");
    let (endpoint, received) = push_service().await;
    let browser_key = p256::SecretKey::random(&mut OsRng);
    let p256dh = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(browser_key.public_key().to_sec1_bytes());
    let mut auth = [0_u8; 16];
    OsRng.fill_bytes(&mut auth);
    state
        .notification_service
        .subscribe(
            &endpoint,
            &p256dh,
            &base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(auth),
        )
        .await
        .expect("subscription");
    let mut terminal = state.notification_service.subscribe_user_notifications();
    super::user_wait::spawn_notifier(&state);

    state.runtime_service.announce_user_wait(UserWait {
        session_id: "session".to_owned(),
        ask: UserAsk::permission(PermissionRequestMode::Unsafe, "make install"),
    });

    let notification = tokio::time::timeout(WAIT_BOUND, terminal.recv())
        .await
        .expect("the terminal is notified")
        .expect("notification");
    assert_eq!(notification.session_id, "session");
    assert_eq!(notification.title, "Fix the build");
    assert_eq!(notification.body, "Approve running: make install");

    let request = tokio::time::timeout(WAIT_BOUND, received)
        .await
        .expect("the browser is pushed")
        .expect("push service");
    assert!(request.starts_with("post /push "), "{request}");
    assert!(request.contains("content-encoding: aes128gcm"), "{request}");
    assert!(request.contains("authorization: vapid"), "{request}");

    let mailbox = tokio::time::timeout(WAIT_BOUND, async {
        loop {
            let entries = state
                .db()
                .list_notification_mailbox_entries(10)
                .await
                .expect("mailbox");
            if entries.iter().any(|entry| entry.status == "delivered") {
                return entries;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the mailbox records the delivery");
    assert_eq!(mailbox.len(), 1);
    assert_eq!(mailbox[0].session_id.as_deref(), Some("session"));
    assert_eq!(mailbox[0].title, "Fix the build");
    assert_eq!(mailbox[0].sent_count, 1);
}
