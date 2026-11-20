//! Per-host forge authentication.
//!
//! Every forge host May talks to names one of three modes in
//! `[forge."<host>"]`: `oauth` (May's own OAuth login, stored in the OS
//! keychain and refreshed), `token` (a personal access token in the OS
//! keychain) or `cli` (the host CLI's login: `gh auth token` or `glab
//! config get token`).
//!
//! There is no implicit fallback: a host with no `[forge]` entry makes
//! [`ForgeAuth::token_for`] return [`ForgeAuthError::Unauthenticated`]
//! immediately, without probing any CLI, keychain or environment.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use zenwave::header::{ACCEPT, CONTENT_TYPE, USER_AGENT};
use zenwave::{Client, Method, ResponseExt};

use super::{ForgeHost, ForgeHostError, ForgeKind, GITHUB_HOST, GITLAB_HOST};

/// Keychain service forge credentials are stored under.
const KEYCHAIN_SERVICE: &str = "dev.may.forge";

/// Refresh skew: an access token this close to expiry is rotated.
const ACCESS_EXPIRY_SKEW: Duration = Duration::from_secs(60);

/// How much a `slow_down` reply stretches the poll interval (RFC 8628).
const SLOW_DOWN_EXTRA: Duration = Duration::from_secs(5);

/// The device-flow grant type, shared by GitHub and GitLab.
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// OAuth application `client_id` May registered for `github.com`; not
/// registered yet.
const GITHUB_COM_CLIENT_ID: Option<&str> = None;
/// OAuth application `client_id` May registered for `gitlab.com`; not
/// registered yet.
const GITLAB_COM_CLIENT_ID: Option<&str> = None;

/// The `client_id` May's own OAuth registration takes for `host`, where one
/// exists; `None` where the host's `client_id` must come from the config.
fn builtin_client_id(kind: ForgeKind, host: &str) -> Option<&'static str> {
    match (kind, host) {
        (ForgeKind::GitHub, GITHUB_HOST) => GITHUB_COM_CLIENT_ID,
        (ForgeKind::GitLab, GITLAB_HOST) => GITLAB_COM_CLIENT_ID,
        _ => None,
    }
}

/// Where the token for a forge host comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ForgeAuthMode {
    /// May's own OAuth login: device flow or PKCE, stored in the OS
    /// keychain, refreshed automatically.
    Oauth,
    /// A personal access token stored in the OS keychain
    /// ([`ForgeAuth::store_pat`]).
    Token,
    /// The host CLI's own login: `gh auth token --hostname <host>` for
    /// GitHub, `glab config get token --host <host>` for GitLab.
    Cli,
}

/// One `[forge."<host>"]` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeHostConfig {
    /// The forge software the host runs; required for hosts that are
    /// neither `github.com` nor `gitlab.com`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ForgeKind>,
    /// How May authenticates to the host.
    pub mode: ForgeAuthMode,
    /// OAuth application `client_id`, required with `mode = "oauth"` where
    /// May has no built-in registration for the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

impl ForgeHostConfig {
    /// The kind this entry names, or the kind `host`'s name implies.
    #[must_use]
    pub fn kind_for(&self, host: &str) -> Option<ForgeKind> {
        self.kind.or_else(|| ForgeKind::of_host(host))
    }
}

/// What forge authentication can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ForgeAuthError {
    /// The host has no `[forge]` entry, or its entry's credential store is
    /// empty. Nothing was probed.
    #[error(
        "forge host {host:?} is not authenticated; configure [forge.\"{host}\"] in config.toml"
    )]
    Unauthenticated {
        /// The host that could not be authenticated.
        host: String,
    },
    /// A host string is not a valid `host[:port]`.
    #[error(transparent)]
    InvalidHost(#[from] ForgeHostError),
    /// The host's kind is not configured and not implied by its name.
    #[error("forge kind of {host:?} is unknown; set kind in [forge.\"{host}\"]")]
    KindNotConfigured {
        /// The host whose kind is missing.
        host: String,
    },
    /// `mode = "oauth"` needs a `client_id` and the host has none.
    #[error("OAuth client_id is not configured for {host:?}; set client_id in [forge.\"{host}\"]")]
    ClientIdNotConfigured {
        /// The host whose `client_id` is missing.
        host: String,
    },
    /// OS keychain failure.
    #[error("keychain error: {0}")]
    Keychain(#[from] keyring::Error),
    /// The host CLI could not produce a token.
    #[error("`{program}` did not yield a token for {host:?}: {message}")]
    Cli {
        /// The host whose token was requested.
        host: String,
        /// The program that ran.
        program: &'static str,
        /// What went wrong.
        message: String,
    },
    /// Transport failure against the forge's OAuth endpoints.
    #[error("OAuth request for {host} failed: {source}")]
    Transport {
        /// The host whose OAuth endpoint was called.
        host: String,
        /// The transport error.
        source: zenwave::Error,
    },
    /// The OAuth endpoint answered but not with the expected payload.
    #[error("OAuth endpoint of {host}: {message}")]
    Protocol {
        /// The host whose endpoint misbehaved.
        host: String,
        /// What the endpoint answered.
        message: String,
    },
    /// The user declined the device authorization.
    #[error("device authorization for {host} was denied")]
    Denied {
        /// The host the device flow ran against.
        host: String,
    },
    /// The device code expired before authorization.
    #[error("device code for {host} expired; restart the login")]
    Expired {
        /// The host the device flow ran against.
        host: String,
    },
    /// A credential stored in the keychain does not parse.
    #[error("stored credentials for {host} are malformed: {message}")]
    StoredCredentials {
        /// The host whose credential is malformed.
        host: String,
        /// Why it does not parse.
        message: String,
    },
    /// The flow is not defined for the host's forge kind.
    #[error("{feature} is not supported on {host} ({kind:?})")]
    Unsupported {
        /// The host the flow was tried against.
        host: String,
        /// The host's forge kind.
        kind: ForgeKind,
        /// The flow that was tried.
        feature: &'static str,
    },
}

/// OAuth credential material stored in the OS keychain as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthCredentials {
    /// Bearer token for the forge's API and HTTPS git.
    pub access_token: String,
    /// Refresh token; rotated on every refresh where the forge rotates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Access-token expiry as epoch seconds, when the forge reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_epoch_secs: Option<i64>,
    /// Granted scopes, when the forge reports them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
}

