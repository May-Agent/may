use jiff::Timestamp;

use super::history::MessageRole;
use super::runtime::PersistedAttachment;

#[derive(Debug, thiserror::Error)]
pub enum PersistedMessageError {
    #[error("invalid persisted message role in session {session_id}, message {message_id}: {role}")]
    InvalidRole {
        session_id: String,
        message_id: i32,
        role: String,
    },
    #[error(
        "invalid persisted message timestamp in session {session_id}, message {message_id}: {timestamp}: {reason}"
    )]
    InvalidTimestamp {
        session_id: String,
        message_id: i32,
        timestamp: String,
        reason: String,
    },
    #[error(
        "invalid persisted message attachments in session {session_id}, message {message_id}: {source}"
    )]
    InvalidAttachments {
        session_id: String,
        message_id: i32,
        #[source]
        source: serde_json::Error,
    },
    #[error("missing persisted message timestamp in session {session_id}, message {message_id}")]
    MissingTimestamp { session_id: String, message_id: i32 },
}

#[derive(Debug, Clone)]
pub struct PersistedMessage<'a> {
    row: &'a crate::db::entity::messages::Model,
    pub role: MessageRole,
    pub timestamp: Option<Timestamp>,
    /// Attachment records stored with the message.
    pub attachments: Vec<PersistedAttachment>,
}

impl PersistedMessage<'_> {
    pub fn require_timestamp(&self) -> Result<&Timestamp, PersistedMessageError> {
        self.timestamp
            .as_ref()
            .ok_or_else(|| PersistedMessageError::MissingTimestamp {
                session_id: self.row.session_id.clone(),
                message_id: self.row.id,
            })
    }
}

impl<'a> TryFrom<&'a crate::db::entity::messages::Model> for PersistedMessage<'a> {
    type Error = PersistedMessageError;

    fn try_from(row: &'a crate::db::entity::messages::Model) -> Result<Self, Self::Error> {
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
