//! Lease behaviour of [`SessionRuns`] against real SQLite database files.
//!
//! Each test opens independent [`ChatDb`] handles on one file, as a web
//! server and a TUI process sharing `history.db` do.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;

use super::{
    AcquireError, PendingUserMessage, PendingUserMessageMode, PersistedAskUserQuestion,
    PersistedAskUserRequest, PersistedPermissionRequest, PersistedPermissionRequestPayload,
    PersistedWorkspaceRequest, PersistedWorkspaceRequestPayload, RunOutcome, SessionRuntimeService,
    SessionRuntimeState,
};
use crate::PermissionRequestMode;
use crate::db::ChatDb;
use crate::ui_event_bus::UiEventBus;

/// Upper bound on any single wait in this module.
const WAIT_BOUND: Duration = Duration::from_secs(20);

fn init_executor() {
    let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
}

/// Bound `future` by [`WAIT_BOUND`], naming `what` it waits for on expiry.
async fn bounded<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT_BOUND, future)
        .await
        .unwrap_or_else(|_| panic!("timed out after {WAIT_BOUND:?} waiting for {what}"))
}

struct Fixture {
    _dir: TempDir,
    path: std::path::PathBuf,
}

impl Fixture {
    async fn new(session_ids: &[&str]) -> Self {
        init_executor();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        let db = ChatDb::open(&path).await.expect("open database");
        for session_id in session_ids {
            db.create_session(session_id, Some("Session"), Some("test-model"))
                .await
                .expect("create session");
        }
        Self { _dir: dir, path }
    }

    /// A runtime service over a fresh connection to the shared file.
    async fn service(&self) -> Arc<SessionRuntimeService> {
        self.service_with_ttl(super::DEFAULT_RUN_LEASE_TTL).await
    }

    async fn service_with_ttl(&self, ttl: Duration) -> Arc<SessionRuntimeService> {
        let db = ChatDb::open(&self.path).await.expect("open handle");
        let bus = UiEventBus::new();
        Arc::new(SessionRuntimeService::with_run_ttl(db, bus, ttl))
    }
}

fn pending_message(content: &str) -> PendingUserMessage {
    PendingUserMessage {
        content: content.to_string(),
        attachments: Vec::new(),
        hidden_context_xml: None,
        created_at: jiff::Timestamp::now().to_string(),
        delivery_mode: PendingUserMessageMode::Queue,
    }
}

/// Two handles racing `acquire` on one database file: exactly one holder
/// wins every round.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_acquires_yield_exactly_one_holder() {
    const ROUNDS: usize = 60;
    let fixture = Fixture::new(&["session"]).await;
    let first = fixture.service().await;
    let second = fixture.service().await;

    for round in 0..ROUNDS {
        let (a, b) = bounded(
            "both racing acquires to return",
            futures_lite::future::zip(
                {
                    let first = first.clone();
                    async move { first.runs().acquire("session").await }
                },
                {
                    let second = second.clone();
                    async move { second.runs().acquire("session").await }
                },
            ),
        )
        .await;
        let winner = match (a, b) {
            (Ok(guard), Err(AcquireError::Busy(_))) | (Err(AcquireError::Busy(_)), Ok(guard)) => {
                guard
            }
            (a, b) => panic!("round {round}: expected one holder, got {a:?} and {b:?}"),
        };
        winner
            .finish_idle(RunOutcome::Completed)
            .await
            .expect("release the round's lease");
    }
}

/// Handle for the web server and the TUI: the second to acquire is refused.
#[tokio::test]
async fn tui_and_web_acquiring_one_session_refuse_the_second() {
    let fixture = Fixture::new(&["session"]).await;
    let web = fixture.service().await;
    let tui = fixture.service().await;

    let web_run = web.runs().acquire("session").await.expect("web acquires");
    let refused = tui.runs().acquire("session").await;
    assert!(
        matches!(refused, Err(AcquireError::Busy(ref busy)) if busy.session_id == "session"),
        "the TUI must get SessionBusy while the web run holds the session, got {refused:?}"
    );
    assert_eq!(
        tui.get("session").await.expect("runtime").state,
        SessionRuntimeState::Running
    );

    web_run
        .finish_idle(RunOutcome::Completed)
        .await
        .expect("web run ends");
    tui.runs()
        .acquire("session")
        .await
        .expect("the TUI acquires once the web run ended")
        .finish_idle(RunOutcome::Completed)
        .await
        .expect("TUI run ends");
}

