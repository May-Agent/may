use jiff::Timestamp;

use super::history::MessageRole;
use super::runtime::PersistedAttachment;

/// A persisted message row could not be read into its model form.
#[derive(Debug, thiserror::Error)]
pub enum PersistedMessageError {
    /// The row's role string names no message role.
    #[error("invalid persisted message role in session {session_id}, message {message_id}: {role}")]
    InvalidRole {
        /// The session the row belongs to.
        session_id: String,
        /// The row's message id.
        message_id: i32,
        /// The role string the row carries.
        role: String,
    },
    /// The row's timestamp does not parse.
    #[error(
        "invalid persisted message timestamp in session {session_id}, message {message_id}: {timestamp}: {reason}"
    )]
    InvalidTimestamp {
        /// The session the row belongs to.
        session_id: String,
        /// The row's message id.
        message_id: i32,
        /// The timestamp string the row carries.
        timestamp: String,
        /// Why it does not parse.
        reason: String,
    },
    /// The row's attachments do not deserialize.
    #[error(
        "invalid persisted message attachments in session {session_id}, message {message_id}: {source}"
    )]
    InvalidAttachments {
        /// The session the row belongs to.
        session_id: String,
        /// The row's message id.
        message_id: i32,
        /// The deserialization failure.
        #[source]
        source: serde_json::Error,
    },
    /// The row carries no timestamp.
    #[error("missing persisted message timestamp in session {session_id}, message {message_id}")]
    MissingTimestamp {
        /// The session the row belongs to.
        session_id: String,
        /// The row's message id.
        message_id: i32,
    },
}

/// A message row read into its model form.
#[derive(Debug, Clone)]
pub struct PersistedMessage<'a> {
    row: &'a may_db::entity::messages::Model,
    /// The message's role.
    pub role: MessageRole,
    /// When the message was written.
    pub timestamp: Option<Timestamp>,
    /// Attachment records stored with the message.
    pub attachments: Vec<PersistedAttachment>,
}

impl PersistedMessage<'_> {
    /// The message's timestamp, or the error a missing one names.
    ///
    /// # Errors
    /// [`PersistedMessageError::MissingTimestamp`] when the row carries none.
    #[allow(dead_code)] // only the test-only message mapper names the row in errors
    pub fn require_timestamp(&self) -> Result<&Timestamp, PersistedMessageError> {
        self.timestamp
            .as_ref()
            .ok_or_else(|| PersistedMessageError::MissingTimestamp {
                session_id: self.row.session_id.clone(),
                message_id: self.row.id,
            })
    }
}

impl<'a> TryFrom<&'a may_db::entity::messages::Model> for PersistedMessage<'a> {
    type Error = PersistedMessageError;

    fn try_from(row: &'a may_db::entity::messages::Model) -> Result<Self, Self::Error> {
        let role =
            row.role
                .parse::<MessageRole>()
                .map_err(|role| PersistedMessageError::InvalidRole {
                    session_id: row.session_id.clone(),
                    message_id: row.id,
                    role,
                })?;
        let timestamp = row
            .timestamp
            .as_deref()
            .map(|value| {
                value.parse::<Timestamp>().map_err(|error| {
                    PersistedMessageError::InvalidTimestamp {
                        session_id: row.session_id.clone(),
                        message_id: row.id,
                        timestamp: value.to_string(),
                        reason: error.to_string(),
                    }
                })
            })
            .transpose()?;
        let attachments = match row.attachments.as_deref() {
            Some(raw) => serde_json::from_str(raw).map_err(|source| {
                PersistedMessageError::InvalidAttachments {
                    session_id: row.session_id.clone(),
                    message_id: row.id,
                    source,
                }
            })?,
            None => Vec::new(),
        };

        Ok(Self {
            row,
            role,
            timestamp,
            attachments,
        })
    }
}
