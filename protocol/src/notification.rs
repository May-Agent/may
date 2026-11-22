//! The web-push notification wire types.

/// Push notification payload delivered to browser service workers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationPayload {
    /// ID assigned when the notification was created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_id: Option<String>,
    /// Notification title.
    pub title: String,
    /// Notification body text.
    pub body: String,
    /// Session the notification relates to, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Profile of that session; opening the notification goes to the session
    /// under it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

/// A notification that a session waits on the user: what push delivers to
/// browsers and what the daemon streams to attached terminals.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserNotification {
    /// Session that waits.
    pub session_id: String,
    /// Profile of that session, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// The session's name.
    pub title: String,
    /// What the user is asked.
    pub body: String,
}