impl OAuthCredentials {
    /// Whether the access token is expired or within `skew` of it. Missing
    /// expiry counts as valid — non-expiring OAuth tokens report none.
    #[must_use]
    pub fn is_expired(&self, skew: Duration) -> bool {
        let Some(expires_at) = self.expires_at_epoch_secs else {
            return false;
        };
        let skew = i64::try_from(skew.as_secs()).unwrap_or(i64::MAX);
        expires_at <= epoch_now().saturating_add(skew)
    }
}

fn epoch_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// A device authorization as the forge returns it: the code the user enters
/// at the verification page and the `device_code` polled meanwhile.
#[derive(Debug, Clone)]
pub struct DeviceAuthorization {
    /// Opaque code polled at the token endpoint.
    pub device_code: String,
    /// Short code the user types at `verification_uri`.
    pub user_code: String,
    /// Verification page URL.
    pub verification_uri: String,
    /// Verification URL carrying the user code, when the forge sends one.
    pub verification_uri_complete: Option<String>,
    /// Poll interval the server asked for.
    pub interval: Duration,
    /// How long the device code stays valid.
    pub expires_in: Duration,
}

/// What one device-flow poll learned.
#[derive(Debug)]
pub enum DevicePoll {
    /// Still waiting on the user; sleep `retry_after` and poll again.
    Pending {
        /// When to poll next — `interval`, or more after a `slow_down`.
        retry_after: Duration,
    },
    /// The user authorized; these are the credentials.
    Authorized(OAuthCredentials),
}

/// PKCE verifier/challenge pair for one authorization-code login.
#[derive(Debug)]
pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    /// A fresh pair: 32 random bytes as the verifier, its SHA-256 as the
    /// S256 challenge, both base64url without padding.
    ///
    /// # Panics
    /// Panics when the OS secure random source is unavailable.
    #[must_use]
    pub fn generate() -> Self {
        use base64::Engine as _;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use sha2::{Digest, Sha256};

        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("secure random source unavailable");
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }

    /// The `code_verifier` sent at code exchange.
    #[must_use]
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    /// The `code_challenge` sent in the authorization URL.
    #[must_use]
    pub fn challenge(&self) -> &str {
        &self.challenge
    }
}

/// The OAuth endpoints of one forge host.
#[derive(Debug)]
pub struct ForgeOAuth {
    kind: ForgeKind,
    client_id: String,
    /// `https://<host>` in production; a loopback base in tests.
    base: String,
    /// How errors name the host.
    label: String,
}

impl ForgeOAuth {
    /// The endpoints `host` serves over HTTPS.
    #[must_use]
    pub fn new(kind: ForgeKind, host: &ForgeHost, client_id: String) -> Self {
        Self {
            kind,
            client_id,
            base: format!("https://{host}"),
            label: host.as_str().to_string(),
        }
    }

    /// The endpoints under `base` — a loopback mock or a gateway.
    #[must_use]
    pub fn with_base_url(kind: ForgeKind, client_id: String, base: String) -> Self {
        Self {
            kind,
            client_id,
            label: base.clone(),
            base,
        }
    }

    fn device_code_url(&self) -> String {
        let path = match self.kind {
            ForgeKind::GitHub => "/login/device/code",
            ForgeKind::GitLab => "/oauth/authorize_device",
        };
        format!("{}{path}", self.base)
    }

    fn token_url(&self) -> String {
        let path = match self.kind {
            ForgeKind::GitHub => "/login/oauth/access_token",
            ForgeKind::GitLab => "/oauth/token",
        };
        format!("{}{path}", self.base)
    }

    fn protocol(&self, message: impl Into<String>) -> ForgeAuthError {
        ForgeAuthError::Protocol {
            host: self.label.clone(),
            message: message.into(),
        }
    }

