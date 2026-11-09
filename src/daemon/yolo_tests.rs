//! A session's yolo switch: while it is on, every approval request the
//! session raises is granted at once and recorded like a user approval, a
//! scheduled task it creates is stored approved and enabled, and turning it
//! off makes the next request ask again. A session without yolo is unchanged.

use std::sync::Arc;
use std::time::Duration;

use aither_agent::workspace_request::RequestWorkspaceArgs;
use aither_sandbox::permission::{PermissionError, PermissionHandler};
use aither_sandbox::{CommandRole, ScriptLanguage, SshServer, TerminalMode};
use tempfile::TempDir;

use super::state::WorkspaceApprover;
use crate::agent::PermissionApprover;
use crate::agent::ssh_authorizer::MaySshAuthorizer;
use crate::db::{ChatDb, CreateScheduledTaskParams};
use crate::session::SessionRuntimeService;
use crate::task_trigger::PersistedTaskTrigger;
use crate::terminal_permission_handler::TerminalPermissionHandler;
use crate::ui_event_bus::UiEventBus;

struct Fixture {
    _dir: TempDir,
    runtime: Arc<SessionRuntimeService>,
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
        let home = crate::session::MayHomePaths::new(dir.path().join("home"));
        Self {
            _dir: dir,
            runtime: Arc::new(SessionRuntimeService::new(db, UiEventBus::new(), home)),
        }
    }

    fn approver(&self) -> PermissionApprover {
        PermissionApprover::new()
            .with_runtime_service(self.runtime.clone())
            .with_approval_timeout(Duration::from_millis(300))
            .for_session("session")
    }

    async fn yolo(&self, enabled: bool) {
        self.runtime
            .set_yolo("session", enabled)
            .await
            .expect("yolo persists");
    }

    async fn history(&self, role: &str) -> usize {
        self.runtime
            .db()
            .get_messages_by_role("session", role)
            .await
            .expect("history loads")
            .len()
    }
}

#[tokio::test]
async fn an_unsafe_command_runs_without_a_prompt_in_a_yolo_session() {
    let fixture = Fixture::new().await;
    fixture.yolo(true).await;
    let approver = fixture.approver();

    let granted = approver
        .check(TerminalMode::Unsafe, "make install", ScriptLanguage::Shell)
        .await
        .expect("granted");

    assert!(granted);
    let runtime = fixture.runtime.get("session").await.expect("runtime");
    assert!(runtime.yolo);
    assert!(runtime.waiting_permission.is_none());
    assert!(
        !approver
            .will_wait_for_approval(TerminalMode::Unsafe, "x", ScriptLanguage::Shell)
            .await
    );
    assert_eq!(fixture.history("permission_request").await, 1);
    assert_eq!(fixture.history("permission_response").await, 1);
}

#[tokio::test]
async fn a_subagent_of_a_yolo_session_is_granted_too() {
    let fixture = Fixture::new().await;
    fixture.yolo(true).await;
    let subagent = fixture.approver().for_subagent();
    assert!(
        subagent
            .check(TerminalMode::Unsafe, "make install", ScriptLanguage::Shell)
            .await
            .expect("granted")
    );
}

#[tokio::test]
async fn turning_yolo_off_makes_the_next_request_ask_again() {
    let fixture = Fixture::new().await;
    let approver = fixture.approver();
    fixture.yolo(true).await;
    assert!(
        approver
            .check(TerminalMode::Unsafe, "first", ScriptLanguage::Shell)
            .await
            .expect("granted")
    );

    fixture.yolo(false).await;
    let error = approver
        .check(TerminalMode::Unsafe, "second", ScriptLanguage::Shell)
        .await
        .expect_err("nobody answers, so the request is denied");
    let PermissionError::Denied(message) = error else {
        panic!("expected a denial, got {error:?}");
    };
    assert!(message.contains("not answered"), "{message}");
    assert_eq!(fixture.history("permission_request").await, 2);
    assert!(
        approver
            .will_wait_for_approval(TerminalMode::Unsafe, "x", ScriptLanguage::Shell)
            .await
    );
}

