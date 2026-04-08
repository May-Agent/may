//! Session row model for the chat history database.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub id: String,
    pub title: Option<String>,
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
    pub created_at: String,
    pub updated_at: String,
}
