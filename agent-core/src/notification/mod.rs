//! Push notification subsystem.
//!
//! - [`NotificationManager`](self::NotificationManager) handles VAPID
//!   keypair generation/loading, push subscription management, and
//!   sending notifications to all subscribed browsers.
//! - [`service::NotificationService`] wraps the manager with a durable
//!   mailbox so pending notifications survive may restarts.

pub use may_protocol::notification::*;

pub mod service;
mod user_wait;

pub use may_protocol::notification::UserNotification;
pub use service::{NotificationService, NotificationServiceError};

use std::path::Path;
use std::sync::Arc;

use async_fs;
use p256::ecdsa::SigningKey;
use p256::pkcs8::EncodePrivateKey;
use web_push::{
    ContentEncoding, SubscriptionInfo, Urgency, VapidSignatureBuilder, WebPushError,
    WebPushMessage, WebPushMessageBuilder,
};
use zenwave::Client as _;

use crate::db::ChatDb;

/// Error type for notification operations.
#[derive(Debug, thiserror::Error)]
pub enum NotificationError {
    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Web push error.
    #[error("Web push error: {0}")]
    WebPush(#[from] WebPushError),
    /// Database error.
    #[error("Database error: {0}")]
    Db(#[from] crate::db::ChatDbError),
    /// PKCS8 encoding error.
    #[error("PKCS8 encoding error")]
    Pkcs8,
    /// P256 key error.
    #[error("P256 key error: {0}")]
    P256Key(String),
}

/// Web Push notification manager.
pub struct NotificationManager {
    /// Cached VAPID private key PEM bytes.
    vapid_pem: Arc<[u8]>,
    /// VAPID public key as uncompressed point bytes (65 bytes), base64url-encoded.
    public_key_base64url: String,
    /// Database for subscription storage.
    db: ChatDb,
}

impl std::fmt::Debug for NotificationManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotificationManager")
            .field("db", &self.db)
            .finish_non_exhaustive()
    }
}

impl NotificationManager {
    /// Create a new notification manager.
    ///
    /// Loads or generates a VAPID keypair at `may_dir/vapid_private.pem`.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when reading or generating the VAPID keypair
    /// fails.
    pub async fn new(may_dir: &Path, db: ChatDb) -> Result<Self, NotificationError> {
        let pem_path = may_dir.join("vapid_private.pem");

        // Generate keypair if it doesn't exist
        if async_fs::metadata(&pem_path).await.is_err() {
            Self::generate_vapid_keypair(&pem_path).await?;
            tracing::info!("Generated new VAPID keypair at {}", pem_path.display());
        }

        // Read PEM and derive public key
        let pem_contents = async_fs::read_to_string(&pem_path).await?;
        let public_key_base64url = Self::derive_public_key_from_pem(&pem_contents)?;
        let vapid_pem: Arc<[u8]> = pem_contents.into_bytes().into();

        Ok(Self {
            vapid_pem,
            public_key_base64url,
            db,
        })
    }

    /// Generate a new VAPID EC P-256 keypair and write the private key as PEM.
    async fn generate_vapid_keypair(pem_path: &Path) -> Result<(), NotificationError> {
        use p256::elliptic_curve::Generate as _;

        let signing_key = SigningKey::generate();
        let pem = signing_key
            .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
            .map_err(|_| NotificationError::Pkcs8)?;
        async_fs::write(pem_path, pem.as_bytes()).await?;
        Ok(())
    }

    /// Derive the uncompressed public key from PEM and encode as base64url.
    fn derive_public_key_from_pem(pem: &str) -> Result<String, NotificationError> {
        use p256::pkcs8::DecodePrivateKey;

        let signing_key = SigningKey::from_pkcs8_pem(pem)
            .map_err(|e| NotificationError::P256Key(format!("Invalid PEM: {e}")))?;
        let verifying_key = signing_key.verifying_key();
        let encoded_point = verifying_key.to_sec1_point(false);
        let bytes = encoded_point.as_bytes();
        Ok(base64_url_encode(bytes))
    }

    /// Get the VAPID public key for browser subscription (base64url-encoded).
    #[must_use]
    pub fn public_key(&self) -> &str {
        &self.public_key_base64url
    }

    /// Send a push notification to all subscribed browsers.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when listing subscriptions or delivering
    /// the payload fails; see [`Self::send_payload`].
    pub async fn send(&self, title: &str, body: &str) -> Result<usize, NotificationError> {
        self.send_payload(NotificationPayload {
            notification_id: None,
            title: title.to_string(),
            body: body.to_string(),
            session_id: None,
            profile_id: None,
        })
        .await
    }

