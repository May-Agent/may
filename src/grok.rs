//! xAI Grok subscription OAuth.
//!
//! Logs in with an xAI (X) account through the OAuth 2.0 device-authorization
//! flow used by the official Grok CLI, giving SuperGrok / X Premium+
//! subscribers access to `api.x.ai` models without a pay-per-use API key.
//!
//! Credentials are stored in `~/.may/xai_token.json`. The access token is a
//! short-lived JWT sent as the `api.x.ai` bearer; refresh tokens rotate on
//! every refresh and the stored credentials are updated through
//! [`save_credentials`] whenever a rotation happens.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::auth_store::{delete_file, load_json, save_json};
use crate::config::MayConfig;

/// Token file name for xAI OAuth credentials.
const TOKEN_FILE: &str = "xai_token.json";

/// OAuth client ID registered for CLI device authorization (the same ID the
/// official Grok CLI uses).
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";

/// Scopes requested for Grok CLI inference plus refresh and identity.
const SCOPES: &str = "openid profile email offline_access grok-cli:access api:access";

/// Device authorization endpoint.
const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";

/// OIDC discovery document; the token endpoint is read from it rather than
/// hardcoded.
const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";

/// OIDC userinfo endpoint (account email for display).
const USERINFO_URL: &str = "https://auth.x.ai/oauth2/userinfo";

/// Environment variable supplying an xAI OAuth token directly (headless use).
const OAUTH_TOKEN_ENV: &str = "XAI_OAUTH_TOKEN";

