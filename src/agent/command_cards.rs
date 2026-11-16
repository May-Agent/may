//! Where the commands that render as cards report them.
//!
//! `subagent` and `ask_user` run inside the session's IPC command registry,
//! so the chat's event stream never sees what they did. Each reports the
//! card of the terminal call that ran it here, keyed by the call's id; the
//! daemon takes the updates off the channel, stores them and publishes them
//! to the session's clients.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::command_reading::Card;

/// A change to the card of one terminal call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardUpdate {
    /// The terminal call the card belongs to.
    pub call_id: String,
    /// The card as it stands now; `None` withdraws it, so clients show the
    /// ordinary command row.
    pub card: Option<Card>,
}

/// One session's channel of card updates; clones share it across agent
/// rebuilds.
#[derive(Debug, Clone)]
pub struct CommandCards {
    updates: async_channel::Sender<CardUpdate>,
    pending: async_channel::Receiver<CardUpdate>,
    claimed: Arc<AtomicBool>,
}

impl Default for CommandCards {
    fn default() -> Self {
        let (updates, pending) = async_channel::unbounded();
        Self {
            updates,
            pending,
            claimed: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl CommandCards {
    /// Reports the card of the terminal call `call_id`, or withdraws it.
    pub fn report(&self, call_id: impl Into<String>, card: Option<Card>) {
        // The channel is unbounded and this handle keeps it open.
        let _ = self.updates.try_send(CardUpdate {
            call_id: call_id.into(),
            card,
        });
    }

    /// The updates, for the one reader that publishes them: `Some` on the
    /// first call, `None` once a reader took them.
    #[must_use]
    pub fn claim_updates(&self) -> Option<async_channel::Receiver<CardUpdate>> {
        (!self.claimed.swap(true, Ordering::AcqRel)).then(|| self.pending.clone())
    }
}
