//! The artifact manifest every consumer of the tool distribution shares.
//!
//! `build.rs` includes this module to hash the manifest into the container
//! image references, so it stays `std` + `serde_json`.

/// Release asset `tool-distribution-0.24.0.json` of aither's `patch-v0.24.0`
/// release, byte for byte.
pub const BASELINE: &[u8] = include_bytes!("tool-distribution-0.24.0.json");

/// The artifacts May builds and releases itself (`apply_patch` from
/// `patch/v<version>`, `aither-sed` from `sed/v<version>`).
pub const MAY_ARTIFACTS: &[u8] = include_bytes!("tool-artifacts.json");

/// The baseline manifest with every artifact of a tool [`MAY_ARTIFACTS`]
/// supplies replaced by May's, so no artifact of those tools points at the
/// baseline's release.
///
/// # Errors
/// Fails when either manifest is not JSON of the expected shape.
pub fn combined_manifest() -> serde_json::Result<serde_json::Value> {
    let mut manifest: serde_json::Value = serde_json::from_slice(BASELINE)?;
    let mut own: serde_json::Value = serde_json::from_slice(MAY_ARTIFACTS)?;
    let mut own: Vec<serde_json::Value> = serde_json::from_value(own["artifacts"].take())?;
    let mut artifacts: Vec<serde_json::Value> =
        serde_json::from_value(manifest["artifacts"].take())?;
    artifacts.retain(|artifact| !own.iter().any(|mine| mine["tool"] == artifact["tool"]));
    artifacts.append(&mut own);
    manifest["artifacts"] = artifacts.into();
    Ok(manifest)
}

/// [`combined_manifest`] as bytes: what the container image reference hashes,
/// so the tag changes with either manifest.
///
/// # Errors
/// Fails when either manifest is not JSON of the expected shape.
pub fn combined_manifest_bytes() -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&combined_manifest()?)
}
