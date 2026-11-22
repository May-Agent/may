use arc_swap::ArcSwap;
use async_channel::{Receiver, Sender};
use std::collections::HashMap;

/// The one-shot answer channels of requests that wait on the user, keyed by
/// request id.
#[derive(Debug, Default)]
pub struct LiveResponseRegistry<Response> {
    senders: ArcSwap<HashMap<String, Sender<Response>>>,
}

impl<Response> LiveResponseRegistry<Response> {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            senders: ArcSwap::from_pointee(HashMap::new()),
        }
    }

    /// Registers a waiter for `request_id` and returns the channel its
    /// response arrives on.
    pub fn open(&self, request_id: &str) -> Receiver<Response> {
        let (tx, rx) = async_channel::bounded(1);
        self.senders.rcu(|senders| {
            let mut next = HashMap::clone(senders);
            next.insert(request_id.to_string(), tx.clone());
            next
        });
        rx
    }

    /// Take the waiter of `request_id` out of the registry, so that no other
    /// path can resolve it. Returns `None` when nobody waits.
    pub fn take(&self, request_id: &str) -> Option<Sender<Response>> {
        let mut taken = None;
        self.senders.rcu(|senders| {
            let mut next = HashMap::clone(senders);
            taken = next.remove(request_id);
            next
        });
        taken
    }

    /// Put a waiter taken with [`Self::take`] back, because it was not
    /// resolved.
    pub fn restore(&self, request_id: &str, sender: &Sender<Response>) {
        self.senders.rcu(|senders| {
            let mut next = HashMap::clone(senders);
            next.insert(request_id.to_string(), sender.clone());
            next
        });
    }

    /// Answers `request_id` with `response`; `false` when nobody waits.
    pub fn resolve(&self, request_id: &str, response: Response) -> bool {
        self.take(request_id)
            .is_some_and(|tx| tx.try_send(response).is_ok())
    }

    /// The request ids that still wait, for a test.
    #[cfg(any(test, feature = "testing"))]
    pub fn pending_ids(&self) -> Vec<String> {
        self.senders.load().keys().cloned().collect()
    }
}
