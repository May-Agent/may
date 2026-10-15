//! Push notification subsystem.
//!
//! - [`NotificationManager`](self::NotificationManager) handles VAPID
//!   keypair generation/loading, push subscription management, and
//!   sending notifications to all subscribed browsers.
//! - [`service::NotificationService`] wraps the manager with a durable
//!   mailbox so pending callbacks survive may restarts.

pub mod service;

pub use service::{NotificationService, NotificationServiceError};

use std::path::Path;
use std::sync::Arc;

use async_fs;
use p256::ecdsa::SigningKey;
use p256::pkcs8::EncodePrivateKey;
use web_push::{
    ContentEncoding, IsahcWebPushClient, SubscriptionInfo, Urgency, VapidSignatureBuilder,
    WebPushClient, WebPushError, WebPushMessageBuilder,
};

use crate::db::ChatDb;
use crate::xml::XmlBlock;

/// Push notification payload delivered to browser service workers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_id: Option<String>,
    pub title: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_context_xml: Option<XmlBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_message: Option<String>,
}

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
    /// Reusable HTTP client for push delivery.
    client: IsahcWebPushClient,
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

        let client = IsahcWebPushClient::new().map_err(|e| {
            NotificationError::P256Key(format!("Failed to create push client: {e}"))
        })?;

        Ok(Self {
            vapid_pem,
            public_key_base64url,
            db,
            client,
        })
    }

    /// Generate a new VAPID EC P-256 keypair and write the private key as PEM.
    async fn generate_vapid_keypair(pem_path: &Path) -> Result<(), NotificationError> {
        let signing_key = SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
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
        let encoded_point = verifying_key.to_encoded_point(false);
        let bytes = encoded_point.as_bytes();
        Ok(base64_url_encode(bytes))
    }

    /// Get the VAPID public key for browser subscription (base64url-encoded).
    #[must_use]
    pub fn public_key(&self) -> &str {
        &self.public_key_base64url
    }

    /// Send a push notification to all subscribed browsers.
    pub async fn send(&self, title: &str, body: &str) -> Result<usize, NotificationError> {
        self.send_payload(NotificationPayload {
            notification_id: None,
            title: title.to_string(),
            body: body.to_string(),
            session_id: None,
            callback_context_xml: None,
            callback_message: None,
        })
        .await
    }

    /// Send a structured push notification payload to all subscribers.
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

            match self.client.send(message).await {
                Ok(()) => {
                    sent += 1;
                }
                Err(WebPushError::EndpointNotValid(_) | WebPushError::EndpointNotFound(_)) => {
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

/// Base64url encode without padding (RFC 4648 section 5).
fn base64_url_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}
