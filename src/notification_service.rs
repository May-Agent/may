//! Durable notification mailbox service.

use std::sync::Arc;

use crate::db::ChatDb;
use crate::notification::{NotificationManager, NotificationPayload};
use crate::xml_fragment::XmlFragment;

const MAILBOX_STATUS_PENDING: &str = "pending";
const MAILBOX_STATUS_DELIVERED: &str = "delivered";
const MAILBOX_STATUS_FAILED: &str = "failed";

#[derive(Clone)]
pub struct NotificationService {
    db: ChatDb,
    manager: Arc<NotificationManager>,
}

impl NotificationService {
    #[must_use]
    pub const fn new(db: ChatDb, manager: Arc<NotificationManager>) -> Self {
        Self { db, manager }
    }

    #[must_use]
    pub fn public_key(&self) -> &str {
        self.manager.public_key()
    }

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

    pub async fn unsubscribe(&self, endpoint: &str) -> Result<(), crate::db::ChatDbError> {
        self.db.remove_push_subscription(endpoint).await
    }

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
                Err(NotificationServiceError::Notification(error))
            }
        }
    }

    pub async fn redeliver_pending(&self) -> Result<(), NotificationServiceError> {
        let pending = self.db.list_pending_notification_mailbox_entries().await?;
        for item in pending {
            let payload = NotificationPayload {
                notification_id: Some(item.id.clone()),
                title: item.title,
                body: item.body,
                session_id: item.session_id,
                callback_context_xml: item
                    .callback_context_xml
                    .map(XmlFragment::parse)
                    .transpose()?,
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
                }
                Err(error) => {
                    let error_text = error.to_string();
                    self.db
                        .update_notification_mailbox_delivery(
                            &item.id,
                            MAILBOX_STATUS_FAILED,
                            0,
                            Some(error_text.as_str()),
                            None,
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NotificationServiceError {
    #[error("database error: {0}")]
    Db(#[from] crate::db::ChatDbError),
    #[error("invalid XML fragment: {0}")]
    InvalidXml(#[from] crate::xml_fragment::XmlFragmentError),
    #[error("notification transport error: {0}")]
    Notification(#[from] crate::notification::NotificationError),
}
