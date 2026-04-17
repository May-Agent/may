//! Notification mailbox queries on [`ChatDb`].
//!
//! All seven methods in this file operate on the single
//! `notification_mailbox` table. Split out of db/mod.rs so the
//! 5000-line monolith can be read by concern instead of scanned by
//! keyword.

use crate::xml::XmlBlock;

use super::{
    ChatDb, ChatDbError, NotificationCallbackConsumeResult, entity::notification_mailbox,
    overflow_error, row_not_found, to_i64_from_u64,
};

impl ChatDb {
    /// Insert a new mailbox notification entry.
    pub async fn create_notification_mailbox_entry(
        &self,
        id: &str,
        payload: &crate::notification::NotificationPayload,
        status: &str,
    ) -> Result<notification_mailbox::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        Ok(self
            .db
            .query(
                "INSERT INTO notification_mailbox (id, title, body, session_id, \
                 callback_context_xml, callback_message, status, sent_count, last_error, \
                 delivered_at, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
            )
            .bind(id)
            .bind(payload.title.as_str())
            .bind(payload.body.as_str())
            .bind(payload.session_id.as_deref())
            .bind(
                payload
                    .callback_context_xml
                    .clone()
                    .map(XmlBlock::into_inner),
            )
            .bind(payload.callback_message.as_deref())
            .bind(status)
            .bind(0_i32)
            .bind(Option::<String>::None)
            .bind(Option::<String>::None)
            .bind(now.as_str())
            .bind(now.as_str())
            .fetch_one::<notification_mailbox::Model>()
            .await?)
    }

    /// Update mailbox delivery state after a send attempt.
    pub async fn update_notification_mailbox_delivery(
        &self,
        id: &str,
        status: &str,
        sent_count: usize,
        last_error: Option<&str>,
        delivered_at: Option<&str>,
    ) -> Result<(), ChatDbError> {
        self.db
            .query(
                "UPDATE notification_mailbox SET status = ?, sent_count = ?, last_error = ?, \
                 delivered_at = ?, updated_at = ? WHERE id = ?",
            )
            .bind(status)
            .bind(
                i32::try_from(sent_count)
                    .map_err(|error| overflow_error("notification sent_count", &error))?,
            )
            .bind(last_error)
            .bind(delivered_at)
            .bind(jiff::Timestamp::now().to_string())
            .bind(id)
            .execute()
            .await?;
        Ok(())
    }

    /// Return notifications that still need delivery attempts.
    pub async fn list_pending_notification_mailbox_entries(
        &self,
    ) -> Result<Vec<notification_mailbox::Model>, ChatDbError> {
        Ok(self
            .db
            .query("SELECT * FROM notification_mailbox WHERE status = ? ORDER BY created_at ASC")
            .bind("pending")
            .fetch_all::<notification_mailbox::Model>()
            .await?)
    }

    /// Atomically claim a pending notification for delivery (CAS: pending → delivering).
    /// Returns true if the claim succeeded, false if another process already claimed it.
    pub async fn claim_pending_notification(&self, id: &str) -> Result<bool, ChatDbError> {
        let result = self
            .db
            .query(
                "UPDATE notification_mailbox SET status = ?, updated_at = ? WHERE id = ? AND status = ?",
            )
            .bind("delivering")
            .bind(jiff::Timestamp::now().to_string())
            .bind(id)
            .bind("pending")
            .execute()
            .await?;
        Ok(result.rows_written > 0)
    }

    /// Release a claimed notification back to pending (on send failure).
    pub async fn release_notification_claim(
        &self,
        id: &str,
        error: &str,
    ) -> Result<(), ChatDbError> {
        self.db
            .query(
                "UPDATE notification_mailbox SET status = ?, last_error = ?, updated_at = ? WHERE id = ?",
            )
            .bind("pending")
            .bind(error)
            .bind(jiff::Timestamp::now().to_string())
            .bind(id)
            .execute()
            .await?;
        Ok(())
    }

    pub async fn list_notification_mailbox_entries(
        &self,
        limit: u64,
    ) -> Result<Vec<notification_mailbox::Model>, ChatDbError> {
        Ok(self
            .db
            .query("SELECT * FROM notification_mailbox ORDER BY created_at DESC LIMIT ?")
            .bind(to_i64_from_u64(limit, "notification mailbox limit")?)
            .fetch_all::<notification_mailbox::Model>()
            .await?)
    }

    pub async fn get_notification_mailbox_entry(
        &self,
        id: &str,
    ) -> Result<Option<notification_mailbox::Model>, ChatDbError> {
        Ok(self
            .db
            .query("SELECT * FROM notification_mailbox WHERE id = ?")
            .bind(id)
            .fetch_optional::<notification_mailbox::Model>()
            .await?)
    }

    pub async fn mark_notification_mailbox_opened(&self, id: &str) -> Result<(), ChatDbError> {
        self.db
            .query("UPDATE notification_mailbox SET status = ?, updated_at = ? WHERE id = ?")
            .bind("opened")
            .bind(jiff::Timestamp::now().to_string())
            .bind(id)
            .execute()
            .await?;
        Ok(())
    }

    pub async fn consume_notification_callback(
        &self,
        id: &str,
    ) -> Result<NotificationCallbackConsumeResult, ChatDbError> {
        let entry = self
            .get_notification_mailbox_entry(id)
            .await?
            .ok_or_else(|| row_not_found(format!("Notification mailbox entry not found: {id}")))?;
        let has_callback = entry.callback_context_xml.is_some() || entry.callback_message.is_some();
        let callback_consumed = has_callback && entry.status != "opened";
        if entry.status != "opened" {
            self.mark_notification_mailbox_opened(id).await?;
        }
        let profile_id = if let Some(session_id) = entry.session_id.as_deref() {
            self.get_session(session_id)
                .await?
                .and_then(|session| session.profile_id)
        } else {
            None
        };
        Ok(NotificationCallbackConsumeResult {
            id: entry.id,
            session_id: entry.session_id,
            profile_id,
            callback_context_xml: entry.callback_context_xml,
            callback_message: entry.callback_message,
            has_callback,
            callback_consumed,
        })
    }
}
