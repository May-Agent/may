//! Fan-out of live turns to every attached client.
//!
//! A turn runs in the daemon whether or not anyone watches it. Its frames
//! go to every `/api/events` subscriber of the session: `session_turn`
//! frames carry the web UI's chat events, `session_agent` frames the agent
//! events the TUI renders. The frames of the turn in progress are kept, so a
//! client that attaches mid-turn first receives everything it missed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

/// Capacity of each session's live channel; a subscriber that falls this
/// far behind refetches history instead.
const CHANNEL_CAPACITY: usize = 1024;

/// One frame for `/api/events`.
#[derive(Debug, Clone)]
pub(crate) struct TurnFrame {
    /// SSE event kind: `session_turn` or `session_agent`.
    pub(crate) kind: &'static str,
    /// JSON payload.
    pub(crate) data: Arc<str>,
}

#[derive(Debug)]
struct SessionChannel {
    tx: broadcast::Sender<Arc<TurnFrame>>,
    replay: Vec<Arc<TurnFrame>>,
}

impl SessionChannel {
    fn new() -> Self {
        Self {
            tx: broadcast::channel(CHANNEL_CAPACITY).0,
            replay: Vec::new(),
        }
    }
}

/// Live turn frames of every session.
#[derive(Debug, Default)]
pub(crate) struct TurnHub {
    sessions: Mutex<HashMap<String, SessionChannel>>,
}

/// A subscription to one session's turns.
#[derive(Debug)]
pub(crate) struct TurnSubscription {
    /// Frames of the turn in progress, oldest first.
    pub(crate) replay: Vec<Arc<TurnFrame>>,
    /// Frames from now on.
    pub(crate) live: broadcast::Receiver<Arc<TurnFrame>>,
}

impl TurnHub {
    fn with_session<R>(&self, session_id: &str, f: impl FnOnce(&mut SessionChannel) -> R) -> R {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let channel = sessions
            .entry(session_id.to_string())
            .or_insert_with(SessionChannel::new);
        f(channel)
    }

    /// Starts a new turn of `session_id`: the replay buffer restarts.
    pub(crate) fn begin_turn(&self, session_id: &str) {
        self.with_session(session_id, |channel| channel.replay.clear());
    }

    /// Publishes `frame` to the session's subscribers and keeps it for
    /// late ones until the turn ends.
    pub(crate) fn publish(&self, session_id: &str, frame: TurnFrame) {
        let frame = Arc::new(frame);
        self.with_session(session_id, |channel| {
            channel.replay.push(frame.clone());
            let _ = channel.tx.send(frame);
        });
    }

    /// Ends the session's turn; later subscribers read it from history.
    pub(crate) fn end_turn(&self, session_id: &str) {
        self.with_session(session_id, |channel| channel.replay.clear());
    }

    /// Subscribes to `session_id`.
    pub(crate) fn subscribe(&self, session_id: &str) -> TurnSubscription {
        self.with_session(session_id, |channel| TurnSubscription {
            replay: channel.replay.clone(),
            live: channel.tx.subscribe(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{TurnFrame, TurnHub};

    fn frame(data: &str) -> TurnFrame {
        TurnFrame {
            kind: "session_turn",
            data: data.into(),
        }
    }

    #[test]
    fn a_late_subscriber_replays_the_turn_and_then_follows_it() {
        let hub = TurnHub::default();
        hub.begin_turn("s");
        hub.publish("s", frame("start"));
        hub.publish("s", frame("text-1"));
        let mut late = hub.subscribe("s");
        assert_eq!(
            late.replay.iter().map(|f| &*f.data).collect::<Vec<_>>(),
            ["start", "text-1"]
        );
        hub.publish("s", frame("text-2"));
        assert_eq!(&*late.live.try_recv().expect("live frame").data, "text-2");
        hub.end_turn("s");
        assert!(hub.subscribe("s").replay.is_empty());
    }
}
