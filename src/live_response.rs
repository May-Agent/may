use async_channel::{Receiver, Sender};
use dashmap::DashMap;

#[derive(Debug, Default)]
pub(crate) struct LiveResponseRegistry<Response> {
    senders: DashMap<String, Sender<Response>>,
}

impl<Response> LiveResponseRegistry<Response> {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            senders: DashMap::new(),
        }
    }

    pub(crate) fn open(&self, request_id: String) -> Receiver<Response> {
        let (tx, rx) = async_channel::bounded(1);
        self.senders.insert(request_id, tx);
        rx
    }

    pub(crate) fn resolve(&self, request_id: &str, response: Response) -> bool {
        self.senders
            .remove(request_id)
            .is_some_and(|(_, tx)| tx.try_send(response).is_ok())
    }

    pub(crate) fn discard(&self, request_id: &str) -> bool {
        self.senders.remove(request_id).is_some()
    }
}
