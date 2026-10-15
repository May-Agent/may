//! The pinned baseline tool distribution every May execution backend prepares.

use aither_sandbox::{ToolDistribution, ToolRuntimeError};

/// Release asset `tool-distribution-0.7.0.json` of aither v0.7.0, byte for byte.
const EMBEDDED_DISTRIBUTION: &[u8] = include_bytes!("tool-distribution-0.7.0.json");
/// SHA-256 of the aither v0.7.0 `tool-distribution-0.7.0.json` release asset.
#[cfg(test)]
const EMBEDDED_DISTRIBUTION_SHA256: &str =
    "fc8670fa02e7619b6b099a46e8889a5533b3ea23c0fb5e281b25da1f07c28ac8";

/// Loads the embedded, hash-pinned baseline distribution.
///
/// Artifacts are downloaded on first use and verified against the manifest's
/// SHA-256 digests before anything executes them.
///
/// # Errors
/// Fails when the embedded manifest does not validate.
pub fn embedded_distribution() -> Result<ToolDistribution, ToolRuntimeError> {
    ToolDistribution::from_json(EMBEDDED_DISTRIBUTION)
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::{EMBEDDED_DISTRIBUTION, EMBEDDED_DISTRIBUTION_SHA256, embedded_distribution};

    #[test]
    fn embedded_distribution_matches_the_published_release_asset() {
        assert_eq!(
            format!("{:x}", Sha256::digest(EMBEDDED_DISTRIBUTION)),
            EMBEDDED_DISTRIBUTION_SHA256
        );
    }

    #[test]
    fn embedded_distribution_validates() {
        embedded_distribution().expect("embedded distribution should validate");
    }
}
