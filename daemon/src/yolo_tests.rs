//! A session's yolo switch: while it is on, every approval request the
//! session raises is granted at once and recorded like a user approval, a
//! scheduled task it creates is stored approved and enabled, and turning it
//! off makes the next request ask again. A session without yolo is unchanged.

use std::sync::Arc;
use std::time::Duration;

use crate::harness::workspace_request::RequestWorkspaceArgs;
use aither_core::llm::{Approval, ApprovalKind, ApprovalRequest, Approver as _};
use tempfile::TempDir;

use super::state::WorkspaceApprover;
use crate::agent::PermissionApprover;
use crate::agent::permission::UNSANDBOXED_KIND;
use crate::agent::ssh_authorizer::MaySshAuthorizer;
use crate::db::{ChatDb, CreateScheduledTaskParams};
use crate::session::SessionRuntimeService;
use crate::task_trigger::PersistedTaskTrigger;
use crate::terminal_permission_handler::TerminalPermissionHandler;
use crate::ui_event_bus::UiEventBus;
use may_protocol::layout_consts::SSH_KIND;

/// A request to run `script` outside the sandbox.
fn unsandboxed(script: &str) -> ApprovalRequest {
    ApprovalRequest::new(ApprovalKind::new(UNSANDBOXED_KIND), script)
}

struct Fixture {
    _dir: TempDir,
    runtime: Arc<SessionRuntimeService>,
}

