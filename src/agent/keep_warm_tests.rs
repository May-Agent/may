//! Keeping the idle agent's prompt cache warm while a background command
//! outlives it, from a saved config through May's agent construction to
//! aither, against a stub model server.
//!
//! The scheduling decision is read off the agent on a [`ManualClock`]; the
//! idle worker's firing, its cancellation and the usage it records run on
//! the system clock with a cache lifetime of a few seconds, since the worker
//! waits on real timers.

use std::time::Duration;

use aither_sandbox::SandboxMode;

use super::idle_compaction_tests::{
    SESSION_MODEL, Session, StubModel, run_short_turn, saved_config,
};
use super::runner::{AgentWorker, spawn_worker};
use crate::cache_lifetime::CacheLifetime;
use crate::config::MayConfig;

/// Half a minute short of the default one-hour lifetime.
const DUE: Duration = Duration::from_secs(59 * 60 + 30);

/// Registers a running job on `agent`'s terminal, bounded by `timeout` when
/// given.
async fn start_job(
    registry: &aither_sandbox::JobRegistry,
    pid: u32,
    timeout: Option<Duration>,
) -> aither_core::IdLease {
    let lease = crate::model_ids::SessionIds::restore([])
        .expect("a pool")
        .lease()
        .expect("a free word");
    if let Some(timeout) = timeout {
        registry.bound_by_timeout(&lease, timeout).await;
    }
    registry
        .register(
            pid,
            &lease,
            "key",
            "sleep 9999",
            SandboxMode::Sandboxed,
            None,
        )
        .await;
    lease
}

fn config_with_lifetime(stub: &StubModel, lifetime: CacheLifetime) -> MayConfig {
    let mut config = saved_config(stub, None);
    config
        .provider
        .get_mut("session")
        .expect("the session provider")
        .cache_lifetime = lifetime;
    config
}

/// With the default one-hour lifetime, a command with no deadline or one past
/// the cold point schedules the refresh half a minute before the entry cools,
/// and repeats it as the entry ages; commands that all end first, or none,
/// schedule nothing.
#[tokio::test]
async fn a_running_command_past_the_cold_point_schedules_a_refresh() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let mut session = Box::pin(Session::start(&saved_config(&stub, None))).await;
    let registry = session.agent.job_registry();

    assert_eq!(
        session.agent.keep_warm_due_in().await,
        None,
        "nothing sent yet, so no entry"
    );
    run_short_turn(&mut session.agent).await;
    assert_eq!(
        session.agent.keep_warm_due_in().await,
        None,
        "no command runs"
    );

    let _short = start_job(&registry, 1, Some(Duration::from_mins(30))).await;
    assert_eq!(
        session.agent.keep_warm_due_in().await,
        None,
        "the only command ends before the entry cools"
    );

    let _long = start_job(&registry, 2, Some(Duration::from_hours(3))).await;
    assert_eq!(session.agent.keep_warm_due_in().await, Some(DUE));

    session.clock.advance(Duration::from_mins(10));
    assert_eq!(
        session.agent.keep_warm_due_in().await,
        Some(Duration::from_secs(49 * 60 + 30)),
        "the refresh stays half a minute before the cold point"
    );
}

#[tokio::test]
async fn a_command_without_a_deadline_schedules_a_refresh() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let mut session = Box::pin(Session::start(&saved_config(&stub, None))).await;
    run_short_turn(&mut session.agent).await;

    let _job = start_job(&session.agent.job_registry(), 1, None).await;
    assert_eq!(session.agent.keep_warm_due_in().await, Some(DUE));
}

/// A refresh is one more request that leaves the conversation as it was and
/// restarts the entry's lifetime.
#[tokio::test]
async fn a_refresh_sends_a_request_and_leaves_the_conversation_alone() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let mut session = Box::pin(Session::start(&saved_config(&stub, None))).await;
    run_short_turn(&mut session.agent).await;
    let before = session.agent.context_messages().await.expect("messages");
    let sent = stub.requests.lock().await.len();

    session.clock.advance(Duration::from_mins(50));
    session
        .agent
        .keep_cache_warm()
        .await
        .expect("the refresh succeeds")
        .expect("the entry was warm");

    assert_eq!(stub.requests.lock().await.len(), sent + 1);
    assert_eq!(
        session.agent.context_messages().await.expect("messages"),
        before
    );
    let _job = start_job(&session.agent.job_registry(), 1, None).await;
    assert_eq!(
        session.agent.keep_warm_due_in().await,
        Some(DUE),
        "the entry's lifetime restarted"
    );
}