/// Stop in each waiting state ends `Idle` with the lease released.
#[tokio::test]
async fn stop_in_each_waiting_state_ends_idle_with_the_lease_released() {
    #[derive(Clone, Copy, Debug)]
    enum Waiting {
        Permission,
        Workspace,
        AskUser,
    }
    let fixture = Fixture::new(&["session"]).await;
    let service = fixture.service().await;

    for (waiting, expected) in [
        (Waiting::Permission, SessionRuntimeState::WaitingPermission),
        (Waiting::Workspace, SessionRuntimeState::WaitingWorkspace),
        (Waiting::AskUser, SessionRuntimeState::WaitingAskUser),
    ] {
        let guard = service.runs().acquire("session").await.expect("acquire");
        match waiting {
            Waiting::Permission => {
                service
                    .set_waiting_permission(
                        "session",
                        Some(PersistedPermissionRequest {
                            id: "permission".to_string(),
                            request: PersistedPermissionRequestPayload {
                                mode: PermissionRequestMode::Default,
                                script: "echo hi".to_string(),
                                session_id: Some("session".to_string()),
                            },
                        }),
                    )
                    .await
                    .expect("record permission wait");
            }
            Waiting::Workspace => {
                service
                    .set_waiting_workspace(
                        "session",
                        Some(PersistedWorkspaceRequest {
                            id: "workspace".to_string(),
                            request: PersistedWorkspaceRequestPayload {
                                path: "/tmp/project".to_string(),
                                reason: "write access".to_string(),
                                session_id: Some("session".to_string()),
                            },
                        }),
                    )
                    .await
                    .expect("record workspace wait");
            }
            Waiting::AskUser => {
                service
                    .set_waiting_ask_user(
                        "session",
                        Some(PersistedAskUserRequest {
                            id: "ask".to_string(),
                            questions: vec![PersistedAskUserQuestion {
                                section: "default".to_string(),
                                question: "Proceed?".to_string(),
                                option: vec!["yes".to_string()],
                                multi_select: false,
                            }],
                        }),
                    )
                    .await
                    .expect("record ask-user wait");
            }
        }
        assert_eq!(
            service.get("session").await.expect("runtime").state,
            expected,
            "{waiting:?}: the session must be waiting before the stop"
        );

        // The stop reaches the run, which ends and settles the session.
        assert!(
            service.runs().cancel("session").await.expect("stop"),
            "{waiting:?}: the stop must reach the run"
        );
        bounded(
            "the stop signal on the run's cancellation channel",
            guard.cancellation().recv(),
        )
        .await
        .expect("cancellation channel stays open while the guard lives");
        guard
            .finish_idle(RunOutcome::Cancelled)
            .await
            .expect("finish cancelled run");

        let runtime = service.get("session").await.expect("runtime");
        assert_eq!(runtime.state, SessionRuntimeState::Idle, "{waiting:?}");
        assert!(runtime.waiting_permission.is_none(), "{waiting:?}");
        assert!(runtime.waiting_workspace.is_none(), "{waiting:?}");
        assert!(runtime.waiting_ask_user.is_none(), "{waiting:?}");
        assert!(
            !service.runs().is_held("session").await.expect("lease"),
            "{waiting:?}: the lease must be released"
        );
    }
}

/// A session left waiting with no run (its run died with the process) is
/// settled by the stop itself.
#[tokio::test]
async fn stop_settles_a_waiting_session_that_has_no_run() {
    let fixture = Fixture::new(&["session"]).await;
    let service = fixture.service().await;
    service
        .set_waiting_workspace(
            "session",
            Some(PersistedWorkspaceRequest {
                id: "workspace".to_string(),
                request: PersistedWorkspaceRequestPayload {
                    path: "/tmp/project".to_string(),
                    reason: "write access".to_string(),
                    session_id: Some("session".to_string()),
                },
            }),
        )
        .await
        .expect("record workspace wait");

    let runtime = bounded(
        "the stop to settle the orphaned wait",
        service.settle_stopped("session"),
    )
    .await
    .expect("settle");
    assert_eq!(runtime.state, SessionRuntimeState::Idle);
    assert!(!service.runs().is_held("session").await.expect("lease"));
}

