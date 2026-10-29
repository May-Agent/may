use arc_swap::ArcSwap;
use async_channel::{Receiver, Sender};
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct LiveResponseRegistry<Response> {
    senders: ArcSwap<HashMap<String, Sender<Response>>>,
}

impl<Response> LiveResponseRegistry<Response> {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            senders: ArcSwap::from_pointee(HashMap::new()),
        }
    }

    pub(crate) fn open(&self, request_id: &str) -> Receiver<Response> {
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
    pub(crate) fn take(&self, request_id: &str) -> Option<Sender<Response>> {
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
    pub(crate) fn restore(&self, request_id: &str, sender: &Sender<Response>) {
        self.senders.rcu(|senders| {
            let mut next = HashMap::clone(senders);
            next.insert(request_id.to_string(), sender.clone());
            next
        });
    }

    pub(crate) fn resolve(&self, request_id: &str, response: Response) -> bool {
        self.take(request_id)
            .is_some_and(|tx| tx.try_send(response).is_ok())
    }
}
