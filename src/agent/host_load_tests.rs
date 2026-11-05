//! The host load alert of a main session, from the `[host_load]` settings
//! of a saved config through May's agent construction to the requests the
//! model receives, with the host read from a scripted source.

use std::time::{Duration, Instant};

use super::idle_compaction_tests::{SESSION_MODEL, Session, StubModel, run_turn, saved_config};

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
async fn mentions(stub: &StubModel, text: &str) -> usize {
    stub.requests
        .lock()
        .await
        .last()
        .expect("a request")
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
    run_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 0, "a calm host is quiet");

    session.parts.host.set_load(2.5);
    settle(&session).await;
    run_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 1);
    assert_eq!(
        mentions(&stub, "waits up to 75s for a command in the foreground").await,
        1,
        "the configured foreground window reaches the alert"
    );

    // The load persists: the alert is not restated.
    settle(&session).await;
    run_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 1);

    session.parts.host.set_load(0.1);
    settle(&session).await;
    run_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_ENDED).await, 1);
    assert_eq!(mentions(&stub, LOAD_HIGH).await, 1);

    settle(&session).await;
    run_turn(&mut session.agent).await;
    assert_eq!(mentions(&stub, LOAD_ENDED).await, 1);
}
