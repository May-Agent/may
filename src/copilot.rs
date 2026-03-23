//! GitHub Copilot token persistence.
//!
//! Manages OAuth tokens for GitHub Copilot authentication.
//! OAuth tokens are stored in `~/.may/copilot_token.json`.
//!
//! The OAuth token must be exchanged for a session token before making API calls.
//! Use [`get_session_token`] to get a session token for API calls.

use crate::config::MayConfig;
use aither_copilot::auth::{CopilotToken, SessionToken};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};

/// Token file name for OAuth token (long-lived).
const TOKEN_FILE: &str = "copilot_token.json";

/// Token file name for session token (short-lived, cached).
const SESSION_FILE: &str = "copilot_session.json";

/// Error type for Copilot token operations.
#[derive(Debug, thiserror::Error)]
pub enum CopilotTokenError {
    #[error("Failed to get May directory: {0}")]
    MayDir(#[from] crate::config::ConfigError),
    #[error("Failed to read token file: {0}")]
    Read(std::io::Error),
    #[error("Failed to write token file: {0}")]
    Write(std::io::Error),
    #[error("Failed to parse token file: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("Token exchange failed: {0}")]
    Exchange(#[from] aither_copilot::CopilotError),
}

// ─── Generic JSON file I/O helpers ──────────────────────────────────────────

/// Load a JSON file, returning `Ok(None)` if not found.
async fn load_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, CopilotTokenError> {
    match async_fs::read_to_string(path).await {
        Ok(content) => Ok(Some(serde_json::from_str(&content)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CopilotTokenError::Read(e)),
    }
}

/// Save a value as a JSON file, creating parent directories if needed.
async fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<(), CopilotTokenError> {
    if let Some(parent) = path.parent() {
        async_fs::create_dir_all(parent)
            .await
            .map_err(CopilotTokenError::Write)?;
    }
    let content = serde_json::to_string_pretty(value)?;
    async_fs::write(path, content)
        .await
        .map_err(CopilotTokenError::Write)
}

/// Delete a file if it exists.
async fn delete_file(path: &Path) -> Result<(), CopilotTokenError> {
    match async_fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CopilotTokenError::Write(e)),
    }
}

// ─── Path helpers ───────────────────────────────────────────────────────────

/// Get the path to the Copilot token file.
fn token_path() -> Result<PathBuf, CopilotTokenError> {
    Ok(MayConfig::may_dir()?.join(TOKEN_FILE))
}

/// Get the path to the session token file.
fn session_path() -> Result<PathBuf, CopilotTokenError> {
    Ok(MayConfig::may_dir()?.join(SESSION_FILE))
}

// ─── Public API ─────────────────────────────────────────────────────────────

/// Load the Copilot token from disk.
///
/// Returns `Ok(None)` if the token file doesn't exist.
pub async fn load_copilot_token() -> Result<Option<CopilotToken>, CopilotTokenError> {
    load_json(&token_path()?).await
}

/// Save the Copilot token to disk.
pub async fn save_copilot_token(token: &CopilotToken) -> Result<(), CopilotTokenError> {
    save_json(&token_path()?, token).await
}

/// Delete the Copilot token from disk.
pub async fn delete_copilot_token() -> Result<(), CopilotTokenError> {
    delete_file(&token_path()?).await
}

/// Delete cached Copilot session token from disk.
pub async fn delete_session_token() -> Result<(), CopilotTokenError> {
    delete_file(&session_path()?).await
}

/// Delete both OAuth and cached session tokens from disk.
pub async fn clear_copilot_credentials() -> Result<(), CopilotTokenError> {
    delete_session_token().await?;
    delete_copilot_token().await?;
    Ok(())
}

/// Check if a Copilot token exists on disk.
#[must_use]
pub fn has_copilot_token() -> bool {
    token_path().map(|p| p.exists()).unwrap_or(false)
}

/// Load cached session token from disk.
async fn load_session_token() -> Result<Option<SessionToken>, CopilotTokenError> {
    load_json(&session_path()?).await
}

/// Save session token to disk.
async fn save_session_token(token: &SessionToken) -> Result<(), CopilotTokenError> {
    save_json(&session_path()?, token).await
}

/// Check if a session token is still valid (not expired).
fn is_session_valid(token: &SessionToken) -> bool {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Add 60 second buffer to avoid edge cases
    token.expires_at > now + 60
}

/// Credentials for Copilot API calls.
#[derive(Debug, Clone)]
pub struct CopilotCredentials {
    /// The session token for API calls.
    pub token: String,
    /// The API endpoint to use.
    pub api_endpoint: String,
    /// OAuth access token (for refreshing session tokens).
    pub oauth_token: String,
}

/// Get valid credentials for Copilot API calls.
///
/// This function:
/// 1. Tries to load a cached session token
/// 2. If valid, returns it
/// 3. If expired or missing, exchanges OAuth token for a new session token
/// 4. Caches the new session token
///
/// # Returns
///
/// - `Ok(Some(credentials))` - valid credentials with token and endpoint
/// - `Ok(None)` - no OAuth token configured
/// - `Err` - error during token exchange
pub async fn get_copilot_credentials() -> Result<Option<CopilotCredentials>, CopilotTokenError> {
    // First, check if we have an OAuth token
    let oauth_token = match load_copilot_token().await? {
        Some(t) => t,
        None => return Ok(None),
    };

    // Try to load cached session token
    if let Ok(Some(session)) = load_session_token().await {
        if is_session_valid(&session) {
            tracing::debug!("Using cached Copilot session token");
            return Ok(Some(CopilotCredentials {
                token: session.token,
                api_endpoint: session.api_endpoint,
                oauth_token: oauth_token.access_token.clone(),
            }));
        }
        tracing::debug!("Cached session token expired, refreshing...");
    }

    // Exchange OAuth token for session token
    tracing::info!("Exchanging OAuth token for Copilot session token");
    let session = aither_copilot::auth::get_session_token(&oauth_token.access_token).await?;

    // Cache the session token
    if let Err(e) = save_session_token(&session).await {
        tracing::warn!("Failed to cache session token: {}", e);
    }

    Ok(Some(CopilotCredentials {
        token: session.token,
        api_endpoint: session.api_endpoint,
        oauth_token: oauth_token.access_token,
    }))
}
