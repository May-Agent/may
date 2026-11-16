//! Durable notification mailbox service.

use std::sync::Arc;

use crate::db::ChatDb;
use crate::notification::{NotificationManager, NotificationPayload, UserNotification};
use crate::session::UserWait;
use crate::ui_event_bus::{UiEventBus, UiEventTopics};

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
    /// Notifications for attached terminals, which raise their own.
    user_notifications: tokio::sync::broadcast::Sender<UserNotification>,
}

/// Notifications buffered for a slow attached client.
const USER_NOTIFICATION_CAPACITY: usize = 64;

impl NotificationService {
    /// Create a service backed by the chat database, a push [`NotificationManager`],
    /// and the UI event bus used to publish mailbox changes.
    #[must_use]
    pub fn new(db: ChatDb, manager: Arc<NotificationManager>, ui_event_bus: UiEventBus) -> Self {
        Self {
            db,
            manager,
            ui_event_bus,
            user_notifications: tokio::sync::broadcast::channel(USER_NOTIFICATION_CAPACITY).0,
        }
    }

    /// The notifications for popups waiting on the user, from now on.
    #[must_use]
    pub fn subscribe_user_notifications(
        &self,
    ) -> tokio::sync::broadcast::Receiver<UserNotification> {
        self.user_notifications.subscribe()
    }

    /// Notify the user that `wait` needs them: streamed to attached
    /// terminals and pushed to subscribed browsers. A failure is logged and
    /// never reaches the popup.
    pub async fn notify_user_wait(&self, wait: &UserWait) {
        let (title, profile_id) = match self.db.get_session(&wait.session_id).await {
            Ok(Some(session)) => (session.title, session.profile_id),
            Ok(None) => (None, None),
            Err(error) => {
                tracing::warn!(session_id = %wait.session_id, %error, "failed to look up the session to notify about");
                (None, None)
            }
        };
        let notification = UserNotification::new(wait, title.as_deref(), profile_id);
        // No attached terminal is fine: browsers still get the push.
        let _ = self.user_notifications.send(notification.clone());
        let payload = NotificationPayload {
            notification_id: None,
            title: notification.title,
            body: notification.body,
            session_id: Some(notification.session_id),
            profile_id: notification.profile_id,
        };
        match self.send_payload(payload).await {
            Ok(0) => {
                tracing::debug!(session_id = %wait.session_id, "no browser subscribed to the notification");
            }
            Ok(sent) => tracing::debug!(session_id = %wait.session_id, sent, "notification pushed"),
            Err(error) => {
                tracing::warn!(session_id = %wait.session_id, %error, "failed to push the notification");
            }
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
        let id = crate::id::new();
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
            profile_id: None,
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
        let notification_id = crate::id::new();
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
    /// mailbox entries fails.
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
                profile_id: item.profile_id,
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
    /// Notification transport error.
    #[error("notification transport error: {0}")]
    Notification(#[from] crate::notification::NotificationError),
}