    /// POST a form and parse the JSON reply, whatever the status.
    async fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<serde_json::Value, ForgeAuthError> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form.iter().copied())
            .finish();
        let mut client = zenwave::client();
        let request = client
            .method(Method::POST, url)
            .map_err(|error| self.protocol(error.to_string()))?
            .header(ACCEPT, "application/json")
            .and_then(|request| request.header(CONTENT_TYPE, "application/x-www-form-urlencoded"))
            .and_then(|request| request.header(USER_AGENT, "may-agent"))
            .map_err(|error| self.protocol(error.to_string()))?
            .bytes_body(body.into_bytes());
        let response = match request.await {
            Ok(response) => response,
            Err(zenwave::Error::Http { response, .. }) => response.response,
            Err(source) => {
                return Err(ForgeAuthError::Transport {
                    host: self.label.clone(),
                    source,
                });
            }
        };
        let status = response.status().as_u16();
        response
            .into_json::<serde_json::Value>()
            .await
            .map_err(|error| {
                self.protocol(format!("status {status} with a non-JSON body: {error}"))
            })
    }

    /// Start a device authorization; the returned code is shown to the
    /// user while [`Self::poll_device_token`] or
    /// [`Self::wait_for_authorization`] completes the flow.
    ///
    /// # Errors
    /// Transport, protocol and endpoint errors.
    pub async fn request_device_code(
        &self,
        scopes: &str,
    ) -> Result<DeviceAuthorization, ForgeAuthError> {
        let value = self
            .post_form(
                &self.device_code_url(),
                &[("client_id", &self.client_id), ("scope", scopes)],
            )
            .await?;
        if let Some(message) = error_description(&value) {
            return Err(self.protocol(message));
        }
        let string = |field: &str| {
            value
                .get(field)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let seconds = |field: &str| value.get(field).and_then(serde_json::Value::as_u64);
        Ok(DeviceAuthorization {
            device_code: string("device_code")
                .ok_or_else(|| self.protocol("device_code missing"))?,
            user_code: string("user_code").ok_or_else(|| self.protocol("user_code missing"))?,
            verification_uri: string("verification_uri")
                .ok_or_else(|| self.protocol("verification_uri missing"))?,
            verification_uri_complete: string("verification_uri_complete"),
            interval: Duration::from_secs(seconds("interval").unwrap_or(5).max(1)),
            expires_in: Duration::from_secs(seconds("expires_in").unwrap_or(900)),
        })
    }

    /// One poll of the token endpoint for a pending device authorization.
    ///
    /// # Errors
    /// [`ForgeAuthError::Denied`], [`ForgeAuthError::Expired`], transport
    /// and protocol errors.
    pub async fn poll_device_token(
        &self,
        device_code: &str,
        interval: Duration,
    ) -> Result<DevicePoll, ForgeAuthError> {
        let value = self
            .post_form(
                &self.token_url(),
                &[
                    ("grant_type", DEVICE_GRANT),
                    ("client_id", &self.client_id),
                    ("device_code", device_code),
                ],
            )
            .await?;
        if value.get("access_token").is_some() {
            return Ok(DevicePoll::Authorized(self.credentials_from(&value, None)?));
        }
        let error = value
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        match error {
            "authorization_pending" => Ok(DevicePoll::Pending {
                retry_after: interval,
            }),
            "slow_down" => Ok(DevicePoll::Pending {
                retry_after: interval + SLOW_DOWN_EXTRA,
            }),
            "access_denied" | "authorization_declined" => Err(ForgeAuthError::Denied {
                host: self.label.clone(),
            }),
            "expired_token" => Err(ForgeAuthError::Expired {
                host: self.label.clone(),
            }),
            _ => Err(self.protocol(
                error_description(&value)
                    .unwrap_or_else(|| "unexpected token response".to_string()),
            )),
        }
    }

    /// Poll until the user authorizes or the device code lapses, sleeping
    /// the interval the server names and the extra `slow_down` seconds.
    ///
    /// # Errors
    /// [`ForgeAuthError::Denied`], [`ForgeAuthError::Expired`], transport
    /// and protocol errors.
    pub async fn wait_for_authorization(
        &self,
        authorization: &DeviceAuthorization,
    ) -> Result<OAuthCredentials, ForgeAuthError> {
        let mut interval = authorization.interval;
        let deadline = Instant::now() + authorization.expires_in;
        loop {
            async_io::Timer::after(interval).await;
            if Instant::now() >= deadline {
                return Err(ForgeAuthError::Expired {
                    host: self.label.clone(),
                });
            }
            match self
                .poll_device_token(&authorization.device_code, interval)
                .await?
            {
                DevicePoll::Pending { retry_after } => interval = retry_after,
                DevicePoll::Authorized(credentials) => return Ok(credentials),
            }
        }
    }

    /// The GitLab authorization URL a browser completes for the PKCE
    /// authorization-code flow.
    ///
    /// # Errors
    /// [`ForgeAuthError::Unsupported`] when this client is not GitLab.
    pub fn authorization_url(
        &self,
        redirect_uri: &str,
        scopes: &str,
        state: &str,
        pkce: &Pkce,
    ) -> Result<String, ForgeAuthError> {
        self.require_gitlab("PKCE authorization-code flow")?;
        let mut url = url::Url::parse(&format!("{}/oauth/authorize", self.base))
            .map_err(|error| self.protocol(format!("authorize URL does not parse: {error}")))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", scopes)
            .append_pair("state", state)
            .append_pair("code_challenge", pkce.challenge())
            .append_pair("code_challenge_method", "S256");
        Ok(url.to_string())
    }

    /// Exchange the `code` a completed GitLab authorization returned for
    /// credentials, proving the exchange with `pkce`'s verifier.
    ///
    /// # Errors
    /// [`ForgeAuthError::Unsupported`] when this client is not GitLab,
    /// plus transport, protocol and endpoint errors.
    pub async fn exchange_authorization_code(
        &self,
        code: &str,
        pkce: &Pkce,
        redirect_uri: &str,
    ) -> Result<OAuthCredentials, ForgeAuthError> {
        self.require_gitlab("PKCE authorization-code flow")?;
        let value = self
            .post_form(
                &self.token_url(),
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", &self.client_id),
                    ("code", code),
                    ("redirect_uri", redirect_uri),
                    ("code_verifier", pkce.verifier()),
                ],
            )
            .await?;
        if let Some(message) = error_description(&value) {
            return Err(self.protocol(message));
        }
        self.credentials_from(&value, None)
    }

    /// Exchange a refresh token for a rotated credential pair.
    ///
    /// GitHub asks for no `client_secret` on tokens the device flow minted;
    /// GitLab asks for none on public clients.
    ///
    /// # Errors
    /// Transport, protocol and grant-rejection errors.
    pub async fn refresh(&self, refresh_token: &str) -> Result<OAuthCredentials, ForgeAuthError> {
        let value = self
            .post_form(
                &self.token_url(),
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", &self.client_id),
                    ("refresh_token", refresh_token),
                ],
            )
            .await?;
        if let Some(message) = error_description(&value) {
            return Err(self.protocol(message));
        }
        self.credentials_from(&value, Some(refresh_token.to_string()))
    }

    fn require_gitlab(&self, feature: &'static str) -> Result<(), ForgeAuthError> {
        if self.kind != ForgeKind::GitLab {
            return Err(ForgeAuthError::Unsupported {
                host: self.label.clone(),
                kind: self.kind,
                feature,
            });
        }
        Ok(())
    }

    /// Build [`OAuthCredentials`] from a token response body, keeping
    /// `fallback_refresh` when the reply carries no new refresh token.
    fn credentials_from(
        &self,
        value: &serde_json::Value,
        fallback_refresh: Option<String>,
    ) -> Result<OAuthCredentials, ForgeAuthError> {
        let access_token = value
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| self.protocol("token response lacks access_token"))?;
        let expires_at = value
            .get("expires_in")
            .and_then(serde_json::Value::as_i64)
            .map(|secs| epoch_now().saturating_add(secs));
        let scopes = value
            .get("scope")
            .and_then(serde_json::Value::as_str)
            .map_or_else(Vec::new, |scope| {
                scope
                    .split([',', ' '])
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect()
            });
        let refresh_token = value
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or(fallback_refresh);
        Ok(OAuthCredentials {
            access_token: access_token.to_string(),
            refresh_token,
            expires_at_epoch_secs: expires_at,
            scopes,
        })
    }
}

