//! Persisted notification mailbox row model.

/// A notification awaiting or having completed delivery.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Notification id.
    pub id: String,
    /// Notification title.
    pub title: String,
    /// Notification body text.
    pub body: String,
    /// Session the notification belongs to, if any.
    pub session_id: Option<String>,
    /// Serialized context fragment attached to the callback.
    pub callback_context_xml: Option<String>,
    /// Callback message delivered to the session on consumption.
    pub callback_message: Option<String>,
    /// Delivery status string.
    pub status: String,
    /// Delivery attempts so far.
    pub sent_count: i32,
    /// Most recent delivery error, if any.
    pub last_error: Option<String>,
    /// Delivery timestamp (ISO 8601), when delivered.
    pub delivered_at: Option<String>,
    /// Creation timestamp (ISO 8601).
    pub created_at: String,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
}
