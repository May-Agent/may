//! The per-session change notices the events stream diffs by.

/// One piece of a session's state the stream diffs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionChange {
    /// The persisted runtime summary.
    Runtime,
    /// Background jobs and subagent runs.
    Background,
    /// The `artifacts/` directory listing.
    Artifacts,
    /// Issues the session's tasks link.
    Issues,
    /// New `ci_findings` rows.
    Warning,
    /// The session's PRs, their states and live checks.
    Prs,
}

/// A `publish_session_change` notice on the session-change channel.
#[derive(Debug)]
pub struct SessionChangeNotice {
    /// The session whose state changed.
    pub session_id: String,
    /// Which piece of its state changed.
    pub kind: SessionChange,
}

/// The channel session-change notices are published on.
pub type SessionChangeSender = async_channel::Sender<SessionChangeNotice>;

/// Queue `kind` for the session's diff publisher; a closed dispatcher
/// drops it the way a dead bus drops a notification.
pub fn notify_change(tx: &SessionChangeSender, session_id: &str, kind: SessionChange) {
    let _ = tx.try_send(SessionChangeNotice {
        session_id: session_id.to_string(),
        kind,
    });
}

/// The cursor of `GET /api/sessions/{id}` and every SSE `id` on this
/// stream: `{epoch}.{index}`.
#[must_use]
pub fn format_cursor(epoch: u64, index: u64) -> String {
    format!("{epoch}.{index}")
}

impl SessionChange {
    /// Every kind, in bit order.
    pub const ALL: [Self; 6] = [
        Self::Runtime,
        Self::Background,
        Self::Artifacts,
        Self::Issues,
        Self::Warning,
        Self::Prs,
    ];

    /// This kind's bit in a change set.
    #[must_use]
    pub const fn bit(self) -> u8 {
        1 << self as u8
    }
}
