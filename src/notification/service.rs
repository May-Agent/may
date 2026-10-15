//! Durable notification mailbox service.

use std::sync::Arc;

use crate::db::ChatDb;
use crate::notification::{NotificationManager, NotificationPayload};
use crate::ui_event_bus::{UiEventBus, UiEventTopics};
use crate::xml::XmlBlock;

const MAILBOX_STATUS_PENDING: &str = "pending";
const MAILBOX_STATUS_DELIVERED: &str = "delivered";
const MAILBOX_STATUS_FAILED: &str = "failed";

/// Delivers notifications over web push and tracks their delivery in the
/// notification mailbox.
#[derive(Debug, Clone)]
pub struct NotificationService {
    db: ChatDb,
    manager: Arc<NotificationManager>,
    ui_event_bus: UiEventBus,
}

impl NotificationService {
    /// Create a service backed by the chat database, a push [`NotificationManager`],
    /// and the UI event bus used to publish mailbox changes.
    #[must_use]
    pub const fn new(
        db: ChatDb,
        manager: Arc<NotificationManager>,
        ui_event_bus: UiEventBus,
    ) -> Self {
        Self {
            db,
            manager,
            ui_event_bus,
        }
    }

    /// Return the VAPID public key clients need to create push subscriptions.
    #[must_use]
    pub fn public_key(&self) -> &str {
        self.manager.public_key()
    }

    /// Store a new web-push subscription for `endpoint`.
    ///
    /// # Errors
    ///
    /// Returns [`ChatDbError`](crate::db::ChatDbError) when inserting the
    /// subscription fails.
    pub async fn subscribe(
        &self,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
    ) -> Result<(), crate::db::ChatDbError> {
        let id = uuid::Uuid::new_v4().to_string();
        self.db
            .add_push_subscription(&id, endpoint, p256dh, auth)
            .await?;
        Ok(())
    }

    /// Remove the web-push subscription for `endpoint`.
    ///
    /// # Errors
    ///
    /// Returns [`ChatDbError`](crate::db::ChatDbError) when the removal fails.
    pub async fn unsubscribe(&self, endpoint: &str) -> Result<(), crate::db::ChatDbError> {
        self.db.remove_push_subscription(endpoint).await
    }

    /// Send a simple title/body notification to every stored subscription.
    ///
    /// Returns the number of subscriptions the payload was delivered to.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationServiceError`] when the mailbox entry cannot be
    /// persisted or the push transport fails; see [`Self::send_payload`].
    pub async fn send(&self, title: &str, body: &str) -> Result<usize, NotificationServiceError> {
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

    /// Send `payload` to every stored subscription and record the outcome in the
    /// notification mailbox.
    ///
    /// Returns the number of subscriptions the payload was delivered to.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationServiceError::Db`] when recording the mailbox entry or
    /// its delivery state fails, and [`NotificationServiceError::Notification`] when
    /// the push transport rejects the payload.
    pub async fn send_payload(
        &self,
        mut payload: NotificationPayload,
    ) -> Result<usize, NotificationServiceError> {
        let notification_id = uuid::Uuid::new_v4().to_string();
        payload.notification_id = Some(notification_id.clone());
        self.db
            .create_notification_mailbox_entry(&notification_id, &payload, MAILBOX_STATUS_PENDING)
            .await?;

        let send_result = self.manager.send_payload(payload).await;
        match send_result {
            Ok(sent_count) => {
                let status = if sent_count > 0 {
                    MAILBOX_STATUS_DELIVERED
                } else {
                    MAILBOX_STATUS_FAILED
                };
                let delivered_at = if sent_count > 0 {
                    Some(jiff::Timestamp::now().to_string())
                } else {
                    None
                };
                let last_error = if sent_count > 0 {
                    None
                } else {
                    Some("no active push subscriptions matched this notification")
                };
                self.db
                    .update_notification_mailbox_delivery(
                        &notification_id,
                        status,
                        sent_count,
                        last_error,
                        delivered_at.as_deref(),
                    )
                    .await?;
                self.ui_event_bus
                    .publish(UiEventTopics::NOTIFICATION_MAILBOX);
                Ok(sent_count)
            }
            Err(error) => {
                let error_text = error.to_string();
                self.db
                    .update_notification_mailbox_delivery(
                        &notification_id,
                        MAILBOX_STATUS_FAILED,
                        0,
                        Some(error_text.as_str()),
                        None,
                    )
                    .await?;
                self.ui_event_bus
                    .publish(UiEventTopics::NOTIFICATION_MAILBOX);
                Err(NotificationServiceError::Notification(error))
            }
        }
    }

    /// Redeliver mailbox entries still marked pending, claiming each entry so
    /// concurrent processes do not double-send.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationServiceError::Db`] when listing, claiming, or updating
    /// mailbox entries fails, and [`NotificationServiceError::InvalidXml`] when a
    /// stored callback context block fails to parse.
    pub async fn redeliver_pending(&self) -> Result<(), NotificationServiceError> {
        let pending = self.db.list_pending_notification_mailbox_entries().await?;
        for item in pending {
            // CAS claim: pending → delivering. If another process already claimed it, skip.
            let claimed = self.db.claim_pending_notification(&item.id).await?;
            if !claimed {
                tracing::debug!(
                    notification_id = %item.id,
                    "skipping redelivery — already claimed by another process"
                );
                continue;
            }

            let payload = NotificationPayload {
                notification_id: Some(item.id.clone()),
                title: item.title,
                body: item.body,
                session_id: item.session_id,
                callback_context_xml: item.callback_context_xml.map(XmlBlock::parse).transpose()?,
                callback_message: item.callback_message,
            };
            let send_result = self.manager.send_payload(payload).await;
            match send_result {
                Ok(sent_count) => {
                    let status = if sent_count > 0 {
                        MAILBOX_STATUS_DELIVERED
                    } else {
                        MAILBOX_STATUS_FAILED
                    };
                    let delivered_at = if sent_count > 0 {
                        Some(jiff::Timestamp::now().to_string())
                    } else {
                        None
                    };
                    let last_error = if sent_count > 0 {
                        None
                    } else {
                        Some("no active push subscriptions matched this notification")
                    };
                    self.db
                        .update_notification_mailbox_delivery(
                            &item.id,
                            status,
                            sent_count,
                            last_error,
                            delivered_at.as_deref(),
                        )
                        .await?;
                    self.ui_event_bus
                        .publish(UiEventTopics::NOTIFICATION_MAILBOX);
                }
                Err(error) => {
                    // Release claim back to pending so it can be retried
                    let error_text = error.to_string();
                    self.db
                        .release_notification_claim(&item.id, &error_text)
                        .await?;
                    self.ui_event_bus
                        .publish(UiEventTopics::NOTIFICATION_MAILBOX);
                }
            }
        }
        Ok(())
    }
}

/// Error type for notification delivery.
#[derive(Debug, thiserror::Error)]
pub enum NotificationServiceError {
    /// Database error.
    #[error("database error: {0}")]
    Db(#[from] crate::db::ChatDbError),
    /// Invalid context xml block.
    #[error("invalid context xml block: {0}")]
    InvalidXml(#[from] crate::xml::XmlBlockError),
    /// Notification transport error.
    #[error("notification transport error: {0}")]
    Notification(#[from] crate::notification::NotificationError),
}
