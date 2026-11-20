//! Forge hosts, the repositories on them and their authentication.
//!
//! [`ForgeRepo`] is the forge-neutral identity of one repository — its
//! canonical `github:`/`gitlab:` binding is what sessions and
//! `workspace.toml` store. [`ForgeAuth`] resolves the bearer token a host
//! authenticates with, per its `[forge."<host>"]` entry: `oauth` through
//! May's own keychain-stored login, `token` through a keychain-stored
//! personal access token, `cli` through the host CLI's login — and nothing
//! for a host without an entry.

mod auth;
mod coord;

pub use auth::{
    DeviceAuthorization, DevicePoll, ForgeAuth, ForgeAuthError, ForgeAuthMode, ForgeHostConfig,
    ForgeOAuth, LiveForgeAuth, OAuthCredentials, Pkce,
};
pub(crate) use coord::looks_like_host;

pub use coord::{
    ForgeHost, ForgeHostError, ForgeKind, ForgeRepo, ForgeRepoError, GITHUB_HOST, GITLAB_HOST,
    url_path_encode,
};
