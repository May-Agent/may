//! The secret types the wire shares.
//!
//! Who may read a stored secret is part of a persisted ask-user question
//! and of the store's own rows; the store itself, the names index and the
//! redaction live in `may-config`'s `secrets` module.

use serde::{Deserialize, Serialize};

/// Who can read a stored secret.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SecretScope {
    /// Every session's terminals, like a `/secret KEY VALUE` value.
    Global,
    /// Only the terminals of the session that asked for it; it is deleted
    /// with that session.
    Session,
}

impl SecretScope {
    /// The scope as the database stores it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Session => "session",
        }
    }

    /// The scope stored as `value`, or `None` when it names none.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "global" => Some(Self::Global),
            "session" => Some(Self::Session),
            _ => None,
        }
    }
}
