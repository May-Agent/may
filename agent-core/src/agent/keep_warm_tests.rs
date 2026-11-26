//! Keeping the idle agent's prompt cache warm while a background command
//! outlives it, from a saved config through May's agent construction to
//! aither, against a stub model server.
//!
//! The scheduling decision is read off the agent on a [`ManualClock`]; the
//! idle worker's firing, its cancellation and the usage it records run on
//! the system clock with a cache lifetime of a few seconds, since the worker
//! waits on real timers.

use std::time::Duration;

use super::idle_compaction_tests::{
    SESSION_MODEL, Session, StubModel, run_short_turn, saved_config,
};
use super::runner::{AgentWorker, spawn_worker};
use crate::MayAgent;
use crate::agent::{ModelSlot, ProviderLlm};
use crate::cache_lifetime::CacheLifetime;
use crate::config::MayConfig;

/// Half a minute short of the default one-hour lifetime.
const DUE: Duration = Duration::from_secs(59 * 60 + 30);

/// A deadline past every cache lifetime the tests use and past the default
/// cap on a command's `timeout`, which [`config_allowing_long_commands`] lifts.
const LONG: Duration = Duration::from_hours(3);

/// A saved config whose shell lets a command run for [`LONG`].
fn config_allowing_long_commands(stub: &StubModel) -> MayConfig {
    let mut config = saved_config(stub, None);
    config.shell.timeout_cap = Duration::from_hours(4);
    config
}

/// Starts a running job on the session's shell, killed once `timeout` has
/// passed since it started.
async fn start_job(shell: &crate::ipc::SessionShell, timeout: Duration) -> aither_sandbox::Job {
    shell
        .run_background(aither_sandbox::ShellArgs {
            description: "sleeps".to_string(),
            script: "sleep 9999".to_string(),
            interpreter: aither_sandbox::Interpreter::default(),
            sandbox: aither_sandbox::SandboxMode::Sandboxed,
            location: None,
            resolution: aither_sandbox::MediaResolution::default(),
            max_lines: None,
            raw: false,
            timeout: timeout.as_secs(),
            background: true,
        })
        .await
        .expect("the job starts")
}

fn config_with_lifetime(stub: &StubModel, lifetime: CacheLifetime) -> MayConfig {
    let mut config = config_allowing_long_commands(stub);
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
    let mut session = Box::pin(Session::start(&config_allowing_long_commands(&stub))).await;
    let shell = session.agent.shell().clone();

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

    let _short = start_job(&shell, Duration::from_mins(30)).await;
    assert_eq!(
        session.agent.keep_warm_due_in().await,
        None,
        "the only command ends before the entry cools"
    );

    let _long = start_job(&shell, LONG).await;
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
    let mut session = Box::pin(Session::start(&config_allowing_long_commands(&stub))).await;
    run_short_turn(&mut session.agent).await;

    // A command's deadline is bounded by the shell's cap; one at the cap is
    // past the cold point.
    let _job = start_job(session.agent.shell(), LONG).await;
    assert_eq!(session.agent.keep_warm_due_in().await, Some(DUE));
}

/// A refresh is one more request that leaves the conversation as it was and
/// restarts the entry's lifetime.
#[tokio::test]
async fn a_refresh_sends_a_request_and_leaves_the_conversation_alone() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let mut session = Box::pin(Session::start(&config_allowing_long_commands(&stub))).await;
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
    let _job = start_job(session.agent.shell(), LONG).await;
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
    worker: AgentWorker<ModelSlot<ProviderLlm>, MayAgent>,
    db: crate::db::ChatDb,
    session_id: String,
    _job: aither_sandbox::Job,
    _temp: tempfile::TempDir,
}