/// Device-flow grant type.
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Error type for xAI Grok auth operations.
#[derive(Debug, thiserror::Error)]
pub enum GrokAuthError {
    /// Failed to resolve the May config directory.
    #[error("Failed to get May directory: {0}")]
    MayDir(#[from] crate::config::ConfigError),
    /// Credential file I/O or parse failure.
    #[error("Credential store error: {0}")]
    Store(#[from] crate::auth_store::AuthStoreError),
    /// HTTP request to the xAI auth service failed.
    #[error("xAI auth request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The auth service returned a non-success status.
    #[error("xAI auth service returned {status}: {body}")]
    Status {
        /// HTTP status code.
        status: reqwest::StatusCode,
        /// Truncated response body.
        body: String,
    },
    /// The user has not yet authorized the device code.
    #[error("device authorization still pending")]
    AuthorizationPending,
    /// The device code expired before the user authorized it.
    #[error("device code expired; restart the login")]
    Expired,
    /// The user denied the authorization request.
    #[error("device authorization was denied")]
    Denied,
    /// The auth service responded with an unexpected payload.
    #[error("unexpected xAI auth response: {0}")]
    Protocol(String),
}

/// Stored xAI OAuth credential material.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrokCredentials {
    /// Bearer token for `api.x.ai` requests.
    pub access_token: String,
    /// Refresh token; rotated on every refresh.
    pub refresh_token: String,
    /// Access-token expiry as epoch seconds, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Account email, fetched from userinfo at login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl GrokCredentials {
    /// Whether the access token is expired (or within `skew` of expiring).
    /// Missing expiry counts as valid — some responses omit `expires_in`.
    #[must_use]
    pub fn is_expired(&self, skew: Duration) -> bool {
        let Some(expires_at) = self.expires_at else {
            return false;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        expires_at <= now.saturating_add(skew.as_secs())
    }
}

/// Device authorization response shown to the user and polled.
#[derive(Debug)]
pub struct GrokDeviceCode {
    /// Opaque code polled at the token endpoint.
    pub device_code: String,
    /// Short code the user types at the verification page.
    pub user_code: String,
    /// Verification page URL.
    pub verification_uri: String,
    /// Verification URL that already carries the user code, when provided.
    pub verification_uri_complete: Option<String>,
    /// Poll interval requested by the server.
    pub interval: Duration,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DiscoveryDocument {
    token_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserinfoResponse {
    #[serde(default)]
    email: Option<String>,
}

/// Path to the stored credential file.
///
/// # Errors
///
/// Returns [`GrokAuthError`] when the May config directory cannot be resolved.
pub fn token_path() -> Result<PathBuf, GrokAuthError> {
    Ok(MayConfig::may_dir()?.join(TOKEN_FILE))
}

/// Load stored credentials; `Ok(None)` when the file does not exist.
///
/// # Errors
///
/// Returns [`GrokAuthError`] when the credential path cannot be resolved or
/// the file exists but cannot be read or parsed.
pub async fn load_credentials() -> Result<Option<GrokCredentials>, GrokAuthError> {
    Ok(load_json(&token_path()?).await?)
}

/// Persist credentials (called on login and on every token rotation).
///
/// # Errors
///
/// Returns [`GrokAuthError`] when the credential path cannot be resolved or
/// the write fails.
pub async fn save_credentials(credentials: &GrokCredentials) -> Result<(), GrokAuthError> {
    save_json(&token_path()?, credentials).await?;
    Ok(())
}

/// Delete stored credentials (logout or after an unrecoverable auth failure).
///
/// # Errors
///
/// Returns [`GrokAuthError`] when the credential path cannot be resolved or
/// removing the file fails.
pub async fn delete_credentials() -> Result<(), GrokAuthError> {
    delete_file(&token_path()?).await?;
    Ok(())
}

/// Whether a credential file exists on disk.
#[must_use]
pub fn has_credentials() -> bool {
    token_path().is_ok_and(|p| p.exists())
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

async fn read_json_or_error<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, GrokAuthError> {
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(GrokAuthError::Status {
            status,
            body: body.chars().take(500).collect(),
        });
    }
    Ok(response.json().await?)
}

/// Start device authorization: returns the code the user enters at
/// `verification_uri` and the `device_code` this flow polls.
///
/// # Errors
///
/// Returns [`GrokAuthError`] when the request fails or the response is
/// missing required fields.
pub async fn request_device_code() -> Result<GrokDeviceCode, GrokAuthError> {
    let response: DeviceCodeResponse = read_json_or_error(
        http()
            .post(DEVICE_CODE_URL)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[("client_id", CLIENT_ID), ("scope", SCOPES)])
            .send()
            .await?,
    )
    .await?;
    Ok(GrokDeviceCode {
        device_code: response.device_code,
        user_code: response.user_code,
        verification_uri: response.verification_uri,
        verification_uri_complete: response.verification_uri_complete,
        interval: Duration::from_secs(response.interval.unwrap_or(5).max(3)),
    })
}

/// Read the xAI token endpoint from the OIDC discovery document, pinned to
/// `https://*.x.ai` so a tampered document cannot redirect token material.
///
/// # Errors
///
/// Returns [`GrokAuthError`] when discovery fails or reports an endpoint
/// outside the xAI auth origin.
pub async fn token_endpoint() -> Result<String, GrokAuthError> {
    let document: DiscoveryDocument = read_json_or_error(
        http()
            .get(DISCOVERY_URL)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await?,
    )
    .await?;
    let endpoint = document.token_endpoint;
    let parsed = url::Url::parse(&endpoint).map_err(|e| GrokAuthError::Protocol(e.to_string()))?;
    let host = parsed.host_str().unwrap_or_default();
    if parsed.scheme() != "https" || !(host == "x.ai" || host.ends_with(".x.ai")) {
        return Err(GrokAuthError::Protocol(format!(
            "invalid token_endpoint {endpoint}"
        )));
    }
    Ok(endpoint)
}

fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn credentials_from(response: TokenResponse, refresh_token: Option<String>) -> GrokCredentials {
    GrokCredentials {
        access_token: response.access_token,
        refresh_token: response.refresh_token.or(refresh_token).unwrap_or_default(),
        expires_at: response
            .expires_in
            .map(|seconds| epoch_now().saturating_add(seconds)),
        email: None,
    }
}

/// Poll the token endpoint once for a pending device authorization.
///
/// # Errors
///
/// - [`GrokAuthError::AuthorizationPending`] while the user has not entered
///   the code yet.
/// - [`GrokAuthError::Expired`] when the device code lapsed.
/// - [`GrokAuthError::Denied`] when the user rejected the request.
/// - [`GrokAuthError::Http`]/[`GrokAuthError::Status`] on transport failure.
/// - [`GrokAuthError::Protocol`] on any other error payload.
pub async fn poll_device_code(
    token_endpoint: &str,
    device_code: &str,
) -> Result<GrokCredentials, GrokAuthError> {
    let response = http()
        .post(token_endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[
            ("grant_type", DEVICE_GRANT),
            ("client_id", CLIENT_ID),
            ("device_code", device_code),
        ])
        .send()
        .await?;

    if response.status().is_success() {
        return Ok(credentials_from(
            response.json::<TokenResponse>().await?,
            None,
        ));
    }

    let error = response
        .json::<TokenErrorResponse>()
        .await
        .map_err(|e| GrokAuthError::Protocol(e.to_string()))?;
    Err(match error.error.as_str() {
        "authorization_pending" | "slow_down" => GrokAuthError::AuthorizationPending,
        "expired_token" => GrokAuthError::Expired,
        "access_denied" => GrokAuthError::Denied,
        other => {
            GrokAuthError::Protocol(error.error_description.unwrap_or_else(|| other.to_string()))
        }
    })
}

/// Exchange a refresh token for a rotated credential pair.
///
/// # Errors
///
/// Returns [`GrokAuthError`] when the request fails or the grant is rejected.
pub async fn refresh_tokens(
    token_endpoint: &str,
    refresh_token: &str,
) -> Result<GrokCredentials, GrokAuthError> {
    let response = http()
        .post(token_endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await?;

    if response.status().is_success() {
        return Ok(credentials_from(
            response.json::<TokenResponse>().await?,
            Some(refresh_token.to_string()),
        ));
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(GrokAuthError::Status {
        status,
        body: body.chars().take(500).collect(),
    })
}

/// Fetch the account email for display; failures leave it unset.
pub async fn fetch_account_email(access_token: &str) -> Option<String> {
    let response = http()
        .get(USERINFO_URL)
        .bearer_auth(access_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response
        .json::<UserinfoResponse>()
        .await
        .ok()
        .and_then(|u| u.email)
}

/// Resolve a usable `api.x.ai` bearer token.
///
/// Order: the `XAI_OAUTH_TOKEN` environment override, then the stored
/// credential — refreshing and persisting it when the access token is within
/// [`ACCESS_EXPIRY_MARGIN`] of expiry.
///
/// # Errors
///
/// Returns [`GrokAuthError`] on storage failures and when a needed token
/// refresh fails.
pub async fn get_access_token() -> Result<Option<String>, GrokAuthError> {
    Ok(get_access().await?.map(|access| access.token))
}

/// How long before its expiry an access token is refreshed.
pub const ACCESS_EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// A usable `api.x.ai` bearer token and when it expires.
#[derive(Debug, Clone)]
pub struct GrokAccess {
    /// The bearer token.
    pub token: String,
    /// When it expires, in seconds since the Unix epoch; `None` when the
    /// token endpoint gave no expiry or the token came from the environment.
    pub expires_at: Option<u64>,
}

/// [`get_access_token`]'s token with its expiry.
///
/// # Errors
///
/// Returns [`GrokAuthError`] on storage failures and when a needed token
/// refresh fails.
pub async fn get_access() -> Result<Option<GrokAccess>, GrokAuthError> {
    if let Ok(token) = std::env::var(OAUTH_TOKEN_ENV)
        && !token.trim().is_empty()
    {
        return Ok(Some(GrokAccess {
            token: token.trim().to_string(),
            expires_at: None,
        }));
    }

    let Some(mut credentials) = load_credentials().await? else {
        return Ok(None);
    };
    if credentials.is_expired(ACCESS_EXPIRY_MARGIN) {
        let endpoint = token_endpoint().await?;
        let email = credentials.email.take();
        credentials = refresh_tokens(&endpoint, &credentials.refresh_token).await?;
        credentials.email = email;
        save_credentials(&credentials).await?;
    }
    Ok(Some(GrokAccess {
        token: credentials.access_token,
        expires_at: credentials.expires_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials(expires_at: Option<u64>) -> GrokCredentials {
        GrokCredentials {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at,
            email: None,
        }
    }

    #[test]
    fn missing_expiry_counts_as_valid() {
        assert!(!credentials(None).is_expired(Duration::ZERO));
    }

    #[test]
    fn expiry_inside_skew_counts_as_expired() {
        let soon = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            + 30;
        assert!(credentials(Some(soon)).is_expired(Duration::from_secs(60)));
        assert!(!credentials(Some(soon)).is_expired(Duration::from_secs(5)));
    }

    #[test]
    fn credentials_roundtrip_keeps_optional_fields() {
        let creds = GrokCredentials {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: Some(42),
            email: Some("x@x.ai".into()),
        };
        let parsed: GrokCredentials =
            serde_json::from_str(&serde_json::to_string(&creds).unwrap()).unwrap();
        assert_eq!(parsed.access_token, "a");
        assert_eq!(parsed.refresh_token, "r");
        assert_eq!(parsed.expires_at, Some(42));
        assert_eq!(parsed.email.as_deref(), Some("x@x.ai"));
    }
}
