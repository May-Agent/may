//! The session's persisted id and its list-row summary.

use jiff::Timestamp;

use crate::session_runtime::SessionRuntimeSummary;

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(transparent)]
/// A session's unique identifier.
pub struct SessionId(String);

/// Summary of a session for listing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct SessionSummary {
    /// Unique session identifier.
    pub id: SessionId,
    /// Session title, once assigned.
    pub title: Option<String>,
    /// Model bound to the session.
    pub model: Option<String>,
    /// Profile the session belongs to.
    pub profile_id: Option<String>,
    /// Whether the session is pinned in its profile.
    pub pinned: bool,
    /// Whether the session has been auto-archived.
    pub archived: bool,
    /// Folder the session is grouped in.
    pub folder_id: Option<String>,
    /// Display order within the pinned section.
    pub pin_sort_order: Option<i32>,
    /// Number of persisted messages.
    pub message_count: usize,
    /// Creation timestamp.
    #[schema(value_type = Option<String>)]
    pub created_at: Option<Timestamp>,
    /// Last update timestamp.
    #[schema(value_type = Option<String>)]
    pub updated_at: Option<Timestamp>,
    /// Content preview for searching (concatenated messages).
    pub content_preview: String,
    /// Optional per-session reasoning/thinking override ("low"|"medium"|"high").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Repository binding: `github:owner/repo`, a local repository key, or
    /// `None` for a repo-less chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Persisted runtime summary for refresh-safe UI state.
    #[serde(
        default,
        skip_serializing_if = "crate::session_runtime::SessionRuntimeSummary::is_empty"
    )]
    pub runtime: SessionRuntimeSummary,
    /// Whether something runs in the session: a turn, a background command
    /// or a subagent run. Set by the daemon when it lists sessions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub working: bool,
}

impl SessionId {
    /// Create from an existing string.
    pub fn from_string(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Get the ID as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
