//! `OpenRouter` OAuth login.
//!
//! `OpenRouter`'s PKCE flow needs no client registration: the user authorizes
//! at `https://openrouter.ai/auth`, the browser is redirected to a loopback
//! listener on `localhost:54549`, and the returned code exchanges for a
//! durable `sk-or-` API key that is stored as the provider's `api_key` — no
//! token file, no refresh.
//!
//! Where a loopback callback cannot reach the machine (SSH, container), the
//! TUI falls back to manual entry: the pasted callback URL or code goes
//! through the same exchange, and a pasted `sk-or-` key is validated against
//! `https://openrouter.ai/api/v1/auth/key` instead.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Authorization page the user completes in a browser.
const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";

/// Code → API-key exchange endpoint.
const KEYS_URL: &str = "https://openrouter.ai/api/v1/auth/keys";

/// Key introspection endpoint used to validate a pasted API key.
const KEY_CHECK_URL: &str = "https://openrouter.ai/api/v1/auth/key";

/// Loopback callback port.
pub const CALLBACK_PORT: u16 = 54549;

/// Loopback callback path.
const CALLBACK_PATH: &str = "/callback";

/// Prefix of `OpenRouter` API keys.
const API_KEY_PREFIX: &str = "sk-or-";

/// Buffer for reading the OAuth callback HTTP request.
const CALLBACK_BUF: usize = 8192;

/// Error type for `OpenRouter` login operations.
#[derive(Debug, thiserror::Error)]
pub enum OpenrouterAuthError {
    /// HTTP request to `OpenRouter` failed.
    #[error("OpenRouter request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The exchange/validation endpoint returned a non-success status.
    #[error("OpenRouter returned {status}: {body}")]
    Status {
        /// HTTP status code.
        status: reqwest::StatusCode,
        /// Truncated response body.
        body: String,
    },
    /// Loopback listener or socket I/O failed.
    #[error("callback listener failed: {0}")]
    Io(#[from] std::io::Error),
    /// The callback request was malformed or carried an error.
    #[error("invalid OAuth callback: {0}")]
    Callback(String),
    /// The API response did not contain the expected payload.
    #[error("unexpected OpenRouter response: {0}")]
    Protocol(String),
}

/// PKCE verifier/challenge pair for one login attempt.
#[derive(Debug)]
pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    /// Generate a fresh PKCE pair.
    ///
    /// # Panics
    ///
    /// Panics if the OS secure random source is unavailable.
    #[must_use]
    pub fn generate() -> Self {
        let mut verifier_bytes = [0u8; 32];
        getrandom::fill(&mut verifier_bytes).expect("secure random source unavailable");
        let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }

    /// The `code_verifier` sent during key exchange.
    #[must_use]
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    /// Build the authorization URL for this challenge.
    ///
    /// `OpenRouter` uses `callback_url` instead of `redirect_uri` and does not
    /// take `client_id`, `response_type`, or `state` parameters.
    ///
    /// # Panics
    ///
    /// Panics if the constant [`AUTHORIZE_URL`] fails to parse (a build-time
    /// invariant).
    #[must_use]
    pub fn authorize_url(&self) -> String {
        let mut url = url::Url::parse(AUTHORIZE_URL).expect("constant URL parses");
        url.query_pairs_mut()
            .append_pair("callback_url", redirect_uri())
            .append_pair("code_challenge", &self.challenge)
            .append_pair("code_challenge_method", "S256");
        url.into()
    }
}

/// The redirect URI the flow advertises.
#[must_use]
pub const fn redirect_uri() -> &'static str {
    "http://localhost:54549/callback"
}

/// Extract the authorization `code` out of a pasted callback URL or return
/// the trimmed input unchanged when it is not a URL.
///
/// # Errors
///
/// Returns [`OpenrouterAuthError::Callback`] when the pasted URL carries no
/// `code` parameter.
pub fn code_from_pasted(input: &str) -> Result<String, OpenrouterAuthError> {
    let input = input.trim();
    let Ok(url) = url::Url::parse(input) else {
        return Ok(input.to_string());
    };
    for (key, value) in url.query_pairs() {
        if key == "code" {
            return Ok(value.into_owned());
        }
    }
    Err(OpenrouterAuthError::Callback(format!(
        "pasted URL has no code parameter: {input}"
    )))
}

/// Whether the pasted input looks like a raw `OpenRouter` API key.
#[must_use]
pub fn is_api_key(input: &str) -> bool {
    input.trim().starts_with(API_KEY_PREFIX)
}

/// Validate an `sk-or-` API key against the introspection endpoint.
///
/// # Errors
///
/// Returns [`OpenrouterAuthError`] when the request fails or the key is
/// rejected.
pub async fn validate_api_key(api_key: &str) -> Result<(), OpenrouterAuthError> {
    let response = reqwest::Client::new()
        .get(KEY_CHECK_URL)
        .bearer_auth(api_key)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await?;
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(OpenrouterAuthError::Status {
        status,
        body: body.chars().take(500).collect(),
    })
}