/// An idle worker whose session runs a command killed `deadline` after it
/// started, under a cache lifetime of `lifetime`.
async fn idle_worker(stub: &StubModel, lifetime: Duration, deadline: Duration) -> Idle {
    let config = config_with_lifetime(stub, CacheLifetime::Limited(lifetime));
    let Session {
        mut agent,
        parts,
        temp,
        ..
    } = Box::pin(Session::start_on_system_clock(&config)).await;
    run_short_turn(&mut agent).await;
    let job = start_job(agent.shell(), deadline).await;
    let db = parts.db.clone();
    let runs = crate::session::SessionRuns::new(db.clone(), crate::ui_event_bus::UiEventBus::new());
    Idle {
        worker: spawn_worker(executor_core::DefaultExecutor, agent, db.clone(), runs),
        db,
        session_id: parts.session_id.as_str().to_string(),
        _job: job,
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
        _job,
        _temp,
    } = idle_worker(&stub, Duration::from_secs(2), LONG).await;
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
    let _idle = idle_worker(&stub, Duration::from_secs(2), Duration::from_secs(1)).await;
    let sent = stub.requests.lock().await.len();

    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(stub.requests.lock().await.len(), sent);
}

#[derive(Debug)]
struct NoopSidekickExecutor;

impl crate::fusion::SidekickExecutor for NoopSidekickExecutor {
    fn execute(
        &self,
        _request: crate::fusion::SidekickRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<crate::fusion::SidekickOutcome, String>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async {
            Ok(crate::fusion::SidekickOutcome::Completed {
                report: String::new(),
                diff: String::new(),
            })
        })
    }
}

fn fusion_service() -> crate::fusion::FusionService {
    crate::fusion::FusionService::new(
        crate::fusion::FusionPairRegistry::default(),
        std::sync::Arc::new(NoopSidekickExecutor),
    )
}

#[allow(clippy::too_many_arguments)]
async fn build_fusion_agent(
    config: &MayConfig,
    live_config: Option<std::sync::Arc<arc_swap::ArcSwap<MayConfig>>>,
    parts: &super::idle_compaction_tests::SessionParts,
    fusion: crate::fusion::FusionService,
    role: crate::fusion::SessionRole,
    execution_mode: crate::session::ExecutionMode,
    clock: Option<&aither_agent::ManualClock>,
    override_lifetime: Option<CacheLifetime>,
) -> MayAgent {
    let mut input = parts.input().await;
    input.execution_mode = execution_mode;
    input.session_role = role;
    input.live_config = live_config;
    input.limits = crate::agent::LiveLimits::of(config);
    input.services.set_fusion(fusion);
    let mut agent_config = MayAgent::session_config(config, input)
        .await
        .expect("fusion session config resolves");
    if let Some(lifetime) = override_lifetime {
        agent_config.cache_lifetime = lifetime;
    }
    agent_config.clock = clock.cloned();
    agent_config.host_reader = Some(parts.host.source());
    Box::pin(MayAgent::new(agent_config))
        .await
        .expect("fusion agent builds")
}

