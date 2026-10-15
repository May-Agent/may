//! Session row model for the chat history database.

/// Session row model.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Session id.
    pub id: String,
    /// Session title, once assigned.
    pub title: Option<String>,
    /// Model bound to the session.
    pub model: Option<String>,
    /// Last message preview (truncated).
    pub preview: Option<String>,
    /// Number of messages in this session.
    pub message_count: i32,
    /// Profile this session belongs to.
    pub profile_id: Option<String>,
    /// Whether this session is pinned in its profile.
    pub pinned: bool,
    /// Whether this session has been auto-archived.
    pub archived: bool,
    /// Folder this session belongs to (for pinned sessions).
    pub folder_id: Option<String>,
    /// Display order within the pinned section.
    pub pin_sort_order: Option<i32>,
    /// Creation timestamp (ISO 8601).
    pub created_at: String,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
    /// Per-session reasoning/thinking depth override ("low" | "medium" | "high").
    /// NULL means "no override; provider/model default applies".
    pub reasoning_effort: Option<String>,
}
