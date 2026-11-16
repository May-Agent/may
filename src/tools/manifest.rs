//! The artifact manifest every consumer of the tool distribution shares.
//!
//! `build.rs` includes this module to hash the manifest into the container
//! image references, so it stays `std` + `serde_json`.

/// The third-party artifacts (`ripgrep`, `ast_grep`, `jq`, `coreutils`,
/// `xargs`, `fd`, `uv`) and the managed Python interpreters, each pinned to its
/// upstream release.
pub const THIRD_PARTY_TOOLS: &[u8] = include_bytes!("third-party-tools.json");

/// The artifacts May builds and releases itself (`bash` from
/// `patch/v<version>`, or aither's `patch-v0.24.0` until a May release carries
/// it, `apply_patch` from `patch/v<version>`, `aither-sed` from
/// `sed/v<version>`).
pub const MAY_ARTIFACTS: &[u8] = include_bytes!("tool-artifacts.json");

/// May's own host-program pins.
///
/// `heel` at the version of the `heel` library
/// May links, which refuses a binary of any other version, the repo-search
/// daemon release whose version matches the `repo-search-client` crate May
/// links, since the daemon refuses a handshake from any other client version,
/// and the managed container runtime's `podman` client.
pub const HOST_PROGRAMS: &[u8] = include_bytes!("host-programs.json");

/// The manifest assembled from May's pin files alone: the third-party
/// artifacts and Python pins, then May's own artifacts, and the host programs.
///
/// # Errors
/// Fails when a pin file is not JSON of the expected shape.
pub fn combined_manifest() -> serde_json::Result<serde_json::Value> {
    let mut manifest: serde_json::Value = serde_json::from_slice(THIRD_PARTY_TOOLS)?;
    let mut own: serde_json::Value = serde_json::from_slice(MAY_ARTIFACTS)?;
    let mut own: Vec<serde_json::Value> = serde_json::from_value(own["artifacts"].take())?;
    let mut host: serde_json::Value = serde_json::from_slice(HOST_PROGRAMS)?;
    let mut artifacts: Vec<serde_json::Value> =
        serde_json::from_value(manifest["artifacts"].take())?;
    artifacts.append(&mut own);
    manifest["artifacts"] = artifacts.into();
    manifest["host_programs"] = host["host_programs"].take();
    Ok(manifest)
}

/// [`combined_manifest`] as bytes: what the container image reference hashes,
/// so the tag changes with any pin file.
///
/// # Errors
/// Fails when a pin file is not JSON of the expected shape.
pub fn combined_manifest_bytes() -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&combined_manifest()?)
}
