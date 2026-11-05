//! Fan-out of live turns to every attached client.
//!
//! A turn runs in the daemon whether or not anyone watches it. Its frames
//! go to every `/api/events` subscriber of the session: `session_turn`
//! frames carry the web UI's chat events, `session_agent` frames the agent
//! events the TUI renders. The frames of the turn in progress are kept, so a
//! client that attaches mid-turn first receives everything it missed.
//!
//! Every frame of a session carries an index, increasing for as long as the
//! daemon runs. Each subscription reads from its own cursor, and a frame is
//! kept until every subscription has read it, so a subscriber that falls
//! behind receives every frame however far behind it is, across the end of
//! its turn included. A client that reconnects names the last index it saw
//! and receives only what came after; when the frames it missed are gone
//! (their turn ended while it was away), the subscription says so and the
//! client reloads the session instead.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use event_listener::Event;

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
    /// Kept frames, oldest first, with consecutive indices: those a late
    /// subscriber replays (from `replay_from`) and the older ones a
    /// subscription has not read yet.
    frames: VecDeque<Arc<TurnFrame>>,
    /// Index of the first frame a late subscriber replays: the first frame
    /// of the turn in progress, or the first after the last turn ended.
    replay_from: u64,
    /// Index of the newest frame published; 0 before the first.
    last_index: u64,
    /// Index of the last frame each live subscription read, by its id.
    cursors: HashMap<u64, u64>,
    next_subscription: u64,
    /// Notified on every publish.
    published: Arc<Event>,
}

impl SessionChannel {
    fn new() -> Self {
        Self {
            frames: VecDeque::new(),
            replay_from: 1,
            last_index: 0,
            cursors: HashMap::new(),
            next_subscription: 0,
            published: Arc::new(Event::new()),
        }
    }

    /// Late subscribers replay from the next frame on.
    fn restart_replay(&mut self) {
        self.replay_from = self.last_index + 1;
        self.prune();
    }

    /// Drops the frames every subscription read that no late subscriber
    /// replays.
    fn prune(&mut self) {
        let read = self
            .cursors
            .values()
            .min()
            .copied()
            .unwrap_or(self.last_index);
        while self
            .frames
            .front()
            .is_some_and(|frame| frame.index < self.replay_from && frame.index <= read)
        {
            self.frames.pop_front();
        }
    }

    /// Index of the oldest kept frame, or of the next one when none is kept.
    fn first_kept(&self) -> u64 {
        self.frames
            .front()
            .map_or(self.last_index + 1, |frame| frame.index)
    }
}

/// Live turn frames of every session.
#[derive(Debug, Default)]
pub struct TurnHub {
    sessions: Mutex<HashMap<String, SessionChannel>>,
}

/// A subscription to one session's turns. Dropping it releases the frames
/// it has not read.
#[derive(Debug)]
pub struct TurnSubscription {
    hub: Arc<TurnHub>,
    session_id: String,
    id: u64,
    published: Arc<Event>,
    /// Whether frames after the index the subscriber named are gone: their
    /// turn ended, so the client must reload the session. The subscription
    /// then reads the turn in progress from its start.
    pub(crate) gap: bool,
}

impl TurnSubscription {
    /// The frames published since the last call, oldest first; waits until
    /// there is at least one. Cancelling the wait loses nothing.
    pub(crate) async fn next(&self) -> Vec<Arc<TurnFrame>> {
        loop {
            let published = self.published.listen();
            let frames = self.take_unread();
            if !frames.is_empty() {
                return frames;
            }
            published.await;
        }
    }

    /// The frames published since the last read, without waiting.
    pub(crate) fn take_unread(&self) -> Vec<Arc<TurnFrame>> {
        let id = self.id;
        self.hub.with_session(&self.session_id, |channel| {
            let cursor = *channel
                .cursors
                .get(&id)
                .expect("a live subscription has a cursor");
            let first = channel.first_kept();
            assert!(
                cursor + 1 >= first,
                "frame {} was dropped before subscription {id} read it",
                cursor + 1
            );
            let skip = usize::try_from(cursor + 1 - first).expect("frame offset fits usize");
            let frames: Vec<_> = channel.frames.iter().skip(skip).cloned().collect();
            channel.cursors.insert(id, channel.last_index);
            channel.prune();
            frames
        })
    }
}

impl Drop for TurnSubscription {
    fn drop(&mut self) {
        let id = self.id;
        self.hub.with_session(&self.session_id, |channel| {
            channel.cursors.remove(&id);
            channel.prune();
        });
    }
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

    /// Starts a new turn of `session_id`: late subscribers replay from its
    /// first frame.
    pub(crate) fn begin_turn(&self, session_id: &str) {
        self.with_session(session_id, SessionChannel::restart_replay);
    }

    /// Publishes a `kind` frame with `data` to the session's subscribers and
    /// keeps it for late ones until the turn ends.
    pub(crate) fn publish(&self, session_id: &str, kind: &'static str, data: Arc<str>) {
        let published = self.with_session(session_id, |channel| {
            channel.last_index += 1;
            channel.frames.push_back(Arc::new(TurnFrame {
                index: channel.last_index,
                kind,
                data,
            }));
            channel.published.clone()
        });
        published.notify(usize::MAX);
    }