/// Polls `condition` until it holds, panicking after `limit`.
async fn eventually(limit: Duration, what: &str, mut condition: impl AsyncFnMut() -> bool) {
    let deadline = std::time::Instant::now() + limit;
    while !condition().await {
        assert!(std::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// An idle worker over a session's agent, with what keeps the session's
/// database and directory alive.
struct Idle {
    worker: AgentWorker,
    db: crate::db::ChatDb,
    session_id: String,
    _temp: tempfile::TempDir,
}

async fn idle_worker(stub: &StubModel, lifetime: Duration) -> Idle {
    let config = config_with_lifetime(stub, CacheLifetime::Limited(lifetime));
    let Session {
        mut agent,
        parts,
        temp,
        ..
    } = Box::pin(Session::start_on_system_clock(&config)).await;
    run_short_turn(&mut agent).await;
    let db = parts.db.clone();
    let runs = crate::session::SessionRuns::new(db.clone(), crate::ui_event_bus::UiEventBus::new());
    Idle {
        worker: spawn_worker(executor_core::DefaultExecutor, agent, db.clone(), runs),
        db,
        session_id: parts.session_id.as_str().to_string(),
        _temp: temp,
    }
}

/// An idle agent with a command that outlives its cache entry refreshes it
/// before it cools, again and again, and each refresh is logged with the
/// session's usage as a keep-warm request; a turn taking the agent ends the
/// refreshes.
#[tokio::test]
async fn the_idle_worker_refreshes_and_a_turn_cancels_it() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let Idle {
        worker,
        db,
        session_id,
        _temp,
    } = idle_worker(&stub, Duration::from_secs(2)).await;
    let registry = worker.job_registry().expect("a registry").clone();
    let _job = start_job(&registry, 1, None).await;
    // A refresh is logged after its response ends, so wait on the log, not
    // on the stub, which counts a request when it arrives.
    eventually(
        Duration::from_secs(15),
        "the idle worker refreshed the cache twice",
        async || {
            db.request_logs_for_session(&session_id)
                .await
                .expect("request logs")
                .len()
                >= 2
        },
    )
    .await;

    let rows = db
        .request_logs_for_session(&session_id)
        .await
        .expect("request logs");
    assert!(
        rows.iter().all(|row| row.keep_warm),
        "every logged request is one"
    );
    let report = super::assemble_usage_report(
        &session_id,
        &rows,
        &[],
        &crate::db::SessionIntervals::default(),
        None,
        0,
    );
    assert_eq!(
        report.models[0].keep_warm_requests,
        u64::try_from(rows.len()).expect("a count")
    );
    assert_eq!(
        report.models[0].requests,
        report.models[0].keep_warm_requests
    );
    assert_eq!(report.models[0].model, "session::session-model");

    // A turn takes the agent: the pending refresh goes with the worker.
    let (reply, agent) = async_channel::bounded(1);
    worker.release(reply);
    let _agent = agent.recv().await.expect("the worker hands the agent back");
    let after_release = stub.requests.lock().await.len();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        stub.requests.lock().await.len(),
        after_release,
        "no refresh is sent once a turn has the agent"
    );
}

/// A command that ends before the entry cools needs no refresh.
#[tokio::test]
async fn the_idle_worker_sends_nothing_when_every_command_ends_first() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let idle = idle_worker(&stub, Duration::from_secs(2)).await;
    let registry = idle.worker.job_registry().expect("a registry").clone();
    let _job = start_job(&registry, 1, Some(Duration::from_secs(1))).await;
    let sent = stub.requests.lock().await.len();

    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(stub.requests.lock().await.len(), sent);
}
