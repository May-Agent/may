//! May's configuration layer: `~/.may/config.toml` and everything it
//! touches.
//!
//! The [`config`] module holds `MayConfig` and the file's schema; around it
//! sit the layers only configuration reaches for — the keychain-backed
//! [`secrets`] store and its env injection ([`bash`]), the machine-wide
//! [`jobserver`] pool, the forge coordinates and credentials ([`forge`]),
//! host git and commit signing ([`host_git`], [`commit_signing`]), the
//! OAuth flows ([`openrouter`]), the credential file store
//! ([`auth_store`]), the shared filesystem helpers ([`fs_utils`],
//! [`pasted_text`]) and the pure setting types the config names:
//! [`language`], [`host_load`], [`update`] and [`subagent_file`].

/// Persistent storage for OAuth credentials.
pub mod auth_store;
/// The managed bash's outcome rendering and secret injection.
pub mod bash;
/// Signing of the commits agents make in their sandbox.
pub mod commit_signing;
/// Configuration loading and types.
pub mod config;
/// Forge coordinates and per-host authentication (GitHub, GitLab).
pub mod forge;
/// Shared filesystem helpers.
pub mod fs_utils;
/// The one way git runs on the host.
pub mod host_git;
/// The machine load a session monitors, and when it is called high.
pub mod host_load;
/// The machine-wide jobserver token pool shared by every terminal.
pub mod jobserver;
/// Communication and working languages.
pub mod language;
/// `OpenRouter` OAuth login flow.
pub mod openrouter;
/// Large pasted text rendered for the model.
pub mod pasted_text;
/// Keychain access, the names index and secret redaction.
pub mod secrets;
/// Subagent definitions and the names that resolve them.
pub mod subagent_file;
/// Update channel and build kind.
pub mod update;

/// Test support shared by this crate's `#[cfg(test)]` code.
#[cfg(test)]
mod test_support;