impl Fixture {
    async fn new() -> Self {
        crate::test_support::install_global_executor();
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
        .approve(unsandboxed("make install"))
        .await
        .expect("granted");

    assert!(matches!(granted, Approval::Approved), "{granted:?}");
    let runtime = fixture.runtime.get("session").await.expect("runtime");
    assert!(runtime.yolo);
    assert!(runtime.waiting_permission.is_none());
    assert!(!approver.will_wait_for_approval().await);
    assert_eq!(fixture.history("permission_request").await, 1);
    assert_eq!(fixture.history("permission_response").await, 1);
}

#[tokio::test]
async fn a_subagent_of_a_yolo_session_is_granted_too() {
    let fixture = Fixture::new().await;
    fixture.yolo(true).await;
    let subagent = fixture.approver().for_subagent();
    assert!(matches!(
        subagent
            .approve(unsandboxed("make install"))
            .await
            .expect("granted"),
        Approval::Approved
    ));
}

#[tokio::test]
async fn turning_yolo_off_makes_the_next_request_ask_again() {
    let fixture = Fixture::new().await;
    let approver = fixture.approver();
    fixture.yolo(true).await;
    assert!(matches!(
        approver
            .approve(unsandboxed("first"))
            .await
            .expect("granted"),
        Approval::Approved
    ));

    fixture.yolo(false).await;
    let denied = approver
        .approve(unsandboxed("second"))
        .await
        .expect("a request nobody answers is denied");
    let Approval::Denied { reason } = denied else {
        panic!("expected a denial, got {denied:?}");
    };
    assert!(reason.contains("not answered"), "{reason}");
    assert_eq!(fixture.history("permission_request").await, 2);
    assert!(approver.will_wait_for_approval().await);
}

#[tokio::test]
async fn a_session_without_yolo_still_asks() {
    let fixture = Fixture::new().await;
    assert!(!fixture.runtime.get("session").await.expect("runtime").yolo);
    let denied = fixture
        .approver()
        .approve(unsandboxed("make install"))
        .await
        .expect("an unanswered request is denied");
    let Approval::Denied { reason } = denied else {
        panic!("expected a denial, got {denied:?}");
    };
    assert!(reason.contains("not answered"), "{reason}");
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
    assert!(!handler.will_wait_for_location().await);

    let request =
        ApprovalRequest::new(ApprovalKind::new(SSH_KIND), "make").with_reason("user@host");
    let granted = tokio::time::timeout(Duration::from_secs(10), handler.approve(request))
        .await
        .expect("did not ask")
        .expect("authorized");

    assert!(matches!(granted, Approval::Approved), "{granted:?}");
    assert_eq!(fixture.history("permission_response").await, 1);
}

#[tokio::test]
async fn a_read_only_subagents_escalation_is_refused_even_in_a_yolo_session() {
    let fixture = Fixture::new().await;
    fixture.yolo(true).await;
    let handler = TerminalPermissionHandler::for_agent(Some(fixture.approver()), None, None)
        .for_read_only_subagent();

    let denied = handler
        .approve(unsandboxed("make install"))
        .await
        .expect("decided");
    let Approval::Denied { reason } = denied else {
        panic!("expected a denial, got {denied:?}");
    };
    assert!(reason.contains("read-only"), "{reason}");

    let ssh = ApprovalRequest::new(ApprovalKind::new(SSH_KIND), "uname").with_reason("ops@prod");
    assert!(matches!(
        handler.approve(ssh).await.expect("decided"),
        Approval::Denied { .. }
    ));
    assert_eq!(fixture.history("permission_request").await, 0);
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

fn selected_answer(option: &str) -> Vec<crate::ask_user::QuestionAnswer> {
    vec![crate::ask_user::QuestionAnswer {
        section: String::new(),
        answer: crate::ask_user::AnswerValue::Selected(vec![option.to_string()]),
    }]
}

#[tokio::test]
async fn ssh_connect_offers_three_options_and_approve_once_asks_again() {
    let fixture = Fixture::new().await;
    let (broker, requests) = crate::ask_user::channel();
    let handler = TerminalPermissionHandler::for_agent(None, None, None).with_ssh_authorizer(
        MaySshAuthorizer::new("session", broker, fixture.runtime.db()),
    );
    assert!(handler.will_wait_for_location().await);

    let responder = tokio::spawn(async move {
        let first = requests.next().await.expect("first ssh ask");
        assert_eq!(
            first.args.request.option,
            vec!["Approve", "Always allow for session", "Deny"]
        );
        first
            .respond(selected_answer("Approve"))
            .expect("respond first");

        let second = requests.next().await.expect("second ssh ask");
        assert_eq!(
            second.args.request.option,
            vec!["Approve", "Always allow for session", "Deny"]
        );
        second
            .respond(selected_answer("Deny"))
            .expect("respond second");
    });

    let first_req =
        ApprovalRequest::new(ApprovalKind::new(SSH_KIND), "uname").with_reason("ops@prod");
    assert!(matches!(
        handler.approve(first_req).await.expect("decided"),
        Approval::Approved
    ));

    let second_req =
        ApprovalRequest::new(ApprovalKind::new(SSH_KIND), "uname").with_reason("ops@prod");
    assert!(matches!(
        handler.approve(second_req).await.expect("decided"),
        Approval::Denied { .. }
    ));

    responder.await.expect("responder");
}

#[tokio::test]
async fn ssh_connect_always_allow_remembers_per_session_and_across_rebuilds() {
    let fixture = Fixture::new().await;
    let db = fixture.runtime.db();
    db.create_session("other-session", Some("Other"), Some("test-model"))
        .await
        .expect("other session");

    let (broker, requests) = crate::ask_user::channel();
    let handler = TerminalPermissionHandler::for_agent(None, None, None)
        .with_ssh_authorizer(MaySshAuthorizer::new("session", broker, db.clone()));

    let responder = tokio::spawn(async move {
        let first = requests.next().await.expect("initial ask");
        first
            .respond(selected_answer("Always allow for session"))
            .expect("respond initial");
        // Only one ask should occur for "session" regardless of subsequent calls
        // or rebuilt authorizers; "other-session" uses its own channel below.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), requests.next())
                .await
                .is_err(),
            "unexpected extra prompt for session grant"
        );
    });

    let req = ApprovalRequest::new(ApprovalKind::new(SSH_KIND), "ls").with_reason("ops@prod");
    assert!(matches!(
        handler.approve(req.clone()).await.expect("first"),
        Approval::Approved
    ));
    // Cached in memory on same authorizer.
    assert!(matches!(
        handler.approve(req.clone()).await.expect("cached"),
        Approval::Approved
    ));

    // Rebuilt authorizer reads ChatDb session grant.
    let (rebuilt_broker, _rebuilt_requests) = crate::ask_user::channel();
    let rebuilt_handler = TerminalPermissionHandler::for_agent(None, None, None)
        .with_ssh_authorizer(MaySshAuthorizer::new("session", rebuilt_broker, db.clone()));
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), rebuilt_handler.approve(req.clone()))
            .await
            .expect("did not ask after rebuild")
            .expect("rebuilt"),
        Approval::Approved
    ));
    responder.await.expect("responder");

    // Another session still prompts.
    let (other_broker, other_requests) = crate::ask_user::channel();
    let other_handler = TerminalPermissionHandler::for_agent(None, None, None)
        .with_ssh_authorizer(MaySshAuthorizer::new("other-session", other_broker, db));
    let other_responder = tokio::spawn(async move {
        let ask = other_requests.next().await.expect("other session asks");
        assert_eq!(ask.args.session_id, "other-session");
        ask.respond(selected_answer("Deny")).expect("respond other");
    });
    assert!(matches!(
        other_handler.approve(req).await.expect("other"),
        Approval::Denied { .. }
    ));
    other_responder.await.expect("other responder");
}

#[tokio::test]
async fn subagent_ssh_connect_routes_through_main_session_and_shares_grants() {
    let fixture = Fixture::new().await;
    let (broker, requests) = crate::ask_user::channel();
    let main_handler = TerminalPermissionHandler::for_agent(None, None, None).with_ssh_authorizer(
        MaySshAuthorizer::new("session", broker, fixture.runtime.db()),
    );
    let subagent_handler = main_handler.for_subagent();

    let responder = tokio::spawn(async move {
        let ask = requests.next().await.expect("subagent routes through main");
        assert_eq!(ask.args.session_id, "session");
        assert_eq!(
            ask.args.request.option,
            vec!["Approve", "Always allow for session", "Deny"]
        );
        ask.respond(selected_answer("Always allow for session"))
            .expect("respond subagent");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), requests.next())
                .await
                .is_err(),
            "main handler should reuse grant from subagent"
        );
    });

    let req =
        ApprovalRequest::new(ApprovalKind::new(SSH_KIND), "uname -a").with_reason("ops@buildbox");
    assert!(matches!(
        subagent_handler
            .approve(req.clone())
            .await
            .expect("subagent approved"),
        Approval::Approved
    ));
    assert!(matches!(
        main_handler.approve(req).await.expect("main reuses grant"),
        Approval::Approved
    ));
    responder.await.expect("responder");
}
