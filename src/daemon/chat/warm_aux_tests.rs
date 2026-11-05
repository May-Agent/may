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
    state_with_idle_agent_on(dir, ran_turn, &stub).await
}

/// [`state_with_idle_agent`] against `stub`.
async fn state_with_idle_agent_on(
    dir: &std::path::Path,
    ran_turn: bool,
    stub: &StubModel,
) -> (Arc<DaemonState>, ManualClock, tempfile::TempDir) {
    let config = saved_config(stub, None);
    let state = Arc::new(
        DaemonState::for_test(dir, config.clone())
            .await
            .expect("test daemon state"),
    );
    let mut session = Box::pin(Session::start(&config)).await;
    if ran_turn {
        run_turn(&mut session.agent).await;
    }
    let clock = session.clock.clone();
    state.parked_agents.park(
        &state,
        SESSION,
        session.agent,
        0,
        crate::session::RunReleased::already_released(),
    );
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
    state.aux_tasks.cancel(SESSION, || {});

    clock.advance(HOUR);
    super::suggest::maybe_suggest(&state, SESSION);
    assert!(
        !state.aux_tasks.is_current(SESSION, 1),
        "the cache went cold with the clock"
    );
}

#[tokio::test]
async fn disabling_suggestions_cancels_the_one_in_flight_and_stops_new_ones() {
    let dir = tempfile::tempdir().expect("dir");
    let (state, _clock, _session) = state_with_idle_agent(dir.path(), true).await;
    super::suggest::maybe_suggest(&state, SESSION);
    assert!(state.aux_tasks.is_current(SESSION, 0));

    state.update_config(|config| config.suggestions.enabled = false);
    super::apply_aux_settings(&state);
    assert!(!state.aux_tasks.is_current(SESSION, 0), "cancelled");

    super::suggest::maybe_suggest(&state, SESSION);
    assert!(!state.aux_tasks.is_current(SESSION, 1), "nothing starts");

    // Enabling again applies at once, too: the next request id is 1.
    state.update_config(|config| config.suggestions.enabled = true);
    super::suggest::maybe_suggest(&state, SESSION);
    assert!(state.aux_tasks.is_current(SESSION, 1));
}

#[tokio::test]
async fn disabling_auto_recap_drops_timers_and_automatic_generations_only() {
    let dir = tempfile::tempdir().expect("dir");
    let (state, _clock, _session) = state_with_idle_agent(dir.path(), true).await;
    let handle = tokio::runtime::Handle::current();
    super::recap::arm(&state, SESSION);
    assert!(state.recap_schedules.is_armed(SESSION));

    state.update_config(|config| config.recap.auto = false);
    super::apply_aux_settings(&state);
    assert!(!state.recap_schedules.is_armed(SESSION), "timer dropped");
    super::recap::arm(&state, SESSION);
    assert!(!state.recap_schedules.is_armed(SESSION), "nothing re-arms");

    // An automatic generation in flight is cancelled; a manual one is not.
    state.update_config(|config| config.recap.auto = true);
    let auto = state.aux_tasks.start(
        SESSION,
        super::aux::AuxKind::AutoRecap,
        |_| std::future::pending(),
        &handle,
    );
    state.update_config(|config| config.recap.auto = false);
    super::apply_aux_settings(&state);
    assert!(!state.aux_tasks.is_current(SESSION, auto));
    let manual = state.aux_tasks.start(
        SESSION,
        super::aux::AuxKind::ManualRecap,
        |_| std::future::pending(),
        &handle,
    );
    super::apply_aux_settings(&state);
    assert!(state.aux_tasks.is_current(SESSION, manual));
}

