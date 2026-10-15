//! Web Push subscription row model for the chat history database.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    pub id: String,
    /// Push endpoint URL from the browser.
    pub endpoint: String,
    /// P-256 DH public key (base64url).
    pub p256dh: String,
    /// Auth secret (base64url).
    pub auth: String,
    /// Subscription creation timestamp (ISO 8601).
    pub created_at: String,
}
