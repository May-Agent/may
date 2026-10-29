//! Durable session run lease row model for active agent execution.

/// Fenced lease recording which run, if any, executes a session.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Owning session id.
    pub session_id: String,
    /// Random token of the holding run, `None` while the lease is free.
    pub owner: Option<String>,
    /// Monotonic count of acquisitions; fences a stale holder after takeover.
    pub generation: i64,
    /// Holder's last renewal, in Unix milliseconds.
    pub heartbeat_at: i64,
    /// Whether cancellation was requested.
    pub cancellation_requested: bool,
}
