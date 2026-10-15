//! `ChatGPT` subscription credential persistence.
//!
//! OAuth credentials for the Codex backend are stored in
//! `~/.may/chatgpt_token.json`. Access tokens are short-lived and refresh
//! tokens rotate on every refresh — the model client updates the stored
//! credentials through [`save_credentials`] whenever a rotation happens.

use std::path::PathBuf;

use aither_codex::auth::CodexCredentials;

use crate::auth_store::{delete_file, load_json, save_json};
use crate::config::MayConfig;

/// Token file name for `ChatGPT` OAuth credentials.
const TOKEN_FILE: &str = "chatgpt_token.json";

/// Error type for `ChatGPT` credential operations.
#[derive(Debug, thiserror::Error)]
pub enum ChatgptAuthError {
    /// Failed to resolve the May config directory.
    #[error("Failed to get May directory: {0}")]
    MayDir(#[from] crate::config::ConfigError),
    /// Credential file I/O or parse failure.
    #[error("Credential store error: {0}")]
    Store(#[from] crate::auth_store::AuthStoreError),
    /// OAuth flow or token refresh failed.
    #[error("Codex auth error: {0}")]
    Codex(#[from] aither_codex::CodexError),
}

/// Path to the stored credential file.
pub fn token_path() -> Result<PathBuf, ChatgptAuthError> {
    Ok(MayConfig::may_dir()?.join(TOKEN_FILE))
}

/// Load stored credentials; `Ok(None)` when the file does not exist.
pub async fn load_credentials() -> Result<Option<CodexCredentials>, ChatgptAuthError> {
    Ok(load_json(&token_path()?).await?)
}

/// Persist credentials (called on login and on every token rotation).
pub async fn save_credentials(credentials: &CodexCredentials) -> Result<(), ChatgptAuthError> {
    save_json(&token_path()?, credentials).await?;
    Ok(())
}

/// Delete stored credentials (logout or after an unrecoverable auth failure).
pub async fn delete_credentials() -> Result<(), ChatgptAuthError> {
    delete_file(&token_path()?).await?;
    Ok(())
}

/// Whether a credential file exists on disk.
#[must_use]
pub fn has_credentials() -> bool {
    token_path().is_ok_and(|p| p.exists())
}