/// A dispatcher finishing while a message is enqueued and its own dispatch
/// attempt is refused: the message is never stranded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatcher_exit_racing_an_enqueue_never_strands_the_message() {
    const ROUNDS: usize = 60;
    let fixture = Fixture::new(&["session"]).await;
    let dispatcher = fixture.service().await;
    let enqueuer = fixture.service().await;

    for round in 0..ROUNDS {
        let holder = dispatcher
            .runs()
            .acquire("session")
            .await
            .expect("the dispatcher holds the session");
        assert!(
            dispatcher
                .take_next_for_run("session")
                .await
                .expect("take")
                .is_none(),
            "round {round}: the queue starts empty"
        );

        let (continued, enqueued_run) = bounded(
            "the dispatcher exit and the enqueue to settle",
            futures_lite::future::zip(
                async { holder.finish(RunOutcome::Completed).await.expect("finish") },
                async {
                    enqueuer
                        .enqueue(
                            "session",
                            pending_message("hello"),
                            PendingUserMessageMode::Queue,
                        )
                        .await
                        .expect("enqueue");
                    // What the enqueue endpoint does: dispatch if it can.
                    enqueuer.runs().acquire("session").await
                },
            ),
        )
        .await;

        let mut running = Vec::new();
        running.extend(continued);
        running.extend(enqueued_run.ok());
        assert!(
            !running.is_empty(),
            "round {round}: the message was enqueued but no run took over to dispatch it"
        );
        // Whoever holds the session runs the message; the queue drains.
        let message = enqueuer
            .take_next_for_run("session")
            .await
            .expect("take")
            .expect("the pending message is still queued");
        assert_eq!(message.content, "hello");
        for guard in running {
            guard
                .finish_idle(RunOutcome::Completed)
                .await
                .expect("release");
        }
    }
}

/// A run finishing with nothing pending does not re-acquire, so the client's
/// next message is not refused.
#[tokio::test]
async fn finish_without_pending_work_releases_for_good() {
    let fixture = Fixture::new(&["session"]).await;
    let service = fixture.service().await;
    let guard = service.runs().acquire("session").await.expect("acquire");
    let continued = guard.finish(RunOutcome::Completed).await.expect("finish");
    assert!(continued.is_none());
    assert!(!service.runs().is_held("session").await.expect("lease"));

    // With work pending, a completed run continues under a new guard, and a
    // failed or cancelled one does not.
    service
        .enqueue(
            "session",
            pending_message("again"),
            PendingUserMessageMode::Queue,
        )
        .await
        .expect("enqueue");
    let guard = service.runs().acquire("session").await.expect("acquire");
    let failed = guard.finish(RunOutcome::Failed).await.expect("finish");
    assert!(failed.is_none(), "a failed run must not keep dispatching");
    let guard = service.runs().acquire("session").await.expect("acquire");
    let continued = guard
        .finish(RunOutcome::Completed)
        .await
        .expect("finish")
        .expect("a completed run continues with the pending message");
    assert_eq!(
        service
            .take_next_for_run("session")
            .await
            .expect("take")
            .expect("message")
            .content,
        "again"
    );
    continued
        .finish_idle(RunOutcome::Completed)
        .await
        .expect("release");
}

