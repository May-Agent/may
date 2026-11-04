//! The pinned baseline tool distribution every May execution backend prepares.

use aither_sandbox::{ToolDistribution, ToolRuntimeError};

#[cfg(test)]
mod fixture;
#[cfg(test)]
pub(crate) use fixture::fixture_distribution;

/// Release asset `tool-distribution-0.14.1.json` of aither's `patch-v0.14.1`
/// release, byte for byte.
const EMBEDDED_DISTRIBUTION: &[u8] = include_bytes!("tool-distribution-0.14.1.json");
/// SHA-256 of the `patch-v0.14.1` `tool-distribution-0.14.1.json` release asset.
#[cfg(test)]
const EMBEDDED_DISTRIBUTION_SHA256: &str =
    "1770f8295eedbee58b6be325c82d9f883afcbbd264de75849a7e1c10a1f12ad2";

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

/// May's own host-program pins: the repo-search daemon release whose
/// version matches the `repo-search-client` crate May links, since the
/// daemon refuses a handshake from any other client version.
const HOST_PROGRAMS: &[u8] = include_bytes!("host-programs.json");
/// SHA-256 of `host-programs.json`, so an accidental edit fails a test.
#[cfg(test)]
const HOST_PROGRAMS_SHA256: &str =
    "62a1ad2a2a716a3e2ba88060ba77651c6d6a92984eea3a7b68e00b9a5209ada9";

/// Loads May's hash-pinned host-program manifest.
///
/// # Errors
/// Fails when the embedded manifest does not validate.
pub fn host_program_distribution() -> Result<ToolDistribution, ToolRuntimeError> {
    ToolDistribution::from_json(HOST_PROGRAMS)
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::{
        EMBEDDED_DISTRIBUTION, EMBEDDED_DISTRIBUTION_SHA256, HOST_PROGRAMS, HOST_PROGRAMS_SHA256,
        embedded_distribution, host_program_distribution,
    };

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

    #[test]
    fn host_programs_match_their_pin() {
        assert_eq!(
            format!("{:x}", Sha256::digest(HOST_PROGRAMS)),
            HOST_PROGRAMS_SHA256
        );
        host_program_distribution().expect("host-program manifest should validate");
    }
}