/// The `error_description`/`error` of an OAuth error body.
fn error_description(value: &serde_json::Value) -> Option<String> {
    let error = value.get("error").and_then(serde_json::Value::as_str)?;
    let description = value
        .get("error_description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(error);
    Some(format!("{description} ({error})"))
}

/// The keychain account a personal access token is stored under.
fn pat_account(kind: ForgeKind, host: &str) -> String {
    format!("pat:{}:{host}", kind.binding_prefix())
}

/// The keychain account OAuth credentials are stored under.
fn oauth_account(kind: ForgeKind, host: &str) -> String {
    format!("oauth:{}:{host}", kind.binding_prefix())
}

/// Reads, writes and deletes forge credentials in the OS keychain; tests
/// substitute an in-memory map.
#[derive(Debug, Clone, Default)]
struct ForgeKeychain {
    /// Credentials tests keep in memory instead of the OS keychain.
    #[cfg(test)]
    memory: Option<Arc<std::sync::Mutex<BTreeMap<String, String>>>>,
}

impl ForgeKeychain {
    #[cfg(test)]
    fn in_memory() -> Self {
        Self {
            memory: Some(Arc::default()),
        }
    }

    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn read(&self, account: &str) -> Result<Option<String>, ForgeAuthError> {
        #[cfg(test)]
        if let Some(memory) = &self.memory {
            return Ok(memory
                .lock()
                .expect("test credential map is not poisoned")
                .get(account)
                .cloned());
        }
        match keyring::Entry::new(KEYCHAIN_SERVICE, account)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn write(&self, account: &str, value: &str) -> Result<(), ForgeAuthError> {
        #[cfg(test)]
        if let Some(memory) = &self.memory {
            memory
                .lock()
                .expect("test credential map is not poisoned")
                .insert(account.to_string(), value.to_string());
            return Ok(());
        }
        keyring::Entry::new(KEYCHAIN_SERVICE, account)?.set_password(value)?;
        Ok(())
    }

    /// Delete `account`; returns whether it existed.
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn delete(&self, account: &str) -> Result<bool, ForgeAuthError> {
        #[cfg(test)]
        if let Some(memory) = &self.memory {
            return Ok(memory
                .lock()
                .expect("test credential map is not poisoned")
                .remove(account)
                .is_some());
        }
        match keyring::Entry::new(KEYCHAIN_SERVICE, account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

/// `(program, args)` the `cli` mode runs for `host`. `account` is the
/// `gh` login's `--user`; GitLab's CLI has no equivalent and ignores it.
fn cli_command(kind: ForgeKind, host: &str, account: Option<&str>) -> (&'static str, Vec<String>) {
    match kind {
        ForgeKind::GitHub => {
            let mut args = vec![
                "auth".to_string(),
                "token".to_string(),
                "--hostname".to_string(),
                host.to_string(),
            ];
            if let Some(account) = account {
                args.push("--user".to_string());
                args.push(account.to_string());
            }
            ("gh", args)
        }
        ForgeKind::GitLab => (
            "glab",
            ["config", "get", "token", "--host", host]
                .iter()
                .map(ToString::to_string)
                .collect(),
        ),
    }
}

/// Run the host CLI for `token` output, off the async executor.
async fn cli_token(
    kind: ForgeKind,
    host: &str,
    account: Option<&str>,
) -> Result<String, ForgeAuthError> {
    let (program, args) = cli_command(kind, host, account);
    let failed = |message: String| ForgeAuthError::Cli {
        host: host.to_string(),
        program,
        message,
    };
    let output =
        blocking::unblock(move || std::process::Command::new(program).args(&args).output())
            .await
            .map_err(|error| failed(format!("failed to run {program}: {error}")))?;
    if !output.status.success() {
        return Err(failed(format!(
            "it failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err(failed(format!("{program} returned an empty token")));
    }
    Ok(token)
}

/// The authenticator a `[forge]` map describes: per-host `oauth`, `token`
/// and `cli` resolution over the OS keychain.
///
/// Cloning shares the keychain view and the in-memory credential cells, so
/// a refresh racing another reader rotates once.
#[derive(Debug, Clone)]
pub struct ForgeAuth {
    keychain: ForgeKeychain,
    /// `[forge]` entries keyed by canonical `host[:port]`.
    hosts: Arc<BTreeMap<String, ForgeHostConfig>>,
    /// Per-instance OAuth credential cells: the mutex makes the
    /// read–refresh–write of `token_for` race-free.
    cells: Arc<async_lock::Mutex<BTreeMap<String, OAuthCredentials>>>,
    /// OAuth endpoint base overriding `https://<host>` (loopback tests).
    oauth_base: Option<String>,
}

impl ForgeAuth {
    /// The authenticator `config`'s `[forge]` map describes.
    #[must_use]
    pub fn new(config: &crate::config::MayConfig) -> Self {
        Self::from_config(&config.forge)
    }

    /// The authenticator the `[forge]` map itself describes.
    #[must_use]
    pub fn from_config(forge: &BTreeMap<String, ForgeHostConfig>) -> Self {
        let hosts = forge
            .iter()
            .map(|(host, entry)| {
                let canonical = ForgeHost::new(host)
                    .map_or_else(|_| host.clone(), |parsed| parsed.as_str().to_string());
                (canonical, entry.clone())
            })
            .collect();
        Self {
            keychain: ForgeKeychain::default(),
            hosts: Arc::new(hosts),
            cells: Arc::default(),
            oauth_base: None,
        }
    }

    /// An authenticator reading and writing an in-memory keychain, for tests.
    #[cfg(test)]
    fn in_memory(hosts: BTreeMap<String, ForgeHostConfig>) -> Self {
        Self {
            keychain: ForgeKeychain::in_memory(),
            hosts: Arc::new(hosts),
            cells: Arc::default(),
            oauth_base: None,
        }
    }

    /// Point this authenticator's OAuth endpoints at `base`, for tests.
    #[cfg(test)]
    fn with_oauth_base(mut self, base: impl Into<String>) -> Self {
        self.oauth_base = Some(base.into());
        self
    }

    /// The kind `host` forges: its entry's `kind`, or the kind its name implies.
    fn kind_of(&self, host: &str) -> Option<ForgeKind> {
        self.hosts
            .get(host)
            .and_then(|entry| entry.kind)
            .or_else(|| ForgeKind::of_host(host))
    }

    fn require_kind(&self, host: &str) -> Result<ForgeKind, ForgeAuthError> {
        self.kind_of(host)
            .ok_or_else(|| ForgeAuthError::KindNotConfigured {
                host: host.to_string(),
            })
    }

    /// The bearer token May uses against `host`.
    ///
    /// A host absent from the `[forge]` map is
    /// [`ForgeAuthError::Unauthenticated`] immediately — no `gh`, `glab`,
    /// keychain or environment probe ever runs for it.
    ///
    /// # Errors
    /// [`ForgeAuthError::Unauthenticated`] for an unconfigured host or an
    /// empty credential store, [`ForgeAuthError::KindNotConfigured`] for a
    /// self-hosted entry without `kind`, [`ForgeAuthError::Cli`] when the
    /// host CLI fails, transport/refresh errors for `oauth`.
    pub async fn token_for(&self, host: &str) -> Result<String, ForgeAuthError> {
        let parsed_host = ForgeHost::new(host)?;
        let host = parsed_host.as_str();
        let entry = self
            .hosts
            .get(host)
            .ok_or_else(|| ForgeAuthError::Unauthenticated {
                host: host.to_string(),
            })?;
        let kind = entry
            .kind_for(host)
            .ok_or_else(|| ForgeAuthError::KindNotConfigured {
                host: host.to_string(),
            })?;
        match entry.mode {
            ForgeAuthMode::Cli => cli_token(kind, host, None).await,
            ForgeAuthMode::Token => self
                .keychain
                .read(&pat_account(kind, host))?
                .filter(|token| !token.is_empty())
                .ok_or_else(|| ForgeAuthError::Unauthenticated {
                    host: host.to_string(),
                }),
            ForgeAuthMode::Oauth => self.oauth_token(kind, &parsed_host).await,
        }
    }

    /// The bearer token of one named account on `host`.
    ///
    /// `account` only means something under `mode = "cli"`: it is handed to
    /// `gh auth token --user <account>` on GitHub. Every other mode resolves
    /// the host's own credential and `account` is ignored, so a profile's
    /// recorded `gh` login never changes an `oauth` or `token` login.
    ///
    /// # Errors
    /// As [`Self::token_for`].
    pub async fn token_for_account(
        &self,
        host: &str,
        account: &str,
    ) -> Result<String, ForgeAuthError> {
        let parsed_host = ForgeHost::new(host)?;
        let host = parsed_host.as_str();
        let entry = self
            .hosts
            .get(host)
            .ok_or_else(|| ForgeAuthError::Unauthenticated {
                host: host.to_string(),
            })?;
        if entry.mode == ForgeAuthMode::Cli {
            let kind = entry
                .kind_for(host)
                .ok_or_else(|| ForgeAuthError::KindNotConfigured {
                    host: host.to_string(),
                })?;
            return cli_token(kind, host, Some(account)).await;
        }
        self.token_for(host).await
    }

    /// The OAuth path of [`Self::token_for`]: serve the stored access
    /// token, or rotate it through the refresh token when it is expiring.
    async fn oauth_token(
        &self,
        kind: ForgeKind,
        parsed_host: &ForgeHost,
    ) -> Result<String, ForgeAuthError> {
        let host = parsed_host.as_str();
        let unauthenticated = || ForgeAuthError::Unauthenticated {
            host: host.to_string(),
        };
        // The lock is held across the read–refresh–write: it is what makes a
        // refresh race-free between readers.
        let mut cells = self.cells.lock().await;
        let mut credentials = if let Some(cached) = cells.get(host).cloned() {
            cached
        } else {
            let json = self
                .keychain
                .read(&oauth_account(kind, host))?
                .ok_or_else(unauthenticated)?;
            serde_json::from_str(&json).map_err(|error| ForgeAuthError::StoredCredentials {
                host: host.to_string(),
                message: error.to_string(),
            })?
        };
        if credentials.is_expired(ACCESS_EXPIRY_SKEW) {
            let Some(refresh_token) = credentials.refresh_token.clone() else {
                return Err(unauthenticated());
            };
            let client = self.oauth_client_resolved(parsed_host)?;
            let mut rotated = client.refresh(&refresh_token).await?;
            if rotated.refresh_token.is_none() {
                rotated.refresh_token = Some(refresh_token);
            }
            if rotated.scopes.is_empty() {
                rotated.scopes.clone_from(&credentials.scopes);
            }
            let json = serde_json::to_string(&rotated).map_err(|error| {
                ForgeAuthError::StoredCredentials {
                    host: host.to_string(),
                    message: error.to_string(),
                }
            })?;
            self.keychain.write(&oauth_account(kind, host), &json)?;
            credentials = rotated;
        }
        cells.insert(host.to_string(), credentials.clone());
        drop(cells);
        Ok(credentials.access_token)
    }

    /// The OAuth endpoint client of `host` — its `[forge]` entry's
    /// `client_id` or May's built-in registration for the hosted forges.
    ///
    /// # Errors
    /// [`ForgeAuthError::Unauthenticated`] for an unconfigured host,
    /// [`ForgeAuthError::KindNotConfigured`] for a missing kind,
    /// [`ForgeAuthError::ClientIdNotConfigured`] when neither the entry nor
    /// a built-in registration names a `client_id`.
    pub fn oauth_client(&self, host: &str) -> Result<ForgeOAuth, ForgeAuthError> {
        let host = ForgeHost::new(host)?;
        self.oauth_client_resolved(&host)
    }

    /// [`Self::oauth_client`] for a parsed host.
    fn oauth_client_resolved(&self, parsed_host: &ForgeHost) -> Result<ForgeOAuth, ForgeAuthError> {
        let host = parsed_host.as_str();
        let entry = self
            .hosts
            .get(host)
            .ok_or_else(|| ForgeAuthError::Unauthenticated {
                host: host.to_string(),
            })?;
        let kind = self.require_kind(host)?;
        let client_id = entry
            .client_id
            .clone()
            .or_else(|| builtin_client_id(kind, host).map(str::to_string))
            .ok_or_else(|| ForgeAuthError::ClientIdNotConfigured {
                host: host.to_string(),
            })?;
        Ok(match &self.oauth_base {
            Some(base) => ForgeOAuth::with_base_url(kind, client_id, base.clone()),
            None => ForgeOAuth::new(kind, parsed_host, client_id),
        })
    }

    /// Store the personal access token `host` authenticates with in the OS
    /// keychain, under its `(kind, host)` pair.
    ///
    /// # Errors
    /// [`ForgeAuthError::KindNotConfigured`] when the host's kind is
    /// unresolvable, [`ForgeAuthError::InvalidHost`] for a malformed host,
    /// keychain errors.
    pub fn store_pat(&self, host: &str, token: &str) -> Result<(), ForgeAuthError> {
        let host = ForgeHost::new(host)?;
        let kind = self.require_kind(host.as_str())?;
        self.keychain
            .write(&pat_account(kind, host.as_str()), token)
    }

    /// Store `host`'s OAuth credentials in the OS keychain and this
    /// authenticator's cell.
    ///
    /// # Errors
    /// [`ForgeAuthError::KindNotConfigured`] when the host's kind is
    /// unresolvable, [`ForgeAuthError::InvalidHost`] for a malformed host,
    /// keychain and serialization errors.
    pub async fn store_oauth(
        &self,
        host: &str,
        credentials: &OAuthCredentials,
    ) -> Result<(), ForgeAuthError> {
        let host = ForgeHost::new(host)?;
        let kind = self.require_kind(host.as_str())?;
        let json = serde_json::to_string(credentials).map_err(|error| {
            ForgeAuthError::StoredCredentials {
                host: host.as_str().to_string(),
                message: error.to_string(),
            }
        })?;
        self.keychain
            .write(&oauth_account(kind, host.as_str()), &json)?;
        self.cells
            .lock()
            .await
            .insert(host.as_str().to_string(), credentials.clone());
        Ok(())
    }

    /// Delete every credential stored for `host`, PAT and OAuth under both
    /// kinds — a reconfigured kind never leaves a stale credential behind.
    /// Returns whether anything was stored.
    ///
    /// # Errors
    /// [`ForgeAuthError::InvalidHost`] for a malformed host, keychain errors.
    pub async fn delete_credentials(&self, host: &str) -> Result<bool, ForgeAuthError> {
        let host = ForgeHost::new(host)?;
        self.cells.lock().await.remove(host.as_str());
        let mut deleted = false;
        for kind in [ForgeKind::GitHub, ForgeKind::GitLab] {
            deleted |= self.keychain.delete(&pat_account(kind, host.as_str()))?;
            deleted |= self.keychain.delete(&oauth_account(kind, host.as_str()))?;
        }
        Ok(deleted)
    }

    /// The one-shot HTTPS credential git carries for `host`: the token
    /// [`Self::token_for`] resolves, as `Authorization: Basic` material
    /// scoped to `https://<host>/`.
    ///
    /// # Errors
    /// As [`Self::token_for`].
    pub async fn git_https_auth(
        &self,
        host: &str,
    ) -> Result<crate::host_git::HttpsAuth, ForgeAuthError> {
        let parsed = ForgeHost::new(host)?;
        let token = self.token_for(host).await?;
        let kind = self.require_kind(parsed.as_str())?;
        crate::host_git::HttpsAuth::new(parsed.as_str(), kind.git_basic_username(), &token).map_err(
            |error| ForgeAuthError::Protocol {
                host: parsed.as_str().to_string(),
                message: error.to_string(),
            },
        )
    }
}

/// A [`ForgeAuth`] that follows the live `MayConfig`: every clone shares
/// one swap cell, so rebuilding it on a config change reaches every holder
/// — token sources, cloud providers and the OAuth endpoints alike.
#[derive(Debug, Clone)]
pub struct LiveForgeAuth(Arc<arc_swap::ArcSwap<ForgeAuth>>);

impl Default for LiveForgeAuth {
    fn default() -> Self {
        Self::from_config(&BTreeMap::new())
    }
}

impl LiveForgeAuth {
    /// An authenticator reading `config`'s `[forge]` map.
    #[must_use]
    pub fn new(config: &crate::config::MayConfig) -> Self {
        Self(Arc::new(arc_swap::ArcSwap::from_pointee(ForgeAuth::new(
            config,
        ))))
    }

    /// An authenticator reading the `[forge]` map itself.
    #[must_use]
    pub fn from_config(forge: &BTreeMap<String, ForgeHostConfig>) -> Self {
        Self(Arc::new(arc_swap::ArcSwap::from_pointee(
            ForgeAuth::from_config(forge),
        )))
    }

    /// Rebuild the shared authenticator after `config` changed.
    pub fn update(&self, config: &crate::config::MayConfig) {
        self.0.store(Arc::new(ForgeAuth::new(config)));
    }

    /// The authenticator as it is right now.
    #[must_use]
    pub fn get(&self) -> Arc<ForgeAuth> {
        self.0.load_full()
    }

    /// [`ForgeAuth::token_for`] on the live authenticator.
    ///
    /// # Errors
    /// As [`ForgeAuth::token_for`].
    pub async fn token_for(&self, host: &str) -> Result<String, ForgeAuthError> {
        self.get().token_for(host).await
    }

    /// [`ForgeAuth::token_for_account`] on the live authenticator.
    ///
    /// # Errors
    /// As [`ForgeAuth::token_for_account`].
    pub async fn token_for_account(
        &self,
        host: &str,
        account: &str,
    ) -> Result<String, ForgeAuthError> {
        self.get().token_for_account(host, account).await
    }

    /// [`ForgeAuth::oauth_client`] on the live authenticator.
    ///
    /// # Errors
    /// As [`ForgeAuth::oauth_client`].
    pub fn oauth_client(&self, host: &str) -> Result<ForgeOAuth, ForgeAuthError> {
        self.get().oauth_client(host)
    }

    /// [`ForgeAuth::store_pat`] on the live authenticator.
    ///
    /// # Errors
    /// As [`ForgeAuth::store_pat`].
    pub fn store_pat(&self, host: &str, token: &str) -> Result<(), ForgeAuthError> {
        self.get().store_pat(host, token)
    }

    /// [`ForgeAuth::store_oauth`] on the live authenticator.
    ///
    /// # Errors
    /// As [`ForgeAuth::store_oauth`].
    pub async fn store_oauth(
        &self,
        host: &str,
        credentials: &OAuthCredentials,
    ) -> Result<(), ForgeAuthError> {
        self.get().store_oauth(host, credentials).await
    }

    /// [`ForgeAuth::delete_credentials`] on the live authenticator.
    ///
    /// # Errors
    /// As [`ForgeAuth::delete_credentials`].
    pub async fn delete_credentials(&self, host: &str) -> Result<bool, ForgeAuthError> {
        self.get().delete_credentials(host).await
    }

    /// [`ForgeAuth::git_https_auth`] on the live authenticator.
    ///
    /// # Errors
    /// As [`ForgeAuth::git_https_auth`].
    pub async fn git_https_auth(
        &self,
        host: &str,
    ) -> Result<crate::host_git::HttpsAuth, ForgeAuthError> {
        self.get().git_https_auth(host).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ApiReply, serve_api};

    fn entry(
        kind: Option<ForgeKind>,
        mode: ForgeAuthMode,
        client_id: Option<&str>,
    ) -> ForgeHostConfig {
        ForgeHostConfig {
            kind,
            mode,
            client_id: client_id.map(str::to_string),
        }
    }

    fn hosts(entries: &[(&str, ForgeHostConfig)]) -> BTreeMap<String, ForgeHostConfig> {
        entries
            .iter()
            .map(|(host, entry)| ((*host).to_string(), entry.clone()))
            .collect()
    }

    #[tokio::test]
    async fn an_unconfigured_host_is_unauthenticated_with_no_probe() {
        let auth = ForgeAuth::in_memory(BTreeMap::new());
        for host in ["github.com", "gitlab.com", "git.corp.com"] {
            let error = auth.token_for(host).await.unwrap_err();
            assert!(
                matches!(error, ForgeAuthError::Unauthenticated { .. }),
                "{host}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn token_mode_reads_the_keychain_pat() {
        let auth = ForgeAuth::in_memory(hosts(&[(
            "gitlab.corp.com",
            entry(Some(ForgeKind::GitLab), ForgeAuthMode::Token, None),
        )]));

        // Empty store: configured, still unauthenticated.
        let error = auth.token_for("gitlab.corp.com").await.unwrap_err();
        assert!(matches!(error, ForgeAuthError::Unauthenticated { .. }));

        auth.store_pat("gitlab.corp.com", "glpat-token-from-store")
            .unwrap();
        assert_eq!(
            auth.token_for("gitlab.corp.com").await.unwrap(),
            "glpat-token-from-store"
        );

        assert!(auth.delete_credentials("gitlab.corp.com").await.unwrap());
        let error = auth.token_for("gitlab.corp.com").await.unwrap_err();
        assert!(matches!(error, ForgeAuthError::Unauthenticated { .. }));
    }

    #[tokio::test]
    async fn a_self_hosted_entry_without_kind_is_a_typed_error() {
        let auth = ForgeAuth::in_memory(hosts(&[(
            "gitlab.corp.com",
            entry(None, ForgeAuthMode::Token, None),
        )]));
        let error = auth.token_for("gitlab.corp.com").await.unwrap_err();
        assert!(matches!(error, ForgeAuthError::KindNotConfigured { .. }));
    }

    #[test]
    fn cli_mode_targets_the_right_host_cli() {
        let (program, args) = cli_command(ForgeKind::GitHub, "ghe.corp.com", None);
        assert_eq!(program, "gh");
        assert_eq!(args, ["auth", "token", "--hostname", "ghe.corp.com"]);
        let (program, args) = cli_command(ForgeKind::GitLab, "gitlab.corp.com", None);
        assert_eq!(program, "glab");
        assert_eq!(
            args,
            ["config", "get", "token", "--host", "gitlab.corp.com"]
        );
    }

    #[tokio::test]
    async fn github_device_flow_polls_until_authorized() {
        crate::test_support::install_global_executor();
        let polls = Arc::new(std::sync::Mutex::new(0_u32));
        let counted = polls.clone();
        let (base, requests) = serve_api(move |request, _base| {
            if request.path == "/login/device/code" {
                return ApiReply::json(
                    200,
                    serde_json::json!({
                        "device_code": "dc-1",
                        "user_code": "ABCD-1234",
                        "verification_uri": "https://ghe.test/device",
                        "expires_in": 900,
                        "interval": 5,
                    })
                    .to_string(),
                );
            }
            assert_eq!(request.path, "/login/oauth/access_token");
            *counted.lock().unwrap() += 1;
            let poll_number = *counted.lock().unwrap();
            match poll_number {
                1 => ApiReply::json(
                    200,
                    serde_json::json!({"error": "authorization_pending"}).to_string(),
                ),
                2 => ApiReply::json(200, serde_json::json!({"error": "slow_down"}).to_string()),
                _ => ApiReply::json(
                    200,
                    serde_json::json!({
                        "access_token": "gho_access",
                        "refresh_token": "ghr_refresh",
                        "expires_in": 28800,
                        "scope": "repo gist",
                    })
                    .to_string(),
                ),
            }
        })
        .await;

        let oauth = ForgeOAuth::with_base_url(ForgeKind::GitHub, "client".into(), base);
        let authorization = oauth.request_device_code("repo gist").await.unwrap();
        assert_eq!(authorization.device_code, "dc-1");
        assert_eq!(authorization.user_code, "ABCD-1234");
        assert_eq!(authorization.interval, Duration::from_secs(5));

        let poll = oauth
            .poll_device_token(&authorization.device_code, authorization.interval)
            .await
            .unwrap();
        let DevicePoll::Pending { retry_after } = poll else {
            panic!("first poll is pending");
        };
        assert_eq!(retry_after, Duration::from_secs(5));

        let poll = oauth
            .poll_device_token(&authorization.device_code, retry_after)
            .await
            .unwrap();
        let DevicePoll::Pending { retry_after } = poll else {
            panic!("a slow_down still pends");
        };
        assert_eq!(retry_after, Duration::from_secs(10));

        let poll = oauth
            .poll_device_token(&authorization.device_code, retry_after)
            .await
            .unwrap();
        let DevicePoll::Authorized(credentials) = poll else {
            panic!("third poll authorizes");
        };
        assert_eq!(credentials.access_token, "gho_access");
        assert_eq!(credentials.refresh_token.as_deref(), Some("ghr_refresh"));
        assert_eq!(credentials.scopes, ["repo", "gist"]);
        assert!(credentials.expires_at_epoch_secs.is_some());
        assert!(requests.lock().unwrap().iter().all(|r| r.method == "POST"));
    }

    #[tokio::test]
    async fn gitlab_device_flow_denial_and_expiry_are_typed() {
        crate::test_support::install_global_executor();
        let (base, _requests) = serve_api(move |request, _base| {
            if request.path == "/oauth/authorize_device" {
                return ApiReply::json(
                    200,
                    serde_json::json!({
                        "device_code": "dc",
                        "user_code": "0A44L90H",
                        "verification_uri": "https://gl.test/oauth/device",
                        "verification_uri_complete": "https://gl.test/oauth/device?user_code=0A44L90H",
                        "expires_in": 300,
                        "interval": 5,
                    })
                    .to_string(),
                );
            }
            ApiReply::json(400, serde_json::json!({"error": "access_denied"}).to_string())
        })
        .await;

        let oauth = ForgeOAuth::with_base_url(ForgeKind::GitLab, "client".into(), base);
        let authorization = oauth.request_device_code("api").await.unwrap();
        assert_eq!(
            authorization.verification_uri_complete.as_deref(),
            Some("https://gl.test/oauth/device?user_code=0A44L90H")
        );
        let error = oauth
            .poll_device_token(&authorization.device_code, authorization.interval)
            .await
            .unwrap_err();
        assert!(matches!(error, ForgeAuthError::Denied { .. }), "{error}");
    }

    #[tokio::test]
    async fn refresh_rotates_and_keeps_the_pair() {
        crate::test_support::install_global_executor();
        let (base, requests) = serve_api(move |request, _base| {
            assert_eq!(request.path, "/oauth/token");
            ApiReply::json(
                200,
                serde_json::json!({
                    "access_token": "refreshed-access",
                    "refresh_token": "rotated-refresh",
                    "expires_in": 7200,
                    "scope": "api",
                })
                .to_string(),
            )
        })
        .await;

        let oauth = ForgeOAuth::with_base_url(ForgeKind::GitLab, "client".into(), base);
        let credentials = oauth.refresh("old-refresh").await.unwrap();
        assert_eq!(credentials.access_token, "refreshed-access");
        assert_eq!(
            credentials.refresh_token.as_deref(),
            Some("rotated-refresh")
        );
        let body = &requests.lock().unwrap()[0].body;
        assert!(body.contains("grant_type=refresh_token"), "{body}");
        assert!(body.contains("refresh_token=old-refresh"), "{body}");
        assert!(body.contains("client_id=client"), "{body}");
    }

    #[tokio::test]
    async fn token_for_refreshes_an_expiring_oauth_credential() {
        crate::test_support::install_global_executor();
        let (base, requests) = serve_api(move |request, _base| {
            assert_eq!(request.path, "/oauth/token");
            ApiReply::json(
                200,
                serde_json::json!({
                    "access_token": "rotated-access",
                    "expires_in": 7200,
                })
                .to_string(),
            )
        })
        .await;

        let auth = ForgeAuth::in_memory(hosts(&[(
            "gitlab.com",
            entry(None, ForgeAuthMode::Oauth, Some("client")),
        )]))
        .with_oauth_base(base);
        let expired = OAuthCredentials {
            access_token: "stale-access".to_string(),
            refresh_token: Some("keep-me-refresh".to_string()),
            expires_at_epoch_secs: Some(epoch_now() - 10),
            scopes: vec!["api".to_string()],
        };
        auth.store_oauth("gitlab.com", &expired).await.unwrap();

        let token = auth.token_for("gitlab.com").await.unwrap();
        assert_eq!(token, "rotated-access");
        // The refresh the reply omitted is kept; the rotation persisted.
        let stored: OAuthCredentials = serde_json::from_str(
            &auth
                .keychain
                .read(&oauth_account(ForgeKind::GitLab, "gitlab.com"))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stored.refresh_token.as_deref(), Some("keep-me-refresh"));
        assert_eq!(requests.lock().unwrap().len(), 1);

        // A second read is served from the rotated credential, no request.
        assert_eq!(
            auth.token_for("gitlab.com").await.unwrap(),
            "rotated-access"
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn oauth_mode_without_client_id_fails_typed() {
        let auth = ForgeAuth::in_memory(hosts(&[(
            "gitlab.com",
            entry(None, ForgeAuthMode::Oauth, None),
        )]));
        let expired = OAuthCredentials {
            access_token: "stale".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at_epoch_secs: Some(epoch_now() - 10),
            scopes: Vec::new(),
        };
        auth.store_oauth("gitlab.com", &expired).await.unwrap();
        let error = auth.token_for("gitlab.com").await.unwrap_err();
        assert!(
            matches!(error, ForgeAuthError::ClientIdNotConfigured { .. }),
            "{error}"
        );
    }

    #[tokio::test]
    async fn oauth_mode_valid_token_needs_no_client_id() {
        let auth = ForgeAuth::in_memory(hosts(&[(
            "github.com",
            entry(None, ForgeAuthMode::Oauth, None),
        )]));
        let valid = OAuthCredentials {
            access_token: "gho_valid".to_string(),
            refresh_token: None,
            expires_at_epoch_secs: None,
            scopes: vec!["repo".to_string()],
        };
        auth.store_oauth("github.com", &valid).await.unwrap();
        assert_eq!(auth.token_for("github.com").await.unwrap(), "gho_valid");
    }

    #[test]
    fn pkce_challenges_the_verifier() {
        use base64::Engine as _;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use sha2::{Digest, Sha256};

        let pkce = Pkce::generate();
        assert_eq!(pkce.verifier().len(), 43);
        assert_eq!(
            pkce.challenge(),
            URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier().as_bytes()))
        );
    }

    #[tokio::test]
    async fn gitlab_pkce_authorize_and_exchange() {
        crate::test_support::install_global_executor();
        let (base, _requests) = serve_api(move |request, _base| {
            assert_eq!(request.path, "/oauth/token");
            assert!(request.body.contains("grant_type=authorization_code"));
            assert!(request.body.contains("code_verifier="));
            ApiReply::json(
                200,
                serde_json::json!({
                    "access_token": "pkce-access",
                    "refresh_token": "pkce-refresh",
                    "expires_in": 7200,
                    "scope": "api",
                })
                .to_string(),
            )
        })
        .await;

        let oauth = ForgeOAuth::with_base_url(ForgeKind::GitLab, "client".into(), base.clone());
        let pkce = Pkce::generate();
        let url = oauth
            .authorization_url("http://localhost:4545/callback", "api", "state-1", &pkce)
            .unwrap();
        assert!(
            url.starts_with(&format!("{base}/oauth/authorize?")),
            "{url}"
        );
        for part in [
            "response_type=code",
            "client_id=client",
            "code_challenge_method=S256",
            "state=state-1",
        ] {
            assert!(url.contains(part), "{url} lacks {part}");
        }

        let credentials = oauth
            .exchange_authorization_code("auth-code", &pkce, "http://localhost:4545/callback")
            .await
            .unwrap();
        assert_eq!(credentials.access_token, "pkce-access");
        assert_eq!(credentials.refresh_token.as_deref(), Some("pkce-refresh"));

        // PKCE is GitLab-only here.
        let github = ForgeOAuth::with_base_url(ForgeKind::GitHub, "client".into(), base);
        let error = github
            .authorization_url("http://localhost/cb", "repo", "s", &pkce)
            .unwrap_err();
        assert!(
            matches!(error, ForgeAuthError::Unsupported { .. }),
            "{error}"
        );
    }

    #[tokio::test]
    async fn git_https_auth_wraps_the_resolved_token() {
        let auth = ForgeAuth::in_memory(hosts(&[(
            "gitlab.com",
            entry(None, ForgeAuthMode::Token, None),
        )]));
        auth.store_pat("gitlab.com", "glpat-for-git").unwrap();
        let credential = auth.git_https_auth("gitlab.com").await.unwrap();
        assert_eq!(credential.origin(), "https://gitlab.com/");
    }
}
