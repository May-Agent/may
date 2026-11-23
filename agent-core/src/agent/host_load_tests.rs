//! The host load alert of a main session, from the `[host_load]` settings
//! of a saved config through May's agent construction to the requests the
//! model receives, with the host read from a scripted source.

use std::time::{Duration, Instant};

use super::idle_compaction_tests::{
    COMPACTION_MARKER, COMPACTION_MODEL, SESSION_MODEL, Session, StubModel, run_short_turn,
    saved_config,
};

const LOAD_HIGH: &str = "The machine load is high";
const LOAD_ENDED: &str = "The machine load has ended";

/// Waits until the monitor has read the host twice more, so a reading taken
/// after the change has been observed and its event sent.
async fn settle(session: &Session) {
    let target = session.parts.host.reads() + 2;
    let deadline = Instant::now() + Duration::from_secs(10);
    while session.parts.host.reads() < target {
        assert!(Instant::now() < deadline, "the monitor reads the host");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// How often `text` occurs in the last request the model received.
///
/// The session stays under the compaction floor: a compaction landing
/// between two turns would fold the alerts the model already saw into its
/// summary, and whether one lands before the next request is timing.
async fn mentions(stub: &StubModel, text: &str) -> usize {
    let requests = stub.requests.lock().await;
    assert!(
        !requests
            .iter()
            .any(|request| request.contains(COMPACTION_MARKER)),
        "the session never compacts"
    );
    requests
        .last()
        .expect("a turn request")
        .matches(text)
        .count()
}

#[tokio::test]
async fn the_load_alert_fires_once_on_entering_and_once_on_leaving_high_load() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let mut config = saved_config(&stub, None);
    config.host_load.load_threshold = 1.0;
    config.host_load.start_after = Duration::ZERO;
    config.host_load.end_after = Duration::ZERO;
    config.host_load.poll_interval = Duration::from_millis(5);
    config.host_load.high_load_foreground_wait = Duration::from_secs(75);
    let mut session = Box::pin(Session::start(&config)).await;

    settle(&session).await;
    run_short_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 0, "a calm host is quiet");

    session.parts.host.set_load(2.5);
    settle(&session).await;
    run_short_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 1);
    assert_eq!(
        mentions(&stub, "waits up to 75s for a command in the foreground").await,
        1,
        "the configured foreground window reaches the alert"
    );

    // The load persists: the alert is not restated.
    settle(&session).await;
    run_short_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 1);

    session.parts.host.set_load(0.1);
    settle(&session).await;
    run_short_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_ENDED).await, 1);
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 1);

    settle(&session).await;
    run_short_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_ENDED).await, 1);
}

#[tokio::test]
async fn red_then_green_while_idle_emits_only_per_seen_state_and_starts_no_turn() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let mut config = saved_config(&stub, None);
    config.host_load.load_threshold = 1.0;
    config.host_load.start_after = Duration::ZERO;
    config.host_load.end_after = Duration::ZERO;
    config.host_load.poll_interval = Duration::from_millis(5);
    let mut session = Box::pin(Session::start(&config)).await;

    // Baseline normal state seen by the model.
    settle(&session).await;
    run_short_turn(&mut session.agent).await;
    assert_eq!(stub.requests.lock().await.len(), 1);

    // Normal -> Red -> Green while idle: no turn starts, and returning to
    // the normal state the model already assumed produces no alert.
    session.parts.host.set_load(2.5);
    settle(&session).await;
    session.parts.host.set_load(0.1);
    settle(&session).await;
    assert_eq!(
        stub.requests.lock().await.len(),
        1,
        "state transitions while idle never start a turn"
    );
    run_short_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 0);
    assert_eq!(mentions(&stub, LOAD_ENDED).await, 0);

    // Now the model sees Red, then the host returns to Green while idle:
    // the next request carries one green alert.
    session.parts.host.set_load(2.5);
    settle(&session).await;
    run_short_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 1);

    session.parts.host.set_load(0.1);
    settle(&session).await;
    assert_eq!(
        stub.requests.lock().await.len(),
        3,
        "no turn started while idle"
    );
    run_short_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_ENDED).await, 1);
}

#[tokio::test]
async fn cold_reassembly_drops_superseded_state_alerts() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let compaction = StubModel::spawn(COMPACTION_MODEL).await;
    let mut config = saved_config(&stub, Some(&compaction));
    config.host_load.load_threshold = 1.0;
    config.host_load.start_after = Duration::ZERO;
    config.host_load.end_after = Duration::ZERO;
    config.host_load.poll_interval = Duration::from_millis(5);
    let mut session = Box::pin(Session::start(&config)).await;

    session.parts.host.set_load(2.5);
    settle(&session).await;
    run_short_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 1);

    session.parts.host.set_load(0.1);
    settle(&session).await;
    run_short_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 1);
    assert_eq!(mentions(&stub, LOAD_ENDED).await, 1);

    // Cold cache reassembly while normal drops earlier red/green alerts.
    session.clock.advance(Duration::from_mins(61));
    run_short_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 0);
    assert_eq!(mentions(&stub, LOAD_ENDED).await, 0);

    // Cold cache reassembly while still Red drops superseded history and keeps one fresh Red alert.
    session.parts.host.set_load(2.5);
    settle(&session).await;
    run_short_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 1);

    session.clock.advance(Duration::from_mins(61));
    run_short_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 1);
    assert_eq!(mentions(&stub, LOAD_ENDED).await, 0);
}

#[tokio::test]
async fn background_completion_after_interrupt_starts_no_turn_and_reaches_next_user_message() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let config = saved_config(&stub, None);
    let mut session = Box::pin(Session::start(&config)).await;

    // User interrupts the idle/previous work before a background command finishes.
    session.agent.interrupt();

    // Launch a quick detached background job directly on the session shell.
    let _job = session
        .agent
        .shell()
        .run_background(aither_sandbox::ShellArgs {
            description: "echo after interrupt".to_string(),
            script: "echo after-interrupt-done".to_string(),
            interpreter: aither_sandbox::Interpreter::default(),
            sandbox: aither_sandbox::SandboxMode::Sandboxed,
            location: None,
            resolution: aither_sandbox::MediaResolution::default(),
            max_lines: None,
            raw: false,
            timeout: 10,
            background: true,
        })
        .await
        .expect("command spawns");

    let wake = loop {
        let wake = tokio::time::timeout(Duration::from_secs(5), session.agent.wait_background())
            .await
            .expect("background event arrives");
        assert!(
            !wake.starts_run,
            "a background completion after an interrupt starts no turn"
        );
        if !wake.pending.is_empty() {
            break wake;
        }
    };
    drop(wake);

    // The next user turn clears the interrupt and delivers the recorded alert.
    run_short_turn(&mut session.agent).await;
    let last = stub
        .requests
        .lock()
        .await
        .last()
        .cloned()
        .expect("user turn request");
    assert!(
        last.contains("after-interrupt-done"),
        "next user message carries the background result: {last}"
    );
}
