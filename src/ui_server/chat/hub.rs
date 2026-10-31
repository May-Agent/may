//! Fan-out of live turns to every attached client.
//!
//! A turn runs in the daemon whether or not anyone watches it. Its frames
//! go to every `/api/events` subscriber of the session: `session_turn`
//! frames carry the web UI's chat events, `session_agent` frames the agent
//! events the TUI renders. The frames of the turn in progress are kept, so a
//! client that attaches mid-turn first receives everything it missed.
//!
//! Every frame of a session carries an index, increasing for as long as the
//! daemon runs. A client that reconnects, or a subscriber that fell behind,
//! names the last index it saw and receives only what came after; when the
//! frames it missed are gone (their turn ended), the subscription says so
//! and the client reloads the session instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

/// Capacity of each session's live channel; a subscriber that falls this
/// far behind catches up from the replay of the turn in progress.
const CHANNEL_CAPACITY: usize = 1024;

/// One frame for `/api/events`.
#[derive(Debug, Clone)]
pub struct TurnFrame {
    /// Position of the frame among the session's frames, from 1.
    pub(crate) index: u64,
    /// SSE event kind: `session_turn` or `session_agent`.
    pub(crate) kind: &'static str,
    /// JSON payload.
    pub(crate) data: Arc<str>,
}

#[derive(Debug)]
struct SessionChannel {
    tx: broadcast::Sender<Arc<TurnFrame>>,
    replay: Vec<Arc<TurnFrame>>,
    /// Index of the newest frame published; 0 before the first.
    last_index: u64,
}

impl SessionChannel {
    fn new() -> Self {
        Self {
            tx: broadcast::channel(CHANNEL_CAPACITY).0,
            replay: Vec::new(),
            last_index: 0,
        }
    }
}

/// Live turn frames of every session.
#[derive(Debug, Default)]
pub struct TurnHub {
    sessions: Mutex<HashMap<String, SessionChannel>>,
}

/// A subscription to one session's turns.
#[derive(Debug)]
pub struct TurnSubscription {
    /// Frames of the turn in progress the subscriber has not seen, oldest
    /// first. After a gap, the whole turn in progress.
    pub(crate) replay: Vec<Arc<TurnFrame>>,
    /// Frames from now on.
    pub(crate) live: broadcast::Receiver<Arc<TurnFrame>>,
    /// Whether frames after the index the subscriber named are gone: their
    /// turn ended, so the client must reload the session.
    pub(crate) gap: bool,
}

impl TurnHub {
    fn with_session<R>(&self, session_id: &str, f: impl FnOnce(&mut SessionChannel) -> R) -> R {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = f(sessions
            .entry(session_id.to_string())
            .or_insert_with(SessionChannel::new));
        drop(sessions);
        result
    }

    /// Starts a new turn of `session_id`: the replay buffer restarts.
    pub(crate) fn begin_turn(&self, session_id: &str) {
        self.with_session(session_id, |channel| channel.replay.clear());
    }

    /// Publishes a `kind` frame with `data` to the session's subscribers and
    /// keeps it for late ones until the turn ends.
    pub(crate) fn publish(&self, session_id: &str, kind: &'static str, data: Arc<str>) {
        self.with_session(session_id, |channel| {
            channel.last_index += 1;
            let frame = Arc::new(TurnFrame {
                index: channel.last_index,
                kind,
                data,
            });
            channel.replay.push(frame.clone());
            let _ = channel.tx.send(frame);
        });
    }

    /// Ends the session's turn; later subscribers read it from history.
    pub(crate) fn end_turn(&self, session_id: &str) {
        self.with_session(session_id, |channel| channel.replay.clear());
    }

    /// Subscribes to `session_id`. `after` is the index of the last frame
    /// the subscriber saw; without it the turn in progress replays from its
    /// start.
    pub(crate) fn subscribe(&self, session_id: &str, after: Option<u64>) -> TurnSubscription {
        self.with_session(session_id, |channel| {
            let Some(after) = after else {
                return TurnSubscription {
                    replay: channel.replay.clone(),
                    live: channel.tx.subscribe(),
                    gap: false,
                };
            };
            // The frames after `after` are all still here when there are
            // none, or the turn in progress began no later than the first.
            let gap = after < channel.last_index
                && channel
                    .replay
                    .first()
                    .is_none_or(|first| first.index > after + 1);
            let replay = if gap {
                channel.replay.clone()
            } else {
                channel
                    .replay
                    .iter()
                    .filter(|frame| frame.index > after)
                    .cloned()
                    .collect()
            };
            TurnSubscription {
                replay,
                live: channel.tx.subscribe(),
                gap,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::TurnHub;

    fn data(subscription: &super::TurnSubscription) -> Vec<&str> {
        subscription
            .replay
            .iter()
            .map(|frame| &*frame.data)
            .collect()
    }

    #[test]
    fn a_late_subscriber_replays_the_turn_and_then_follows_it() {
        let hub = TurnHub::default();
        hub.begin_turn("s");
        hub.publish("s", "session_turn", "start".into());
        hub.publish("s", "session_turn", "text-1".into());
        let mut late = hub.subscribe("s", None);
        assert_eq!(data(&late), ["start", "text-1"]);
        hub.publish("s", "session_turn", "text-2".into());
        assert_eq!(&*late.live.try_recv().expect("live frame").data, "text-2");
        hub.end_turn("s");
        assert!(hub.subscribe("s", None).replay.is_empty());
    }

    #[test]
    fn a_returning_subscriber_gets_only_what_it_missed() {
        let hub = TurnHub::default();
        hub.begin_turn("s");
        hub.publish("s", "session_turn", "start".into());
        hub.publish("s", "session_turn", "text-1".into());
        hub.publish("s", "session_turn", "text-2".into());
        let back = hub.subscribe("s", Some(2));
        assert!(!back.gap);
        assert_eq!(data(&back), ["text-2"]);
        let current = hub.subscribe("s", Some(3));
        assert!(!current.gap);
        assert!(current.replay.is_empty());
    }

    #[test]
    fn frames_of_an_ended_turn_are_a_gap() {
        let hub = TurnHub::default();
        hub.begin_turn("s");
        hub.publish("s", "session_turn", "start".into());
        hub.publish("s", "session_turn", "done".into());
        hub.end_turn("s");
        hub.begin_turn("s");
        hub.publish("s", "session_turn", "next-start".into());
        // The subscriber saw only the first frame: `done` is gone.
        let back = hub.subscribe("s", Some(1));
        assert!(back.gap);
        assert_eq!(data(&back), ["next-start"], "the turn in progress, whole");
        // Nothing missed after a turn that ended cleanly.
        let idle = TurnHub::default();
        idle.begin_turn("s");
        idle.publish("s", "session_turn", "done".into());
        idle.end_turn("s");
        assert!(!idle.subscribe("s", Some(1)).gap);
    }
}
