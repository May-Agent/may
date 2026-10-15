//! GitHub Copilot token persistence.
//!
//! Manages OAuth tokens for GitHub Copilot authentication.
//! OAuth tokens are stored in `~/.may/copilot_token.json`.
//!
//! The OAuth token must be exchanged for a session token before making API calls.
//! Use [`get_session_token`] to get a session token for API calls.

use crate::auth_store::{delete_file, load_json, save_json};
use crate::config::MayConfig;
use aither_copilot::auth::{CopilotToken, SessionToken};
use std::path::PathBuf;

/// Token file name for OAuth token (long-lived).
const TOKEN_FILE: &str = "copilot_token.json";

/// Token file name for session token (short-lived, cached).
const SESSION_FILE: &str = "copilot_session.json";

/// Error type for Copilot token operations.
#[derive(Debug, thiserror::Error)]
pub enum CopilotTokenError {
    /// Failed to get May directory.
    #[error("Failed to get May directory: {0}")]
    MayDir(#[from] crate::config::ConfigError),
    /// Credential store error.
    #[error("Credential store error: {0}")]
    Store(#[from] crate::auth_store::AuthStoreError),
    /// Token exchange failed.
    #[error("Token exchange failed: {0}")]
    Exchange(#[from] aither_copilot::CopilotError),
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
///
/// # Errors
///
/// Returns [`CopilotTokenError`] when the token path cannot be resolved or
/// the token file cannot be read or parsed.
pub async fn load_copilot_token() -> Result<Option<CopilotToken>, CopilotTokenError> {
    Ok(load_json(&token_path()?).await?)
}

/// Save the Copilot token to disk.
///
/// # Errors
///
/// Returns [`CopilotTokenError`] when the token path cannot be resolved or
/// the write fails.
pub async fn save_copilot_token(token: &CopilotToken) -> Result<(), CopilotTokenError> {
    save_json(&token_path()?, token).await?;
    Ok(())
}

/// Delete the Copilot token from disk.
///
/// # Errors
///
/// Returns [`CopilotTokenError`] when the token path cannot be resolved or
/// removing the file fails.
pub async fn delete_copilot_token() -> Result<(), CopilotTokenError> {
    delete_file(&token_path()?).await?;
    Ok(())
}

/// Delete cached Copilot session token from disk.
///
/// # Errors
///
/// Returns [`CopilotTokenError`] when the session-token path cannot be
/// resolved or removing the file fails.
pub async fn delete_session_token() -> Result<(), CopilotTokenError> {
    delete_file(&session_path()?).await?;
    Ok(())
}

/// Delete both OAuth and cached session tokens from disk.
///
/// # Errors
///
/// Returns [`CopilotTokenError`] when either token deletion fails.
pub async fn clear_copilot_credentials() -> Result<(), CopilotTokenError> {
    delete_session_token().await?;
    delete_copilot_token().await?;
    Ok(())
}

/// Check if a Copilot token exists on disk.
#[must_use]
pub fn has_copilot_token() -> bool {
    token_path().is_ok_and(|p| p.exists())
}

/// Load cached session token from disk.
async fn load_session_token() -> Result<Option<SessionToken>, CopilotTokenError> {
    Ok(load_json(&session_path()?).await?)
}

/// Save session token to disk.
async fn save_session_token(token: &SessionToken) -> Result<(), CopilotTokenError> {
    save_json(&session_path()?, token).await?;
    Ok(())
}

/// Check if a session token is still valid (not expired).
fn is_session_valid(token: &SessionToken) -> bool {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));

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
///
/// # Errors
///
/// Returns [`CopilotTokenError`] when loading the OAuth token fails or the
/// session-token exchange with GitHub fails.
pub async fn get_copilot_credentials() -> Result<Option<CopilotCredentials>, CopilotTokenError> {
    // First, check if we have an OAuth token
    let Some(oauth_token) = load_copilot_token().await? else {
        return Ok(None);
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
