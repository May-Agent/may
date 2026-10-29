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
}
