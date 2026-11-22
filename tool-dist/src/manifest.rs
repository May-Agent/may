//! The artifact manifest every consumer of the tool distribution shares.
//!
//! `build.rs` includes this module to hash the manifest into the container
//! image references, so it stays `std` + `serde_json`.

/// The third-party artifacts (`ripgrep`, `ast_grep`, `jq`, `coreutils`,
/// `xargs`, `fd`, `uv`) and the managed Python interpreters, each pinned to its
/// upstream release.
pub const THIRD_PARTY_TOOLS: &[u8] = include_bytes!("third-party-tools.json");

/// The artifacts May builds and releases itself (`bash` and `apply_patch` from
/// `patch/v<version>`, `aither-sed` from `sed/v<version>`).
pub const MAY_ARTIFACTS: &[u8] = include_bytes!("tool-artifacts.json");

/// May's own host-program pins.
///
/// `heel` at the version of the `heel` library
/// May links, which refuses a binary of any other version, the repo-search
/// daemon release whose version matches the `repo-search-client` crate May
/// links, since the daemon refuses a handshake from any other client version,
/// and the managed container runtime's `podman` client.
pub const HOST_PROGRAMS: &[u8] = include_bytes!("host-programs.json");

pub use may_container_image::{combined_manifest, combined_manifest_bytes};