#[tokio::test]
async fn a_config_change_event_applies_the_settings_to_the_running_daemon() {
    let dir = tempfile::tempdir().expect("dir");
    let (state, _clock, _session) = state_with_idle_agent(dir.path(), true).await;
    super::watch_aux_settings(&state);
    super::recap::arm(&state, SESSION);
    assert!(state.recap_schedules.is_armed(SESSION));

    state.update_config(|config| config.recap.auto = false);
    state.notify_ui_events(crate::ui_event_bus::UiEventTopics::CONFIG);
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.recap_schedules.is_armed(SESSION) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the change disarms the timer without a restart");
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

const RECAP_TEXT: &str = "RECAP-SENTINEL where the work stands";
const SUGGESTION_TEXT: &str = "SUGGESTION-SENTINEL run the tests";
const RECAP_PROMPT: &str = "Write a catch-up note for the user";
const SUGGESTION_PROMPT: &str = "Predict the user's next message";

/// Answers the two aux prompts with valid output; any other request gets the
/// stub's default reply.
fn aux_replies(body: &str) -> Option<String> {
    if body.contains(RECAP_PROMPT) {
        Some(serde_json::json!({"summary": RECAP_TEXT, "next_action": null}).to_string())
    } else if body.contains(SUGGESTION_PROMPT) {
        Some(serde_json::json!({"suggestion": SUGGESTION_TEXT}).to_string())
    } else {
        None
    }
}

async fn wait_until(mut done: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !done().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the condition held within the bound");
}

/// A suggestion and a recap generated through the session's parked agent
/// reach neither its stored model history nor the next request it sends.
#[tokio::test]
async fn aux_generations_leave_the_conversation_and_the_next_request_untouched() {
    let dir = tempfile::tempdir().expect("dir");
    let stub = StubModel::spawn_replying(SESSION_MODEL, aux_replies).await;
    let (state, _clock, _session) = state_with_idle_agent_on(dir.path(), true, &stub).await;
    state
        .db()
        .create_session(SESSION, Some("test"), Some("a::gpt-4o"))
        .await
        .expect("session");
    for (role, content) in [
        (crate::session::MessageRole::User, "Say hello."),
        (crate::session::MessageRole::Assistant, "Hello there."),
    ] {
        state
            .db()
            .append_message(
                SESSION,
                role,
                content,
                None,
                crate::db::MessageMeta::default(),
            )
            .await
            .expect("append");
    }
    let stored_before = state.db().get_messages(SESSION).await.expect("rows").len();

    // The suggestion runs to its commit.
    super::suggest::maybe_suggest(&state, SESSION);
    wait_until(async || !state.aux_tasks.is_current(SESSION, 0)).await;
    assert!(
        stub.requests
            .lock()
            .await
            .iter()
            .any(|body| body.contains(SUGGESTION_PROMPT)),
        "the suggestion asked the session model"
    );

    // The recap runs to its stored card.
    super::recap::request_recap(&state, SESSION, super::aux::AuxKind::ManualRecap);
    wait_until(async || {
        state.db().get_messages(SESSION).await.expect("rows").len() > stored_before
    })
    .await;

    // The only new row is the UI-only recap card.
    let rows = state.db().get_messages(SESSION).await.expect("rows");
    let roles: Vec<_> = rows.iter().map(|row| row.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "recap"]);
    assert!(
        rows.iter()
            .all(|row| !row.content.contains(SUGGESTION_TEXT)),
        "a suggestion is never stored"
    );

    // Rebuilding the model history from the database skips the card.
    let history = crate::session::load_history_from_db(&state.db(), SESSION)
        .await
        .expect("history");
    let replayed = format!("{:?}", history.to_aither_messages().expect("replay"));
    for leaked in [RECAP_TEXT, SUGGESTION_TEXT, RECAP_PROMPT, SUGGESTION_PROMPT] {
        assert!(!replayed.contains(leaked), "model history holds {leaked:?}");
    }

    // The next real request through the same agent carries none of it.
    let worker = state.parked_agents.get(SESSION).expect("parked agent");
    let guard = state
        .runtime_service
        .runs()
        .acquire(SESSION)
        .await
        .expect("acquire run");
    let handle = worker.run(
        "Say hello again.".to_string(),
        None,
        Vec::new(),
        crate::agent::RunLease::of(&guard),
    );
    while let Some(event) = handle.recv().await {
        if matches!(event, crate::agent::TuiEvent::Complete) {
            break;
        }
        assert!(
            !matches!(event, crate::agent::TuiEvent::Error(_)),
            "the turn failed: {event:?}"
        );
    }
    let requests = stub.requests.lock().await;
    let next = requests
        .iter()
        .rev()
        .find(|body| body.contains("Say hello again."))
        .expect("the next turn reached the model");
    for leaked in [RECAP_TEXT, SUGGESTION_TEXT, RECAP_PROMPT, SUGGESTION_PROMPT] {
        assert!(!next.contains(leaked), "the next request holds {leaked:?}");
    }
    drop(requests);
    guard
        .finish_idle(crate::session::RunOutcome::Completed)
        .await
        .expect("release run");
}

/// A stored recap card is read back from a freshly opened database, as after
/// a daemon restart, and replays to clients as a recap row.
#[tokio::test]
async fn a_recap_card_survives_reopening_the_database() {
    let dir = tempfile::tempdir().expect("dir");
    let stub = StubModel::spawn_replying(SESSION_MODEL, aux_replies).await;
    let (state, _clock, _session) = state_with_idle_agent_on(dir.path(), true, &stub).await;
    state
        .db()
        .create_session(SESSION, Some("test"), Some("a::gpt-4o"))
        .await
        .expect("session");
    state
        .db()
        .append_message(
            SESSION,
            crate::session::MessageRole::User,
            "Say hello.",
            None,
            crate::db::MessageMeta::default(),
        )
        .await
        .expect("append");
    super::recap::request_recap(&state, SESSION, super::aux::AuxKind::AutoRecap);
    wait_until(async || state.db().get_messages(SESSION).await.expect("rows").len() == 2).await;

    // A new connection to the same file: nothing is held in memory.
    let reopened = crate::db::ChatDb::open(&dir.path().join("history.db"))
        .await
        .expect("reopen");
    let history = crate::session::load_history_from_db(&reopened, SESSION)
        .await
        .expect("history");
    let card = history
        .messages()
        .iter()
        .find(|message| message.role == crate::session::MessageRole::Recap)
        .expect("the recap row is reloaded");
    let record: crate::daemon::RecapRecord =
        serde_json::from_str(&card.content).expect("the row parses as a recap record");
    assert_eq!(record.summary, RECAP_TEXT);
    assert_eq!(record.turns, 1);
}