    /// Ends the session's turn; later subscribers read it from history.
    /// Subscriptions that have not read all of it still receive the rest.
    pub(crate) fn end_turn(&self, session_id: &str) {
        self.with_session(session_id, SessionChannel::restart_replay);
    }

    /// Subscribes to `session_id`. `after` is the index of the last frame
    /// the subscriber saw; without it the turn in progress replays from its
    /// start.
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        session_id: &str,
        after: Option<u64>,
    ) -> TurnSubscription {
        let (id, published, gap) = self.with_session(session_id, |channel| {
            let replay_cursor = channel.replay_from - 1;
            // An index this daemon never published names another daemon's
            // frames: none of what came after it is here.
            let (cursor, gap) = match after {
                None => (replay_cursor, false),
                Some(after) if after <= channel.last_index && after + 1 >= channel.first_kept() => {
                    (after, false)
                }
                Some(_) => (replay_cursor, true),
            };
            let id = channel.next_subscription;
            channel.next_subscription += 1;
            channel.cursors.insert(id, cursor);
            (id, channel.published.clone(), gap)
        });
        TurnSubscription {
            hub: self.clone(),
            session_id: session_id.to_string(),
            id,
            published,
            gap,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{TurnHub, TurnSubscription};

    fn read(subscription: &TurnSubscription) -> Vec<String> {
        subscription
            .take_unread()
            .iter()
            .map(|frame| frame.data.to_string())
            .collect()
    }

    #[test]
    fn a_late_subscriber_replays_the_turn_and_then_follows_it() {
        let hub = Arc::new(TurnHub::default());
        hub.begin_turn("s");
        hub.publish("s", "session_turn", "start".into());
        hub.publish("s", "session_turn", "text-1".into());
        let late = hub.subscribe("s", None);
        assert_eq!(read(&late), ["start", "text-1"]);
        hub.publish("s", "session_turn", "text-2".into());
        let next = futures_lite::future::block_on(late.next());
        assert_eq!(&*next[0].data, "text-2");
        hub.end_turn("s");
        assert_eq!(read(&hub.subscribe("s", None)), [""; 0]);
    }

    #[test]
    fn a_returning_subscriber_gets_only_what_it_missed() {
        let hub = Arc::new(TurnHub::default());
        hub.begin_turn("s");
        hub.publish("s", "session_turn", "start".into());
        hub.publish("s", "session_turn", "text-1".into());
        hub.publish("s", "session_turn", "text-2".into());
        let back = hub.subscribe("s", Some(2));
        assert!(!back.gap);
        assert_eq!(read(&back), ["text-2"]);
        let current = hub.subscribe("s", Some(3));
        assert!(!current.gap);
        assert_eq!(read(&current), [""; 0]);
    }

    #[test]
    fn frames_of_an_ended_turn_nobody_holds_are_a_gap() {
        let hub = Arc::new(TurnHub::default());
        hub.begin_turn("s");
        hub.publish("s", "session_turn", "start".into());
        hub.publish("s", "session_turn", "done".into());
        hub.end_turn("s");
        hub.begin_turn("s");
        hub.publish("s", "session_turn", "next-start".into());
        // The subscriber saw only the first frame: `done` is gone.
        let back = hub.subscribe("s", Some(1));
        assert!(back.gap);
        assert_eq!(read(&back), ["next-start"], "the turn in progress, whole");
        // Nothing missed after a turn that ended cleanly.
        let idle = Arc::new(TurnHub::default());
        idle.begin_turn("s");
        idle.publish("s", "session_turn", "done".into());
        idle.end_turn("s");
        assert!(!idle.subscribe("s", Some(1)).gap);
        // An index this daemon never published belongs to another daemon.
        assert!(idle.subscribe("s", Some(7)).gap);
    }

    /// A subscriber that falls far behind while following a turn, past the
    /// turn's end and into the next one, still receives every frame in
    /// order, with no gap.
    #[test]
    fn a_subscriber_that_falls_behind_across_a_turn_end_misses_nothing() {
        let hub = Arc::new(TurnHub::default());
        let follower = hub.subscribe("s", None);
        let mut expected = Vec::new();
        for turn in 0..2 {
            hub.begin_turn("s");
            for delta in 0..5_000 {
                let data = format!("turn-{turn}-delta-{delta}");
                hub.publish("s", "session_agent", data.as_str().into());
                expected.push(data);
            }
            if turn == 0 {
                hub.end_turn("s");
            }
        }
        assert_eq!(read(&follower), expected);
        // Read frames of the ended turn are released; the turn in progress
        // stays for late subscribers.
        let late = hub.subscribe("s", None);
        assert_eq!(read(&late).len(), 5_000);
    }

    /// A subscription that goes away releases what it held.
    #[test]
    fn a_dropped_subscription_releases_its_frames() {
        let hub = Arc::new(TurnHub::default());
        let follower = hub.subscribe("s", None);
        hub.begin_turn("s");
        hub.publish("s", "session_turn", "start".into());
        hub.end_turn("s");
        assert!(!hub.subscribe("s", Some(0)).gap, "held for the follower");
        drop(follower);
        assert!(hub.subscribe("s", Some(0)).gap, "released with it");
    }
}