/// Fusion lead and sidekick both promote a limited provider cache lifetime to
/// 1 hour (with matching 1h Claude prompt-cache TTL), while permanent-cache
/// backends stay permanent; the unified pair predicate gates `keep_warm_plan`
/// on peer turns, background jobs, and open `ask_user` waits within the live-
/// configurable grace window.
#[tokio::test]
async fn fusion_mode_configures_one_hour_ttl_and_unified_keep_warm_predicate() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let temp = tempfile::tempdir().expect("tempdir");
    let parts = super::idle_compaction_tests::SessionParts::new(temp.path().to_path_buf()).await;
    parts
        .db
        .create_session(
            parts.session_id.as_str(),
            Some("Lead"),
            Some("session::session-model"),
        )
        .await
        .expect("create lead session");

    let fusion_mode = crate::session::ExecutionMode::Fusion {
        sidekick_model: crate::ModelSelector::new("session", SESSION_MODEL),
    };
    let config = config_with_lifetime(&stub, CacheLifetime::Limited(Duration::from_mins(5)));
    let live_config = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(config.clone()));
    let service = fusion_service();
    let clock = aither_agent::ManualClock::new();

    let mut lead = build_fusion_agent(
        &config,
        Some(live_config.clone()),
        &parts,
        service.clone(),
        crate::fusion::SessionRole::Main,
        fusion_mode.clone(),
        Some(&clock),
        None,
    )
    .await;
    assert_eq!(
        lead.cache_lifetime(),
        aither_agent::context::CacheLifetime::Limited(Duration::from_hours(1)),
        "fusion lead promotes 5m limited cache to 1h"
    );
    assert_eq!(
        lead.claude_cache_ttl(),
        aither_core::llm::model::ClaudePromptCacheTtl::OneHour,
        "fusion lead requests 1h Claude prompt-cache TTL"
    );

    let sidekick = build_fusion_agent(
        &config,
        Some(live_config.clone()),
        &parts,
        service.clone(),
        crate::fusion::SessionRole::FusionSidekick {
            parent_session_id: parts.session_id.clone(),
            parent_sandbox_dir: temp.path().join("sandbox"),
        },
        crate::session::ExecutionMode::Standard,
        Some(&clock),
        None,
    )
    .await;
    assert_eq!(
        sidekick.cache_lifetime(),
        aither_agent::context::CacheLifetime::Limited(Duration::from_hours(1)),
        "fusion sidekick promotes 5m limited cache to 1h"
    );
    assert_eq!(
        sidekick.claude_cache_ttl(),
        aither_core::llm::model::ClaudePromptCacheTtl::OneHour,
        "fusion sidekick requests 1h Claude prompt-cache TTL"
    );

    // Permanent-cache backends stay permanent in Fusion mode.
    let perm_config = config_with_lifetime(&stub, CacheLifetime::Permanent);
    let perm_lead = build_fusion_agent(
        &perm_config,
        None,
        &parts,
        service.clone(),
        crate::fusion::SessionRole::Main,
        fusion_mode,
        Some(&clock),
        None,
    )
    .await;
    assert_eq!(
        perm_lead.cache_lifetime(),
        aither_agent::context::CacheLifetime::Permanent,
        "permanent cache backend is skipped by 1h override"
    );

    // Warm the lead's cache entry above compaction_floor.
    super::idle_compaction_tests::run_turn(&mut lead).await;
    assert_eq!(
        lead.keep_warm_plan().await.refresh_in,
        None,
        "idle pair with no jobs, turns, or open ask_user does not keep warm"
    );
    assert!(
        lead.idle_compaction_due_in().is_some(),
        "idle compaction is ready when keep-warm predicate is false"
    );

    // Peer sidekick active turn activates lead keep-warm.
    let pair = service.pairs().for_parent(parts.session_id.as_str());
    let active_sidekick = pair.begin_sidekick_turn();
    assert_eq!(lead.keep_warm_plan().await.refresh_in, Some(DUE));
    drop(active_sidekick);
    assert_eq!(lead.keep_warm_plan().await.refresh_in, None);

    // Sidekick background job activates lead keep-warm.
    let sidekick_job = start_job(sidekick.shell(), LONG).await;
    assert_eq!(lead.keep_warm_plan().await.refresh_in, Some(DUE));
    drop(sidekick_job);
    let _ = sidekick.shell().jobs().kill_all().await;
    eventually(
        Duration::from_secs(5),
        "sidekick job terminates",
        async || lead.keep_warm_plan().await.refresh_in.is_none(),
    )
    .await;

    // Open ask_user younger than the live-configurable grace window activates
    // keep-warm with a recheck timer, and stops as soon as the live grace window
    // is tightened below the request's age.
    let ask_req = may_protocol::ask_user::PersistedAskUserRequest {
        id: "ask-fusion-1".to_string(),
        questions: vec![may_protocol::ask_user::PersistedAskUserQuestion {
            section: "Confirm".to_string(),
            question: "Proceed?".to_string(),
            option: vec!["Yes".to_string(), "No".to_string()],
            multi_select: false,
            secret: None,
        }],
    };
    parts
        .db
        .set_waiting_ask_user(parts.session_id.as_str(), Some(ask_req))
        .await
        .expect("insert open ask_user");
    let plan = lead.keep_warm_plan().await;
    assert_eq!(plan.refresh_in, Some(DUE));
    assert!(
        plan.recheck_in.is_some(),
        "grace-bounded ask_user schedules an earlier recheck before 59m30s"
    );

    // Tighten live grace window to 0s: predicate flips false immediately without restart.
    let mut tightened = config.clone();
    tightened.experimental.fusion_keep_warm_grace = Duration::ZERO;
    live_config.store(std::sync::Arc::new(tightened));
    assert_eq!(
        lead.keep_warm_plan().await.refresh_in,
        None,
        "expired grace window stops keep-warm immediately"
    );
}

