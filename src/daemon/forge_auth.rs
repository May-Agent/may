//! The forge-authentication API: every `[forge."<host>"]` entry's status,
//! and the endpoints a client configures, disconnects and logs a host in
//! through — token, CLI or the OAuth device flow.
//!
//! Every write lands in `config.toml` through the daemon's live config so
//! the shared [`crate::forge::LiveForgeAuth`] swaps with it; credentials
//! themselves live only in the OS keychain.

use std::sync::Arc;

use skyzen::Result as SkyResult;
use skyzen::utils::{Json, State};

use super::error::{bad_request_error, internal_error, json_ok_response};
use super::state::DaemonState;
use crate::forge::{DevicePoll, ForgeAuthMode, ForgeHost, ForgeKind};

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

/// `kind` from the request's string, or the kind `host`'s name implies.
fn forge_kind(kind: Option<&str>, host: &str) -> SkyResult<Option<ForgeKind>> {
    match kind {
        None => Ok(ForgeKind::of_host(host)),
        Some("github") => Ok(Some(ForgeKind::GitHub)),
        Some("gitlab") => Ok(Some(ForgeKind::GitLab)),
        Some(other) => Err(bad_request_error(format!(
            "unknown forge kind {other:?}; expected \"github\" or \"gitlab\""
        ))),
    }
}

fn status_of(
    config: &crate::config::MayConfig,
    host: &str,
    authenticated: bool,
) -> ForgeHostStatus {
    let entry = config.forge_config(host);
    ForgeHostStatus {
        host: host.to_string(),
        kind: config.forge_kind(host).map(|kind| match kind {
            ForgeKind::GitHub => "github".to_string(),
            ForgeKind::GitLab => "gitlab".to_string(),
        }),
        configured: entry.is_some(),
        mode: entry.map(|entry| entry.mode),
        client_id: entry.and_then(|entry| entry.client_id.clone()),
        authenticated,
    }
}

