use arc_swap::{ArcSwap, Guard};
use async_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Default)]
pub(crate) struct LiveResponseRegistry<Response> {
    senders: ArcSwap<HashMap<String, Sender<Response>>>,
}

impl<Response> LiveResponseRegistry<Response> {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            senders: ArcSwap::from_pointee(HashMap::new()),
        }
    }

    pub(crate) fn open(&self, request_id: String) -> Receiver<Response> {
        let (tx, rx) = async_channel::bounded(1);
        self.senders.rcu(|senders| {
            let mut next = HashMap::clone(senders);
            next.insert(request_id.clone(), tx.clone());
            next
        });
        rx
    }

    pub(crate) fn try_open(&self, request_id: String) -> Option<Receiver<Response>> {
        let (tx, rx) = async_channel::bounded(1);
        loop {
            let current = self.senders.load_full();
            if current.contains_key(request_id.as_str()) {
                return None;
            }

            let mut next = HashMap::clone(current.as_ref());
            next.insert(request_id.clone(), tx.clone());
            let previous =
                Guard::into_inner(self.senders.compare_and_swap(&current, Arc::new(next)));
            if Arc::ptr_eq(&current, &previous) {
                return Some(rx);
            }
        }
    }

    pub(crate) fn resolve(&self, request_id: &str, response: Response) -> bool {
        let tx = self.senders.load().get(request_id).cloned();
        if let Some(tx) = tx {
            self.senders.rcu(|senders| {
                let mut next = HashMap::clone(senders);
                next.remove(request_id);
                next
            });
            return tx.try_send(response).is_ok();
        }
        false
    }

    pub(crate) fn discard(&self, request_id: &str) -> bool {
        if !self.senders.load().contains_key(request_id) {
            return false;
        }
        self.senders.rcu(|senders| {
            let mut next = HashMap::clone(senders);
            next.remove(request_id);
            next
        });
        true
    }
}
