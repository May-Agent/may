//! Message queue for handling user input while agent is processing.

use std::collections::VecDeque;
use std::path::PathBuf;

use jiff::Timestamp;

use crate::session::PendingUserMessageMode;

/// Threshold for auto-saving pasted text to file (chars).
pub const LARGE_PASTE_THRESHOLD: usize = 4000;

/// Maximum chars shown in LLM context for large pastes.
pub const PASTE_TRUNCATE_LIMIT: usize = 2000;

/// An attachment to a message.
#[derive(Debug, Clone)]
pub enum Attachment {
    /// Pasted image (saved to session `files/images/`)
    Image { path: PathBuf, data: Vec<u8> },
    /// File reference (@file mention)
    FileReference { path: PathBuf },
    /// Pasted text (may be saved to file if large)
    PastedText {
        /// Full text content
        content: String,
        /// Path to saved file (for large pastes)
        saved_path: Option<PathBuf>,
    },
}

/// A pending message in the queue.
#[derive(Debug, Clone)]
pub struct PendingMessage {
    /// Message content
    pub content: String,
    /// Attachments
    pub attachments: Vec<Attachment>,
    /// When the message was queued
    pub queued_at: Timestamp,
    /// Whether this should steer the running agent or wait in the queue.
    pub delivery_mode: PendingUserMessageMode,
}

/// Message queue for concurrent user input.
#[derive(Debug, Default)]
pub struct MessageQueue {
    pending_immediate: Option<PendingMessage>,
    queued: VecDeque<PendingMessage>,
}

impl MessageQueue {
    /// Create a new message queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the full queue state from a persisted runtime snapshot.
    pub fn replace(
        &mut self,
        pending_immediate: Option<PendingMessage>,
        queued: Vec<PendingMessage>,
    ) {
        self.pending_immediate = pending_immediate;
        self.queued = queued.into_iter().collect();
    }

    /// Enqueue a message using its delivery mode.
    pub fn enqueue(&mut self, msg: PendingMessage) {
        match msg.delivery_mode {
            PendingUserMessageMode::Steer => {
                if let Some(previous) = self.pending_immediate.replace(msg) {
                    self.queued.push_front(previous);
                }
            }
            PendingUserMessageMode::Queue => {
                self.queued.push_back(msg);
            }
        }
    }

    /// Dequeue the next message.
    pub fn dequeue(&mut self) -> Option<PendingMessage> {
        if let Some(message) = self.pending_immediate.take() {
            return Some(message);
        }
        self.queued.pop_front()
    }

    /// Check if queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending_immediate.is_none() && self.queued.is_empty()
    }

    /// Get number of pending messages.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.pending_immediate.is_some()) + self.queued.len()
    }

    /// Iterate over pending messages without dequeuing.
    pub fn iter(&self) -> impl Iterator<Item = &PendingMessage> {
        self.pending_immediate.iter().chain(self.queued.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::{MessageQueue, PendingMessage};
    use crate::session::PendingUserMessageMode;
    use jiff::Timestamp;

    fn message(content: &str, mode: PendingUserMessageMode) -> PendingMessage {
        PendingMessage {
            content: content.to_string(),
            attachments: Vec::new(),
            queued_at: Timestamp::now(),
            delivery_mode: mode,
        }
    }

    #[test]
    fn steer_message_preempts_queue() {
        let mut queue = MessageQueue::new();
        queue.enqueue(message("queued", PendingUserMessageMode::Queue));
        queue.enqueue(message("steer", PendingUserMessageMode::Steer));

        let first = queue.dequeue().expect("first message");
        let second = queue.dequeue().expect("second message");

        assert_eq!(first.content, "steer");
        assert_eq!(second.content, "queued");
    }

    #[test]
    fn newer_steer_overflows_previous_one() {
        let mut queue = MessageQueue::new();
        queue.enqueue(message("first", PendingUserMessageMode::Steer));
        queue.enqueue(message("second", PendingUserMessageMode::Steer));

        let first = queue.dequeue().expect("first steer");
        let second = queue.dequeue().expect("overflow steer");

        assert_eq!(first.content, "second");
        assert_eq!(second.content, "first");
    }
}