/// While the Fusion keep-warm predicate is active, `runner::next_command`
/// suppresses `idle_compaction_due_in` (so 54m compaction never pre-empts
/// 59m30s cache refresh). As soon as the predicate flips false, `next_command`
/// wakes and runs `compact_when_idle` while the cache is still warm.
#[tokio::test]
async fn fusion_idle_worker_suppresses_early_compaction_then_hands_off_when_predicate_clears() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let temp = tempfile::tempdir().expect("tempdir");
    let parts = super::idle_compaction_tests::SessionParts::new(temp.path().to_path_buf()).await;
    parts
        .db
        .create_session(
            parts.session_id.as_str(),
            Some("Lead"),
            Some("session::session-model"),
        )
        .await
        .expect("create lead session");

    let fusion_mode = crate::session::ExecutionMode::Fusion {
        sidekick_model: crate::ModelSelector::new("session", SESSION_MODEL),
    };
    let config = config_with_lifetime(&stub, CacheLifetime::Limited(Duration::from_secs(3)));
    let service = fusion_service();
    let mut lead = build_fusion_agent(
        &config,
        None,
        &parts,
        service.clone(),
        crate::fusion::SessionRole::Main,
        fusion_mode,
        None,
        Some(CacheLifetime::Limited(Duration::from_secs(3))),
    )
    .await;

    // Seed history above compaction_floor so idle_compaction_due_in is armed.
    super::idle_compaction_tests::run_turn(&mut lead).await;
    assert!(
        lead.idle_compaction_due_in().is_some(),
        "idle compaction would fire at 90% of lifetime if not suppressed"
    );

    // Hold sidekick active turn so unified predicate keeps lead warm.
    let pair = service.pairs().for_parent(parts.session_id.as_str());
    let active_sidekick = pair.begin_sidekick_turn();

    let db = parts.db.clone();
    let session_id = parts.session_id.as_str().to_string();
    let runs = crate::session::SessionRuns::new(db.clone(), crate::ui_event_bus::UiEventBus::new());
    let _worker = spawn_worker(executor_core::DefaultExecutor, lead, db.clone(), runs);

    // Wait for at least 2 keep-warm refreshes (crossing past the 90% idle-compaction mark).
    eventually(
        Duration::from_secs(15),
        "fusion idle worker refreshed the cache twice",
        async || {
            db.request_logs_for_session(&session_id)
                .await
                .expect("request logs")
                .iter()
                .filter(|row| row.keep_warm)
                .count()
                >= 2
        },
    )
    .await;

    {
        let has_compaction = stub
            .requests
            .lock()
            .await
            .iter()
            .any(|body| body.contains(super::idle_compaction_tests::COMPACTION_MARKER));
        assert!(
            !has_compaction,
            "idle compaction must not pre-empt keep-warm while predicate is active"
        );
    }

    // Drop the active sidekick turn: predicate flips false and pair.notify_changed()
    // wakes runner::next_command so compact_when_idle fires while the cache is still warm.
    drop(active_sidekick);
    eventually(
        Duration::from_secs(10),
        "idle compaction ran after keep-warm predicate turned false",
        async || {
            stub.requests
                .lock()
                .await
                .iter()
                .any(|body| body.contains(super::idle_compaction_tests::COMPACTION_MARKER))
        },
    )
    .await;
}
