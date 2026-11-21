//! The container image references CI needs before anything compiles.
//!
//! `may-agent`'s build script bakes `MAY_BASE_IMAGE` and
//! `MAY_WORKSPACE_IMAGE` into the binary through this crate, the
//! `may-container-image` binary prints the same references for the
//! container-image workflow, and `may-agent` re-exports `image_ref` for its
//! own use, so the reference logic lives in exactly one place and the
//! workflow computes it in seconds instead of compiling the whole crate.

pub mod image_ref;

/// The pinned host programs: `heel`, repo-search and the managed container
/// runtime's `podman` client, in the pin file the distribution reads.
const HOST_PROGRAMS: &[u8] = include_bytes!("../../src/tools/host-programs.json");

/// May's own pinned tool artifacts: `apply_patch`, `aither-sed` and bash.
const MAY_ARTIFACTS: &[u8] = include_bytes!("../../src/tools/tool-artifacts.json");

/// The pinned third-party tool artifacts and Python interpreters.
const THIRD_PARTY_TOOLS: &[u8] = include_bytes!("../../src/tools/third-party-tools.json");

/// The manifest assembled from May's pin files alone: the third-party
/// artifacts and Python pins, then May's own artifacts, and the host
/// programs.
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

/// [`combined_manifest`] as bytes: what the container image reference
/// hashes, so the tag changes with any pin file.
///
/// # Errors
/// Fails when a pin file is not JSON of the expected shape.
pub fn combined_manifest_bytes() -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&combined_manifest()?)
}

/// Extract the `version` field of the `heel` package entry in `Cargo.lock`.
#[must_use]
pub fn heel_version_in_lockfile(lockfile: &str) -> Option<String> {
    let mut in_heel_package = false;
    for line in lockfile.lines() {
        if line.starts_with("[[") {
            in_heel_package = false;
        }
        if line == "name = \"heel\"" {
            in_heel_package = true;
        } else if in_heel_package
            && let Some(version) = line
                .strip_prefix("version = \"")
                .and_then(|rest| rest.strip_suffix('"'))
        {
            return Some(version.to_string());
        }
    }
    None
}
