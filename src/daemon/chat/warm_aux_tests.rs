//! Prompt suggestion and the idle recap issue a request only while the
//! session's prompt cache is warm. Time is a [`ManualClock`] the agent's
//! cache warmth reads, advanced instead of slept.

use std::sync::Arc;
use std::time::Duration;

use aither_agent::ManualClock;

use crate::agent::idle_compaction_tests::{
    SESSION_MODEL, Session, StubModel, run_turn, saved_config,
};
use crate::daemon::state::DaemonState;

const SESSION: &str = "session";
const HOUR: Duration = Duration::from_hours(1);

/// A daemon state parking an idle agent for [`SESSION`]. `ran_turn` decides
/// whether the agent has sent a request, which is what warms its cache.
/// The returned directory is the agent's scratch space and must outlive it.
async fn state_with_idle_agent(
    dir: &std::path::Path,
    ran_turn: bool,
) -> (Arc<DaemonState>, ManualClock, tempfile::TempDir) {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let config = saved_config(&stub, None);
    let state = Arc::new(
        DaemonState::for_test(dir, dir, config.clone())
            .await
            .expect("test daemon state"),
    );
    let mut session = Box::pin(Session::start(&config)).await;
    if ran_turn {
        run_turn(&mut session.agent).await;
    }
    let clock = session.clock.clone();
    state.parked_agents.park(&state, SESSION, session.agent, 0);
    (state, clock, session.temp)
}

#[tokio::test]
async fn suggestion_is_skipped_for_a_cold_cache() {
    let dir = tempfile::tempdir().expect("dir");
    let (state, _clock, _session) = state_with_idle_agent(dir.path(), false).await;
    assert!(super::aux::warm_cache(&state, SESSION).is_none());

    super::suggest::maybe_suggest(&state, SESSION);
    assert!(
        !state.aux_tasks.is_current(SESSION, 0),
        "no aux request starts against a cold cache"
    );
}

#[tokio::test]
async fn suggestion_runs_for_a_warm_cache_until_it_goes_cold() {
    let dir = tempfile::tempdir().expect("dir");
    let (state, clock, _session) = state_with_idle_agent(dir.path(), true).await;

    super::suggest::maybe_suggest(&state, SESSION);
    assert!(
        state.aux_tasks.is_current(SESSION, 0),
        "a warm cache starts the aux request"
    );
    state.aux_tasks.cancel(SESSION);

    clock.advance(HOUR);
    super::suggest::maybe_suggest(&state, SESSION);
    assert!(
        !state.aux_tasks.is_current(SESSION, 1),
        "the cache went cold with the clock"
    );
}

#[tokio::test]
async fn recap_idle_deadline_skips_a_cache_that_went_cold() {
    let dir = tempfile::tempdir().expect("dir");
    let (state, clock, _session) = state_with_idle_agent(dir.path(), true).await;
    let warmth = super::aux::warm_cache(&state, SESSION).expect("warm after a turn");
    let idle = Duration::from_mins(30);

    // The deadline passes inside the cache lifetime: the recap may run.
    let slept = |by: Duration| {
        clock.advance(by);
        std::future::ready(())
    };
    assert!(super::recap::idle_with_warm_cache(&warmth, idle, slept).await);

    // The machine slept through the rest of the hour: the timer fires late,
    // and the warmth clock, which counted the sleep, reports it cold.
    let slept = |by: Duration| {
        clock.advance(by);
        std::future::ready(())
    };
    assert!(!super::recap::idle_with_warm_cache(&warmth, idle, slept).await);
}
