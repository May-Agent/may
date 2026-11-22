//! The forge-authentication API: every `[forge."<host>"]` entry's status,
//! and the endpoints a client configures, disconnects and logs a host in
//! through — token, CLI or the OAuth device flow.
//!
//! Every write lands in `config.toml` through the daemon's live config so
//! the shared [`may_config::forge::LiveForgeAuth`] swaps with it; credentials
//! themselves live only in the OS keychain.

use may_config::forge::ForgeAuthMode;

/// How the API answers about one forge host.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct ForgeHostStatus {
    /// The normalized hostname.
    pub host: String,
    /// The forge kind the host runs — implied for `github.com` and
    /// `gitlab.com`, configured otherwise; `None` only while a custom host
    /// still lacks a kind.
    pub kind: Option<String>,
    /// Whether `[forge."<host>"]` exists in `config.toml`.
    pub configured: bool,
    /// The entry's authentication mode, when configured.
    pub mode: Option<ForgeAuthMode>,
    /// The OAuth `client_id`, when the entry sets one.
    pub client_id: Option<String>,
    /// Whether a credential currently resolves for the host.
    pub authenticated: bool,
}

/// The `GET /api/forge-auth` body.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct ForgeAuthStatusResponse {
    /// `github.com`, `gitlab.com`, and every extra host in `[forge]`.
    pub hosts: Vec<ForgeHostStatus>,
}

/// `POST /api/forge-auth/configure`'s body.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct ConfigureForgeAuthRequest {
    /// The host to configure, `host[:port]`.
    pub host: String,
    /// The forge software the host runs, `"github"` or `"gitlab"`;
    /// required for hosts that are neither `github.com` nor `gitlab.com`.
    #[serde(default)]
    pub kind: Option<String>,
    /// How May authenticates: `oauth`, `token` or `cli`.
    pub mode: ForgeAuthMode,
    /// The OAuth application `client_id`, kept for `mode = "oauth"`.
    #[serde(default)]
    pub client_id: Option<String>,
    /// The personal access token to store in the keychain, for
    /// `mode = "token"`.
    #[serde(default)]
    pub token: Option<String>,
}

/// `POST /api/forge-auth/logout`'s body.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct ForgeLogoutRequest {
    /// The host to disconnect.
    pub host: String,
}

/// `POST /api/forge-auth/oauth/start`'s body.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct ForgeOauthStartRequest {
    /// The host to authorize on, `host[:port]`.
    pub host: String,
    /// The forge software the host runs, `"github"` or `"gitlab"`;
    /// required for hosts that are neither `github.com` nor `gitlab.com`.
    #[serde(default)]
    pub kind: Option<String>,
    /// The OAuth application `client_id`; overrides a stored one. May's own
    /// registration applies to `github.com` when absent.
    #[serde(default)]
    pub client_id: Option<String>,
}

/// The device authorization `POST /api/forge-auth/oauth/start` returns.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct ForgeDeviceAuthorization {
    /// Opaque code `oauth/poll` posts back.
    pub device_code: String,
    /// Short code the user types at `verification_uri`.
    pub user_code: String,
    /// Verification page URL.
    pub verification_uri: String,
    /// Verification URL carrying the code, when the forge sends one.
    pub verification_uri_complete: Option<String>,
    /// Seconds to wait between polls.
    pub interval_secs: u64,
    /// Seconds the device code stays valid.
    pub expires_in_secs: u64,
}

/// `POST /api/forge-auth/oauth/poll`'s body.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct ForgeOauthPollRequest {
    /// The host the device flow runs against.
    pub host: String,
    /// The `device_code` `oauth/start` returned.
    pub device_code: String,
}

/// How `oauth/poll` answered.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ForgeOauthPollStatus {
    /// The user authorized; the credential is stored.
    Authorized,
    /// Still waiting on the user.
    Pending,
    /// The server asked to poll less often.
    SlowDown,
}

/// `POST /api/forge-auth/oauth/poll`'s body.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct ForgeOauthPollResponse {
    /// `authorized`, `pending` or `slow_down`.
    pub status: ForgeOauthPollStatus,
    /// Seconds until the next poll.
    pub retry_after_secs: u64,
}
