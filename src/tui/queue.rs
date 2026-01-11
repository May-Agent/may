//! Message queue for handling user input while agent is processing.

use std::collections::VecDeque;
use std::path::PathBuf;

use jiff::Timestamp;

/// Threshold for auto-saving pasted text to file (chars).
pub const LARGE_PASTE_THRESHOLD: usize = 4000;

/// Maximum chars shown in LLM context for large pastes.
pub const PASTE_TRUNCATE_LIMIT: usize = 2000;

/// An attachment to a message.
#[derive(Debug, Clone)]
pub enum Attachment {
    /// Pasted image (saved to session/images/)
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
}

/// Message queue for concurrent user input.
#[derive(Debug, Default)]
pub struct MessageQueue {
    pending: VecDeque<PendingMessage>,
}

impl MessageQueue {
    /// Create a new message queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue a message.
    pub fn enqueue(&mut self, msg: PendingMessage) {
        self.pending.push_back(msg);
    }

    /// Dequeue the next message.
    pub fn dequeue(&mut self) -> Option<PendingMessage> {
        self.pending.pop_front()
    }

    /// Check if queue is empty.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Get number of pending messages.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Iterate over pending messages without dequeuing.
    pub fn iter(&self) -> impl Iterator<Item = &PendingMessage> {
        self.pending.iter()
    }
}