/// A guard dropped without finishing (an early `?` return) releases the
/// lease and leaves the session `Idle`.
#[tokio::test]
async fn dropped_guard_releases_the_session() {
    let fixture = Fixture::new(&["session"]).await;
    let service = fixture.service().await;
    let guard = service.runs().acquire("session").await.expect("acquire");
    drop(guard);

    bounded("the dropped guard to release the lease", async {
        while service.runs().is_held("session").await.expect("lease") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert_eq!(
        service.get("session").await.expect("runtime").state,
        SessionRuntimeState::Idle
    );
    service
        .runs()
        .acquire("session")
        .await
        .expect("the session is acquirable again")
        .finish_idle(RunOutcome::Completed)
        .await
        .expect("release");
}

/// A holder whose lease another run took over learns of it from its
/// heartbeat and is told to stop; its fenced writes are refused.
#[tokio::test]
async fn heartbeat_reports_a_lost_lease_to_the_run() {
    let fixture = Fixture::new(&["session"]).await;
    let holder = fixture.service_with_ttl(Duration::from_millis(300)).await;
    let thief = ChatDb::open(&fixture.path).await.expect("thief handle");

    let guard = holder.runs().acquire("session").await.expect("acquire");
    assert!(!guard.is_lost());
    // Another process takes the lease over, judging it expired.
    thief
        .acquire_session_run_lease("session", "thief", 0, i64::MAX)
        .await
        .expect("takeover")
        .expect("an expired lease is taken over");

    bounded(
        "the heartbeat to report the lost lease on the cancellation channel",
        guard.cancellation().recv(),
    )
    .await
    .expect("cancellation channel stays open while the guard lives");
    assert!(guard.is_lost());
    assert!(holder.runs().is_lost("session"));
    let refused = holder
        .db()
        .append_chat_message_as_run(guard.fence(), &crate::session::ChatMessage::user("late"))
        .await;
    assert!(
        matches!(refused, Err(crate::db::ChatDbError::RunLeaseLost { .. })),
        "a stale holder's commit must be refused, got {refused:?}"
    );
    // Finishing a lost run leaves the new holder's state alone.
    guard
        .finish_idle(RunOutcome::Failed)
        .await
        .expect("finish lost run");
    assert_eq!(
        holder.get("session").await.expect("runtime").state,
        SessionRuntimeState::Running,
        "the new holder's Running state must survive the stale finish"
    );
}

/// A stop requested from another process reaches the holder through its
/// heartbeat.
#[tokio::test]
async fn cancel_from_another_process_reaches_the_holder_by_heartbeat() {
    let fixture = Fixture::new(&["session"]).await;
    let holder = fixture.service_with_ttl(Duration::from_millis(300)).await;
    let remote = fixture.service().await;
    let guard = holder.runs().acquire("session").await.expect("acquire");

    assert!(
        remote.runs().cancel("session").await.expect("stop"),
        "the stop must find the lease"
    );
    bounded(
        "the heartbeat to deliver the remote stop request",
        guard.cancellation().recv(),
    )
    .await
    .expect("cancellation channel stays open while the guard lives");
    assert!(!guard.is_lost());
    guard
        .finish_idle(RunOutcome::Cancelled)
        .await
        .expect("finish");
}

/// Restart recovery frees expired leases only and repairs runtime state with
/// field-level updates, leaving a live run's session alone.
#[tokio::test]
async fn recovery_reclaims_only_expired_leases() {
    let fixture = Fixture::new(&["stale", "live"]).await;
    let service = fixture.service().await;
    let db = service.db();

    // `stale` has a dead holder (heartbeat in 1970); `live` a current one.
    db.acquire_session_run_lease("stale", "dead-owner", 0, i64::MIN)
        .await
        .expect("acquire stale")
        .expect("free");
    let live = service.runs().acquire("live").await.expect("acquire live");
    db.set_browser_takeover_owner("stale")
        .await
        .expect("takeover");

    service
        .runs()
        .recover_after_restart()
        .await
        .expect("recover");

    assert_eq!(
        service.get("stale").await.expect("runtime").state,
        SessionRuntimeState::Idle
    );
    assert!(!service.runs().is_held("stale").await.expect("lease"));
    assert_eq!(
        service.get("live").await.expect("runtime").state,
        SessionRuntimeState::Running,
        "a live run's session must be left alone"
    );
    assert!(service.runs().is_held("live").await.expect("lease"));
    live.finish_idle(RunOutcome::Completed)
        .await
        .expect("release");
}