    /// Send a structured push notification payload to all subscribers.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when the subscription list cannot be loaded
    /// or the push transport rejects the payload.
    pub async fn send_payload(
        &self,
        payload: NotificationPayload,
    ) -> Result<usize, NotificationError> {
        let subscriptions = self.db.list_push_subscriptions().await?;
        if subscriptions.is_empty() {
            tracing::debug!("No push subscriptions to notify");
            return Ok(0);
        }

        let payload_str = serde_json::to_string(&payload)
            .map_err(|e| NotificationError::P256Key(format!("Invalid payload JSON: {e}")))?;

        let mut sent = 0usize;
        let mut stale_endpoints = Vec::new();

        for sub in &subscriptions {
            let sub_info = SubscriptionInfo::new(&sub.endpoint, &sub.p256dh, &sub.auth);

            let sig_builder = match VapidSignatureBuilder::from_pem(
                self.vapid_pem.as_ref(),
                &sub_info,
            ) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(endpoint = %sub.endpoint, error = %e, "VAPID signature build failed");
                    continue;
                }
            };
            let signature = match sig_builder.build() {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(endpoint = %sub.endpoint, error = %e, "VAPID signature failed");
                    continue;
                }
            };

            let mut msg_builder = WebPushMessageBuilder::new(&sub_info);
            msg_builder.set_payload(ContentEncoding::Aes128Gcm, payload_str.as_bytes());
            msg_builder.set_ttl(86400);
            msg_builder.set_urgency(Urgency::Normal);
            msg_builder.set_vapid_signature(signature);

            let message = match msg_builder.build() {
                Ok(m) => m,
                Err(e) => {
                    tracing::error!(endpoint = %sub.endpoint, error = %e, "Push message build failed");
                    continue;
                }
            };

            match send_web_push_message(message).await {
                Ok(PushDelivery::Sent) => {
                    sent += 1;
                }
                Ok(PushDelivery::StaleEndpoint) => {
                    tracing::info!(endpoint = %sub.endpoint, "Removing stale push subscription");
                    stale_endpoints.push(sub.endpoint.clone());
                }
                Err(e) => {
                    tracing::warn!(endpoint = %sub.endpoint, error = %e, "Push delivery failed");
                }
            }
        }

        // Clean up stale subscriptions
        for endpoint in stale_endpoints {
            if let Err(e) = self.db.remove_push_subscription(&endpoint).await {
                tracing::error!(endpoint = %endpoint, error = %e, "Failed to remove stale subscription");
            }
        }

        Ok(sent)
    }
}

/// Result of one Web Push HTTP delivery attempt.
enum PushDelivery {
    /// Push service accepted the notification (`2xx`).
    Sent,
    /// Push service reported the subscription gone or unknown (`404`/`410`).
    StaleEndpoint,
}

/// Deliver a built Web Push message over `zenwave` using RFC 8030/8291 headers.
async fn send_web_push_message(message: WebPushMessage) -> Result<PushDelivery, WebPushError> {
    let request = web_push::request_builder::build_request::<Vec<u8>>(message);
    let url = request.uri().to_string();
    let mut client = zenwave::client();
    let mut builder = client.post(url).map_err(|_| WebPushError::InvalidUri)?;
    for (name, value) in request.headers() {
        let value = value
            .to_str()
            .map_err(|_| WebPushError::InvalidCryptoKeys)?;
        builder = builder
            .header(name.as_str(), value)
            .map_err(|_| WebPushError::InvalidCryptoKeys)?;
    }
    let body = request.into_body();
    let response = builder
        .bytes_body(body)
        .await
        .map_err(|error| WebPushError::Io(std::io::Error::other(error.to_string())))?;
    let status = response.status();
    if status.is_success() {
        return Ok(PushDelivery::Sent);
    }
    if matches!(status.as_u16(), 404 | 410) {
        return Ok(PushDelivery::StaleEndpoint);
    }
    if status.as_u16() == 413 {
        return Err(WebPushError::PayloadTooLarge);
    }
    let resp_bytes = response
        .into_body()
        .into_bytes()
        .await
        .map_err(|error| WebPushError::Io(std::io::Error::other(error.to_string())))?;
    let body_text = String::from_utf8_lossy(&resp_bytes);
    Err(WebPushError::Io(std::io::Error::other(format!(
        "push endpoint returned HTTP {}: {}",
        status.as_u16(),
        body_text
    ))))
}

/// Base64url encode without padding (RFC 4648 section 5).
fn base64_url_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}