#[tokio::test]
async fn a_session_without_yolo_still_asks() {
    let fixture = Fixture::new().await;
    assert!(!fixture.runtime.get("session").await.expect("runtime").yolo);
    let error = fixture
        .approver()
        .check(TerminalMode::Unsafe, "make install", ScriptLanguage::Shell)
        .await
        .expect_err("unanswered");
    let PermissionError::Denied(message) = error else {
        panic!("expected a denial, got {error:?}");
    };
    assert!(message.contains("not answered"), "{message}");
}

#[tokio::test]
async fn ssh_connect_is_granted_without_asking_in_a_yolo_session() {
    let fixture = Fixture::new().await;
    fixture.yolo(true).await;
    // Nothing drains the queue: asking would hang.
    let (broker, _queue) = crate::ask_user::channel();
    let handler = TerminalPermissionHandler::for_agent(None, None, None).with_ssh_authorizer(
        MaySshAuthorizer::new("session", broker, fixture.runtime.db()),
    );
    let server = SshServer {
        name: "build".to_string(),
        target: "user@host".to_string(),
    };
    assert!(
        !handler
            .will_wait_for_location(&server, CommandRole::Main)
            .await
    );

    let granted = tokio::time::timeout(
        Duration::from_secs(10),
        handler.check_location(&server, CommandRole::Main),
    )
    .await
    .expect("did not ask")
    .expect("authorized");

    assert!(granted);
    assert_eq!(fixture.history("permission_response").await, 1);
}

#[tokio::test]
async fn a_workspace_request_is_granted_at_once_in_a_yolo_session() {
    let fixture = Fixture::new().await;
    fixture.yolo(true).await;
    let may_dir = tempfile::tempdir().expect("may dir");
    std::fs::write(may_dir.path().join("workspace.toml"), "").expect("workspace.toml");
    let requested = tempfile::tempdir().expect("requested dir");
    let approver = WorkspaceApprover::new()
        .with_runtime_service(fixture.runtime.clone())
        .with_may_dir(may_dir.path().to_path_buf());

    let granted = tokio::time::timeout(
        Duration::from_secs(10),
        approver.request_and_wait(
            "session",
            RequestWorkspaceArgs {
                path: requested.path().display().to_string(),
                reason: "read the dataset".to_string(),
            },
        ),
    )
    .await
    .expect("did not wait")
    .expect("decided");

    assert!(granted);
    let runtime = fixture.runtime.get("session").await.expect("runtime");
    assert!(runtime.waiting_workspace.is_none());
    assert_eq!(fixture.history("workspace_response").await, 1);
}

async fn create_task(db: &ChatDb, profile_id: &str) -> crate::db::entity::scheduled_tasks::Model {
    let trigger = PersistedTaskTrigger::Webhook {
        token: "quiet-brave-mint-otter-sage-fern".to_string(),
        signature: None,
    };
    db.create_scheduled_task(CreateScheduledTaskParams {
        id: "task",
        profile_id,
        session_id: Some("session"),
        name: "task",
        trigger: &trigger,
        cron_expression: None,
        run_at: None,
        script_path: None,
        script_inline: Some("echo hi"),
        needs_network: false,
        timeout_secs: 60,
    })
    .await
    .expect("task persists")
}

#[tokio::test]
async fn a_task_created_in_a_yolo_session_is_approved_and_enabled() {
    let fixture = Fixture::new().await;
    let db = fixture.runtime.db();
    let profile_id = db.ensure_default_profiles().await.expect("profiles");
    fixture.yolo(true).await;

    let created = create_task(&db, &profile_id).await;
    let granted = db
        .grant_task_under_yolo("session", created)
        .await
        .expect("granted");

    assert!(granted.approved);
    assert!(granted.enabled);
}

#[tokio::test]
async fn a_task_created_without_yolo_stays_unapproved() {
    let fixture = Fixture::new().await;
    let db = fixture.runtime.db();
    let profile_id = db.ensure_default_profiles().await.expect("profiles");

    let created = create_task(&db, &profile_id).await;
    let unchanged = db
        .grant_task_under_yolo("session", created.clone())
        .await
        .expect("unchanged");

    assert_eq!(unchanged, created);
    assert!(!unchanged.approved);
}
