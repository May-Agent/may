//! The pinned baseline tool distribution every May execution backend prepares.

use aither_sandbox::{ToolDistribution, ToolRuntimeError};

#[cfg(test)]
mod fixture;
#[cfg(test)]
pub(crate) use fixture::fixture_distribution;

/// Release asset `tool-distribution-0.24.0.json` of aither's `patch-v0.24.0`
/// release, byte for byte.
const EMBEDDED_DISTRIBUTION: &[u8] = include_bytes!("tool-distribution-0.24.0.json");
/// SHA-256 of the `patch-v0.24.0` `tool-distribution-0.24.0.json` release asset.
#[cfg(test)]
const EMBEDDED_DISTRIBUTION_SHA256: &str =
    "965d2f13f3098268d34e251dfd04ca3ab5af7a79c55fcc81a0b07e8155a026ef";

/// Loads the embedded, hash-pinned baseline distribution.
///
/// Artifacts are downloaded on first use and verified against the manifest's
/// SHA-256 digests before anything executes them.
///
/// # Errors
/// Fails when the embedded manifest does not validate.
pub fn embedded_distribution() -> Result<ToolDistribution, ToolRuntimeError> {
    ToolDistribution::from_json(EMBEDDED_DISTRIBUTION).map(authenticated)
}

/// Downloads GitHub release artifacts with the `GITHUB_TOKEN` environment
/// variable when it is set and non-empty (CI sets it to the job's token),
/// the same source the managed heel download reads.
fn authenticated(distribution: ToolDistribution) -> ToolDistribution {
    match std::env::var("GITHUB_TOKEN") {
        Ok(token) if !token.is_empty() => distribution.with_github_token(token),
        _ => distribution,
    }
}

/// May's own host-program pins: the repo-search daemon release whose
/// version matches the `repo-search-client` crate May links, since the
/// daemon refuses a handshake from any other client version.
const HOST_PROGRAMS: &[u8] = include_bytes!("host-programs.json");
/// SHA-256 of `host-programs.json`, so an accidental edit fails a test.
#[cfg(test)]
const HOST_PROGRAMS_SHA256: &str =
    "a15d16b7e1076be211f0ec8c71f7f1ea3ad8c02f54b399b67eb4575df0a302df";

/// Loads May's hash-pinned host-program manifest.
///
/// # Errors
/// Fails when the embedded manifest does not validate.
pub fn host_program_distribution() -> Result<ToolDistribution, ToolRuntimeError> {
    ToolDistribution::from_json(HOST_PROGRAMS).map(authenticated)
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