/// `GET /api/forge-auth`: the status of `github.com`, `gitlab.com` and
/// every extra host `[forge]` names.
pub(super) async fn list_forge_auth(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<skyzen::Response> {
    let config = state.config_snapshot();
    let mut hosts: Vec<String> = vec![
        crate::forge::GITHUB_HOST.to_string(),
        crate::forge::GITLAB_HOST.to_string(),
    ];
    for host in config.forge.keys() {
        if !hosts.contains(host) {
            hosts.push(host.clone());
        }
    }
    let mut statuses = Vec::with_capacity(hosts.len());
    for host in hosts {
        let authenticated = state.forge_auth.token_for(&host).await.is_ok();
        statuses.push(status_of(&config, &host, authenticated));
    }
    json_ok_response(&ForgeAuthStatusResponse { hosts: statuses })
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

/// `POST /api/forge-auth/configure`: write `[forge."<host>"]`, store the
/// token and refresh the live authenticator.
pub(super) async fn configure_forge_auth(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<ConfigureForgeAuthRequest>,
) -> SkyResult<skyzen::Response> {
    let host = ForgeHost::new(req.host.trim()).map_err(bad_request_error)?;
    let host = host.as_str().to_string();
    let kind = forge_kind(req.kind.as_deref(), &host)?;
    if kind.is_none() {
        return Err(bad_request_error(
            "`kind` is required for a host that is neither github.com nor gitlab.com".to_string(),
        ));
    }
    if req.mode != ForgeAuthMode::Token && req.token.is_some() {
        return Err(bad_request_error(
            "`token` only applies to mode \"token\"".to_string(),
        ));
    }
    let client_id = req.client_id.filter(|id| !id.trim().is_empty());
    let updated = state.update_config(|config| {
        config.forge.insert(
            host.clone(),
            crate::forge::ForgeHostConfig {
                kind,
                mode: req.mode,
                client_id: client_id.clone(),
            },
        );
    });
    state.save_config(&updated).await.map_err(internal_error)?;
    if let Some(token) = &req.token {
        state
            .forge_auth
            .store_pat(&host, token)
            .map_err(bad_request_error)?;
    }
    let authenticated = state.forge_auth.token_for(&host).await.is_ok();
    json_ok_response(&status_of(&updated, &host, authenticated))
}

/// `POST /api/forge-auth/logout`'s body.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct ForgeLogoutRequest {
    /// The host to disconnect.
    pub host: String,
}

/// `POST /api/forge-auth/logout`: drop the host's keychain credentials and
/// its `[forge]` entry.
pub(super) async fn forge_logout(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<ForgeLogoutRequest>,
) -> SkyResult<skyzen::Response> {
    let host = ForgeHost::new(req.host.trim()).map_err(bad_request_error)?;
    let host = host.as_str().to_string();
    state
        .forge_auth
        .delete_credentials(&host)
        .await
        .map_err(internal_error)?;
    state
        .forge_device_flows
        .lock()
        .expect("device flows")
        .retain(|(flow_host, _), _| flow_host != &host);
    let updated = state.update_config(|config| {
        config.forge.remove(&host);
    });
    state.save_config(&updated).await.map_err(internal_error)?;
    json_ok_response(&super::types::API_OK)
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

/// The OAuth scope a GitHub device authorization asks for.
const GITHUB_SCOPES: &str = "repo read:org workflow";
/// The OAuth scope a GitLab device authorization asks for.
const GITLAB_SCOPES: &str = "api read_user write_repository";

/// `POST /api/forge-auth/oauth/start`: persist the host's OAuth entry and
/// begin the device flow.
pub(super) async fn forge_oauth_start(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<ForgeOauthStartRequest>,
) -> SkyResult<skyzen::Response> {
    let host = ForgeHost::new(req.host.trim()).map_err(bad_request_error)?;
    let host = host.as_str().to_string();
    let kind = forge_kind(req.kind.as_deref(), &host)?.ok_or_else(|| {
        bad_request_error(
            "`kind` is required for a host that is neither github.com nor gitlab.com".to_string(),
        )
    })?;
    let client_id = req.client_id.filter(|id| !id.trim().is_empty());
    let updated = state.update_config(|config| {
        let entry =
            config
                .forge
                .entry(host.clone())
                .or_insert_with(|| crate::forge::ForgeHostConfig {
                    kind: Some(kind),
                    mode: ForgeAuthMode::Oauth,
                    client_id: None,
                });
        entry.kind = Some(kind);
        entry.mode = ForgeAuthMode::Oauth;
        if let Some(id) = &client_id {
            entry.client_id = Some(id.clone());
        }
    });
    state.save_config(&updated).await.map_err(internal_error)?;
    let authorization = state
        .forge_auth
        .oauth_client(&host)
        .map_err(bad_request_error)?
        .request_device_code(match kind {
            ForgeKind::GitHub => GITHUB_SCOPES,
            ForgeKind::GitLab => GITLAB_SCOPES,
        })
        .await
        .map_err(bad_request_error)?;
    let response = ForgeDeviceAuthorization {
        device_code: authorization.device_code.clone(),
        user_code: authorization.user_code.clone(),
        verification_uri: authorization.verification_uri.clone(),
        verification_uri_complete: authorization.verification_uri_complete.clone(),
        interval_secs: authorization.interval.as_secs(),
        expires_in_secs: authorization.expires_in.as_secs(),
    };
    state
        .forge_device_flows
        .lock()
        .expect("device flows")
        .insert((host, authorization.device_code.clone()), authorization);
    json_ok_response(&response)
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

/// `POST /api/forge-auth/oauth/poll`: poll the device flow once; on
/// `authorized` the credential is stored and `mode = "oauth"` is already
/// persisted.
pub(super) async fn forge_oauth_poll(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<ForgeOauthPollRequest>,
) -> SkyResult<skyzen::Response> {
    let host = ForgeHost::new(req.host.trim()).map_err(bad_request_error)?;
    let host = host.as_str().to_string();
    let key = (host.clone(), req.device_code.clone());
    let authorization = state
        .forge_device_flows
        .lock()
        .expect("device flows")
        .get(&key)
        .cloned()
        .ok_or_else(|| {
            bad_request_error("no device flow runs for that host and code".to_string())
        })?;
    let client = state
        .forge_auth
        .oauth_client(&host)
        .map_err(bad_request_error)?;
    match client
        .poll_device_token(&req.device_code, authorization.interval)
        .await
        .map_err(bad_request_error)?
    {
        DevicePoll::Pending { retry_after } => json_ok_response(&ForgeOauthPollResponse {
            status: if retry_after > authorization.interval {
                ForgeOauthPollStatus::SlowDown
            } else {
                ForgeOauthPollStatus::Pending
            },
            retry_after_secs: retry_after.as_secs(),
        }),
        DevicePoll::Authorized(credentials) => {
            state
                .forge_auth
                .store_oauth(&host, &credentials)
                .await
                .map_err(internal_error)?;
            state
                .forge_device_flows
                .lock()
                .expect("device flows")
                .remove(&key);
            json_ok_response(&ForgeOauthPollResponse {
                status: ForgeOauthPollStatus::Authorized,
                retry_after_secs: 0,
            })
        }
    }
}
