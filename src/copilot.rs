//! GitHub Copilot token persistence.
//!
//! Manages OAuth tokens for GitHub Copilot authentication.
//! OAuth tokens are stored in `~/.may/copilot_token.json`.
//!
//! The OAuth token must be exchanged for a session token before making API calls.
//! Use [`get_session_token`] to get a session token for API calls.

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

/// Get the path to the Copilot token file.
fn token_path() -> Result<PathBuf, CopilotTokenError> {
    Ok(MayConfig::may_dir()?.join(TOKEN_FILE))
}

/// Load the Copilot token from disk.
///
/// Returns `Ok(None)` if the token file doesn't exist.
/// Returns `Ok(Some(token))` if the token was loaded successfully.
/// Returns `Err` if there was an error reading or parsing the file.
pub fn load_copilot_token() -> Result<Option<CopilotToken>, CopilotTokenError> {
    let path = token_path()?;

    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path).map_err(CopilotTokenError::Read)?;
    let token: CopilotToken = serde_json::from_str(&content)?;

    Ok(Some(token))
}

/// Save the Copilot token to disk.
pub fn save_copilot_token(token: &CopilotToken) -> Result<(), CopilotTokenError> {
    let path = token_path()?;

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(CopilotTokenError::Write)?;
    }

    let content = serde_json::to_string_pretty(token)?;
    std::fs::write(&path, content).map_err(CopilotTokenError::Write)?;

    Ok(())
}

/// Delete the Copilot token from disk.
pub fn delete_copilot_token() -> Result<(), CopilotTokenError> {
    let path = token_path()?;

    if path.exists() {
        std::fs::remove_file(&path).map_err(CopilotTokenError::Write)?;
    }

    Ok(())
}

/// Delete cached Copilot session token from disk.
pub fn delete_session_token() -> Result<(), CopilotTokenError> {
    let path = session_path()?;

    if path.exists() {
        std::fs::remove_file(&path).map_err(CopilotTokenError::Write)?;
    }

    Ok(())
}

/// Delete both OAuth and cached session tokens from disk.
pub fn clear_copilot_credentials() -> Result<(), CopilotTokenError> {
    delete_session_token()?;
    delete_copilot_token()?;
    Ok(())
}

/// Check if a Copilot token exists on disk.
pub fn has_copilot_token() -> bool {
    token_path().map(|p| p.exists()).unwrap_or(false)
}

/// Get the path to the session token file.
fn session_path() -> Result<PathBuf, CopilotTokenError> {
    Ok(MayConfig::may_dir()?.join(SESSION_FILE))
}

/// Load cached session token from disk.
fn load_session_token() -> Result<Option<SessionToken>, CopilotTokenError> {
    let path = session_path()?;

    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path).map_err(CopilotTokenError::Read)?;
    let token: SessionToken = serde_json::from_str(&content)?;

    Ok(Some(token))
}

/// Save session token to disk.
fn save_session_token(token: &SessionToken) -> Result<(), CopilotTokenError> {
    let path = session_path()?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(CopilotTokenError::Write)?;
    }

    let content = serde_json::to_string_pretty(token)?;
    std::fs::write(&path, content).map_err(CopilotTokenError::Write)?;

    Ok(())
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
    let oauth_token = match load_copilot_token()? {
        Some(t) => t,
        None => return Ok(None),
    };

    // Try to load cached session token
    if let Ok(Some(session)) = load_session_token() {
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
    if let Err(e) = save_session_token(&session) {
        tracing::warn!("Failed to cache session token: {}", e);
    }

    Ok(Some(CopilotCredentials {
        token: session.token,
        api_endpoint: session.api_endpoint,
        oauth_token: oauth_token.access_token,
    }))
}
