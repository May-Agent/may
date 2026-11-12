//! The cloud home: a machine whose daemon is the home environment of
//! sessions that a daemon elsewhere fronts.
//!
//! The fronting daemon bootstraps the machine and writes [`CloudHome`] into
//! its May home. The machine's daemon then binds its remote listener on the
//! port the fronting daemon forwards to, and admits on it the bearer token
//! whose digest the record holds (`daemon::auth::FrontingTokenIdentitySource`).
//! The record is bootstrap state, rewritten with every bootstrap, which
//! restarts the daemon; it is no user setting.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::Digest as _;

/// The record's file in the May home.
const FILE: &str = "cloud-home.json";

/// The port a cloud home's remote listener binds and the fronting daemon
/// forwards to. May chooses it: the default local listener is 8787, and
/// the remote listener takes the next port so the two never collide.
pub const REMOTE_PORT: u16 = 8788;

/// Bytes of a fronting token: 256 bits, so it cannot be guessed.
const TOKEN_BYTES: usize = 32;

/// What a cloud home's daemon serves its fronting daemon with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudHome {
    /// The port the remote listener binds on loopback.
    pub remote_port: u16,
    /// The account the fronting daemon acts for, for logs.
    pub owner: String,
    /// Hex SHA-256 of the fronting daemon's bearer token.
    pub token_sha256: String,
}

/// Why a cloud home record could not be read.
#[derive(Debug, thiserror::Error)]
pub enum CloudHomeError {
    /// The file exists but could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The record's path.
        path: PathBuf,
        /// The failure.
        #[source]
        source: std::io::Error,
    },
    /// The file is not a cloud home record.
    #[error("{} is not a cloud home record: {source}", path.display())]
    Parse {
        /// The record's path.
        path: PathBuf,
        /// The failure.
        #[source]
        source: serde_json::Error,
    },
    /// The token digest is not 32 hex-encoded bytes.
    #[error("the cloud home's token digest is not a hex SHA-256")]
    Digest,
}

impl CloudHome {
    /// The record for a fronting `token` acting for `owner`.
    #[must_use]
    pub fn for_token(token: &FrontingToken, owner: String) -> Self {
        Self {
            remote_port: REMOTE_PORT,
            owner,
            token_sha256: hex::encode(sha2::Sha256::digest(token.0.as_bytes())),
        }
    }

    /// The record's path in the May home `may_dir`.
    #[must_use]
    pub fn path(may_dir: &Path) -> PathBuf {
        may_dir.join(FILE)
    }

    /// The record in `may_dir`, or `None` on a machine that is no cloud
    /// home.
    ///
    /// # Errors
    /// Fails when the file exists but cannot be read or parsed.
    pub fn load(may_dir: &Path) -> Result<Option<Self>, CloudHomeError> {
        let path = Self::path(may_dir);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(CloudHomeError::Read { path, source }),
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|source| CloudHomeError::Parse { path, source })
    }

    /// The token digest as bytes.
    ///
    /// # Errors
    /// [`CloudHomeError::Digest`] when it is not a hex SHA-256.
    pub fn digest(&self) -> Result<[u8; 32], CloudHomeError> {
        hex::decode(&self.token_sha256)
            .ok()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
            .ok_or(CloudHomeError::Digest)
    }
}

/// The bearer token a fronting daemon authenticates to a cloud home with.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FrontingToken(String);

impl FrontingToken {
    /// A fresh random token.
    ///
    /// # Panics
    /// When the operating system's random source fails, which leaves no
    /// secure token to issue.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0_u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).expect("the operating system's random source works");
        Self(hex::encode(bytes))
    }

    /// The token as stored.
    #[must_use]
    pub const fn from_stored(token: String) -> Self {
        Self(token)
    }

    /// The token's text, for the `Authorization` header and storage.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for FrontingToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrontingToken(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_without_the_record_is_no_cloud_home() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(CloudHome::load(dir.path()).expect("loads"), None);
    }

    #[test]
    fn the_record_round_trips_and_keeps_only_the_tokens_digest() {
        let dir = tempfile::tempdir().expect("temp dir");
        let token = FrontingToken::generate();
        let home = CloudHome::for_token(&token, "octocat".into());
        std::fs::write(
            CloudHome::path(dir.path()),
            serde_json::to_string(&home).expect("serializes"),
        )
        .expect("written");

        let loaded = CloudHome::load(dir.path())
            .expect("loads")
            .expect("a cloud home");

        assert_eq!(loaded, home);
        assert_eq!(loaded.remote_port, REMOTE_PORT);
        assert!(!loaded.token_sha256.contains(token.expose()));
        assert_eq!(
            loaded.digest().expect("a digest"),
            <[u8; 32]>::from(sha2::Sha256::digest(token.expose().as_bytes()))
        );
    }

    #[test]
    fn generated_tokens_differ() {
        assert_ne!(FrontingToken::generate(), FrontingToken::generate());
    }
}
