//! Behaviour of [`SessionRuns`]: one run per session, stop delivery and
//! settling, against a real SQLite database file.

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
    service: Arc<SessionRuntimeService>,
}

impl Fixture {
    async fn new(session_ids: &[&str]) -> Self {
        init_executor();
        let dir = tempfile::tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("open database");
        for session_id in session_ids {
            db.create_session(session_id, Some("Session"), Some("test-model"))
                .await
                .expect("create session");
        }
        let bus = UiEventBus::new();
        Self {
            _dir: dir,
            service: Arc::new(SessionRuntimeService::new(db, bus)),
        }
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

/// Tasks racing `acquire` on one session: exactly one holder wins every
/// round.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_acquires_yield_exactly_one_holder() {
    const RACERS: usize = 32;
    let fixture = Fixture::new(&["session"]).await;

    for _ in 0..20 {
        let service = fixture.service.clone();
        let racers = (0..RACERS)
            .map(|_| {
                let service = service.clone();
                tokio::spawn(async move { service.runs().acquire("session").await })
            })
            .collect::<Vec<_>>();
        let mut winner = None;
        for racer in racers {
            match racer.await.expect("racer task") {
                Ok(guard) => {
                    assert!(winner.is_none(), "a second run was admitted");
                    winner = Some(guard);
                }
                Err(AcquireError::Busy(_)) => {}
                Err(other) => panic!("unexpected acquire failure: {other:?}"),
            }
        }
        winner
            .expect("one racer must hold the session")
            .finish_idle(RunOutcome::Completed)
            .await
            .expect("release the round's run");
    }
}

/// While a run holds the session, `acquire` is refused with [`SessionBusy`];
/// once it finishes the session is free again.
#[tokio::test]
async fn a_second_acquire_is_busy_until_the_run_finishes() {
    let fixture = Fixture::new(&["session"]).await;
    let service = &fixture.service;

    let run = service.runs().acquire("session").await.expect("acquire");
    let refused = service.runs().acquire("session").await;
    assert!(
        matches!(refused, Err(AcquireError::Busy(ref busy)) if busy.session_id == "session"),
        "a second run must get SessionBusy, got {refused:?}"
    );
    assert_eq!(
        service.get("session").await.expect("runtime").state,
        SessionRuntimeState::Running
    );

    run.finish_idle(RunOutcome::Completed)
        .await
        .expect("run ends");
    service
        .runs()
        .acquire("session")
        .await
        .expect("the session is free once the run ended")
        .finish_idle(RunOutcome::Completed)
        .await
        .expect("second run ends");
}

/// Stop in each waiting state ends `Idle` with the run released.
#[tokio::test]
async fn stop_in_each_waiting_state_ends_idle_with_the_run_released() {
    #[derive(Clone, Copy, Debug)]
    enum Waiting {
        Permission,
        Workspace,
        AskUser,
    }
    let fixture = Fixture::new(&["session"]).await;
    let service = &fixture.service;

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
            service.runs().cancel("session"),
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
            !service.runs().is_held("session"),
            "{waiting:?}: the run must be released"
        );
    }
}

/// A session left waiting with no run (its run died with the process) is
/// settled by the stop itself.
#[tokio::test]
async fn stop_settles_a_waiting_session_that_has_no_run() {
    let fixture = Fixture::new(&["session"]).await;
    let service = &fixture.service;
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
    assert!(!service.runs().is_held("session"));
}

/// A run that finishes `Completed` with a queued message takes the session
/// again for the follow-up.
#[tokio::test]
async fn a_completed_run_with_queued_work_continues() {
    let fixture = Fixture::new(&["session"]).await;
    let service = &fixture.service;

    let run = service.runs().acquire("session").await.expect("acquire");
    service
        .db()
        .set_pending_immediate_message("session", pending_message("follow up"))
        .await
        .expect("queue the follow-up");

    let next = run
        .finish(RunOutcome::Completed)
        .await
        .expect("finish")
        .expect("the queued message starts a follow-up run");
    next.finish_idle(RunOutcome::Completed)
        .await
        .expect("follow-up ends");
    assert!(!service.runs().is_held("session"));
}

/// A run that finishes `Completed` with nothing queued releases for good.
#[tokio::test]
async fn finish_without_pending_work_releases_for_good() {
    let fixture = Fixture::new(&["session"]).await;
    let service = &fixture.service;

    let run = service.runs().acquire("session").await.expect("acquire");
    let next = run.finish(RunOutcome::Completed).await.expect("finish");
    assert!(next.is_none());
    assert!(!service.runs().is_held("session"));
    assert_eq!(
        service.get("session").await.expect("runtime").state,
        SessionRuntimeState::Idle
    );
}

/// Dropping a guard without finishing still settles the session.
#[tokio::test]
async fn dropped_guard_releases_the_session() {
    let fixture = Fixture::new(&["session"]).await;
    let service = &fixture.service;

    let run = service.runs().acquire("session").await.expect("acquire");
    drop(run);

    bounded("the dropped run to settle", async {
        while service.get("session").await.expect("runtime").state != SessionRuntimeState::Idle {
            async_io::Timer::after(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(!service.runs().is_held("session"));
}

/// Restart recovery settles sessions a dead process left owned: waiting
/// requests and takeovers are resolved and the state lands `Idle`.
#[tokio::test]
async fn recovery_settles_sessions_a_dead_run_left_owned() {
    let fixture = Fixture::new(&["stale"]).await;
    let service = &fixture.service;

    // What a dead process leaves behind: a claimed run, a waiting request
    // and a browser takeover.
    let run = service.runs().acquire("stale").await.expect("acquire");
    let stale_fence = run.fence().clone();
    service
        .set_waiting_permission(
            "stale",
            Some(PersistedPermissionRequest {
                id: "permission".to_string(),
                request: PersistedPermissionRequestPayload {
                    mode: PermissionRequestMode::Default,
                    script: "echo hi".to_string(),
                    session_id: Some("stale".to_string()),
                },
            }),
        )
        .await
        .expect("record permission wait");
    service
        .db()
        .set_browser_takeover_owner("stale")
        .await
        .expect("takeover");
    std::mem::forget(run); // die without settling, as a crash would

    // Recovery runs at daemon start, before any run exists; the forgotten
    // guard is not in this registry either, so forget it too.
    let fresh = SessionRuntimeService::new(service.db(), UiEventBus::new());
    fresh
        .runs()
        .recover_after_restart()
        .await
        .expect("recovery");

    let runtime = fresh.get("stale").await.expect("runtime");
    assert_eq!(runtime.state, SessionRuntimeState::Idle);
    assert!(runtime.waiting_permission.is_none());
    assert!(
        !service
            .db()
            .get_session_runtime("stale")
            .await
            .expect("runtime")
            .expect("row")
            .interactions
            .is_active()
    );
    // The dead run's fence no longer commits.
    assert!(matches!(
        service.db().finish_session_run(&stale_fence).await,
        Ok(false)
    ));
}