/// Exchange an authorization code + PKCE verifier for the durable API key.
///
/// # Errors
///
/// Returns [`OpenrouterAuthError`] on transport errors, rejection by the keys
/// endpoint, or a response missing the `key` field.
pub async fn exchange_code(code: &str, verifier: &str) -> Result<String, OpenrouterAuthError> {
    let response = reqwest::Client::new()
        .post(KEYS_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&serde_json::json!({
            "code": code,
            "code_verifier": verifier,
            "code_challenge_method": "S256",
        }))
        .send()
        .await?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(OpenrouterAuthError::Status {
            status,
            body: body.chars().take(500).collect(),
        });
    }
    let body: KeyExchangeResponse = response.json().await?;
    if body.key.is_empty() {
        return Err(OpenrouterAuthError::Protocol("empty key".into()));
    }
    Ok(body.key)
}

#[derive(Debug, Deserialize)]
struct KeyExchangeResponse {
    key: String,
}

/// Listen on `localhost:54549` for a single OAuth callback and return the
/// authorization code.
///
/// Only the first request on the port is handled; `OpenRouter` sends no
/// `state` parameter, so there is nothing to validate. A minimal success page
/// is served to the browser.
///
/// # Errors
///
/// - [`OpenrouterAuthError::Io`] if the port cannot be bound or the socket
///   fails.
/// - [`OpenrouterAuthError::Callback`] on malformed requests or a callback
///   carrying an `error` parameter.
#[cfg(not(target_arch = "wasm32"))]
pub async fn listen_for_callback() -> Result<String, OpenrouterAuthError> {
    use futures_lite::{AsyncReadExt, AsyncWriteExt};

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], CALLBACK_PORT));
    let listener = async_io::Async::<std::net::TcpListener>::bind(addr)?;
    tracing::info!(port = CALLBACK_PORT, "waiting for OpenRouter callback");

    let (mut stream, _) = listener.accept().await?;
    let mut buf = vec![0u8; CALLBACK_BUF];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let (code, error) = match parse_callback_request(&request) {
        Ok(code) => (Some(code), None),
        Err(err) => (None, Some(err.to_string())),
    };

    let (status, body) = code.as_ref().map_or(
        (
            "400 Bad Request",
            "<html><body><h1>Login failed</h1><p>Return to the terminal for details.</p></body></html>",
        ),
        |_| {
            (
                "200 OK",
                "<html><body><h1>Logged in</h1><p>You may close this tab and return to May.</p></body></html>",
            )
        },
    );
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.close().await;

    match (code, error) {
        (Some(code), _) => Ok(code),
        (None, Some(err)) => Err(OpenrouterAuthError::Callback(err)),
        _ => Err(OpenrouterAuthError::Callback("empty callback".into())),
    }
}

/// Parse `GET /callback?code=…` out of the raw HTTP request.
#[cfg(not(target_arch = "wasm32"))]
fn parse_callback_request(request: &str) -> Result<String, OpenrouterAuthError> {
    let path = request
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| OpenrouterAuthError::Callback("malformed HTTP request".into()))?;
    let url = url::Url::parse(&format!("http://localhost{path}"))
        .map_err(|e| OpenrouterAuthError::Callback(format!("malformed callback path: {e}")))?;

    if url.path() != CALLBACK_PATH {
        return Err(OpenrouterAuthError::Callback(format!(
            "unexpected callback path {}",
            url.path()
        )));
    }

    let mut code = None;
    let mut error = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }

    if let Some(error) = error {
        return Err(OpenrouterAuthError::Callback(format!(
            "authorization failed: {error}"
        )));
    }
    code.ok_or_else(|| OpenrouterAuthError::Callback("callback missing code".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_from_pasted_returns_bare_code() {
        assert_eq!(code_from_pasted("  abc123 ").unwrap(), "abc123");
    }

    #[test]
    fn code_from_pasted_extracts_code_param() {
        assert_eq!(
            code_from_pasted("http://localhost:54549/callback?code=def456").unwrap(),
            "def456"
        );
    }

    #[test]
    fn code_from_pasted_rejects_url_without_code() {
        assert!(code_from_pasted("http://localhost:54549/callback?error=denied").is_err());
    }

    #[test]
    fn is_api_key_matches_sk_or_prefix() {
        assert!(is_api_key("sk-or-v1-abc"));
        assert!(is_api_key("  sk-or-v1-abc\n"));
        assert!(!is_api_key("sk-ant-abc"));
        assert!(!is_api_key("http://localhost/callback?code=x"));
    }

    #[test]
    fn authorize_url_carries_callback_and_challenge() {
        let url = url::Url::parse(&Pkce::generate().authorize_url()).unwrap();
        assert_eq!(url.as_str().split('?').next().unwrap(), AUTHORIZE_URL);
        let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(params["callback_url"], redirect_uri());
        assert_eq!(params["code_challenge_method"], "S256");
        assert!(!params["code_challenge"].is_empty());
    }
}
