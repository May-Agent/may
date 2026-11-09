//! The pinned tool distribution every May execution backend prepares.

use std::path::Path;

use aither_sandbox::{HostProgram, ToolDistribution, ToolRuntimeError, ToolTarget};
use heel::HeelBinary;

#[cfg(test)]
mod fixture;
#[cfg(test)]
pub(crate) use fixture::{fixture_distribution, test_heel, test_tool_cache};

/// Release asset `tool-distribution-0.24.0.json` of aither's `patch-v0.24.0`
/// release, byte for byte.
const EMBEDDED_DISTRIBUTION: &[u8] = include_bytes!("tool-distribution-0.24.0.json");
/// SHA-256 of the `patch-v0.24.0` `tool-distribution-0.24.0.json` release asset.
#[cfg(test)]
const EMBEDDED_DISTRIBUTION_SHA256: &str =
    "965d2f13f3098268d34e251dfd04ca3ab5af7a79c55fcc81a0b07e8155a026ef";

/// May's own host-program pins: `heel` at the version of the `heel` library
/// May links, which refuses a binary of any other version, and the
/// repo-search daemon release whose version matches the `repo-search-client`
/// crate May links, since the daemon refuses a handshake from any other
/// client version.
const HOST_PROGRAMS: &[u8] = include_bytes!("host-programs.json");
/// SHA-256 of `host-programs.json`, so an accidental edit fails a test.
#[cfg(test)]
const HOST_PROGRAMS_SHA256: &str =
    "8eaed6e3af564328ce98f3a6f7d26e8526587830cab61876310d4edac13df8dc";

/// Loads the distribution: aither's embedded, hash-pinned baseline tools with
/// May's own host programs in place of the baseline's.
///
/// Artifacts are downloaded on first use and verified against the manifest's
/// SHA-256 digests before anything executes them.
///
/// # Errors
/// Fails when an embedded manifest does not validate.
pub fn embedded_distribution() -> Result<ToolDistribution, ToolRuntimeError> {
    let mut manifest: serde_json::Value = serde_json::from_slice(EMBEDDED_DISTRIBUTION)?;
    let host_programs: serde_json::Value = serde_json::from_slice(HOST_PROGRAMS)?;
    manifest["host_programs"] = host_programs["host_programs"].clone();
    ToolDistribution::from_json(&serde_json::to_vec(&manifest)?).map(authenticated)
}

/// Downloads GitHub release artifacts with the `GITHUB_TOKEN` environment
/// variable when it is set and non-empty (CI sets it to the job's token).
fn authenticated(distribution: ToolDistribution) -> ToolDistribution {
    match std::env::var("GITHUB_TOKEN") {
        Ok(token) if !token.is_empty() => distribution.with_github_token(token),
        _ => distribution,
    }
}

/// The distribution's `heel` for this machine, prepared under `cache_root`
/// (a home's tools directory) and verified to be the linked library's
/// version.
///
/// # Errors
/// Fails when the distribution pins no `heel` for this platform, the download
/// or its digest check fails, or the binary is not the linked library's
/// version.
pub async fn prepare_heel(
    distribution: &ToolDistribution,
    cache_root: &Path,
) -> Result<HeelBinary, ToolRuntimeError> {
    let heel = distribution
        .prepare_host_program(HostProgram::Heel, &ToolTarget::native()?, cache_root)
        .await?;
    let path = heel.path().to_path_buf();
    Ok(blocking::unblock(move || HeelBinary::verify(path)).await?)
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::{
        EMBEDDED_DISTRIBUTION, EMBEDDED_DISTRIBUTION_SHA256, HOST_PROGRAMS, HOST_PROGRAMS_SHA256,
        embedded_distribution,
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
    }

    /// May pins `heel` at the linked library's version on every platform the
    /// baseline distribution serves but Intel macOS, so the sandbox binary
    /// always matches the library.
    #[test]
    fn heel_is_pinned_at_the_linked_library_version() {
        let version = env!("MAY_HEEL_LIB_VERSION");
        let pins: serde_json::Value = serde_json::from_slice(HOST_PROGRAMS).unwrap();
        let mut platforms = pins["host_programs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|program| program["tool"] == "heel")
            .map(|program| {
                assert_eq!(program["version"], version, "{program}");
                assert_eq!(program["sha256"], program["executable_sha256"], "{program}");
                let url = program["url"].as_str().unwrap();
                assert!(
                    url.starts_with(&format!(
                        "https://github.com/May-Agent/heel/releases/download/heel-v{version}/"
                    )),
                    "{url}"
                );
                format!(
                    "{}-{}",
                    program["target"]["os"].as_str().unwrap(),
                    program["target"]["arch"].as_str().unwrap()
                )
            })
            .collect::<Vec<_>>();
        platforms.sort();
        assert_eq!(
            platforms,
            [
                "linux-aarch64",
                "linux-x86_64",
                "macos-aarch64",
                "windows-aarch64",
                "windows-x86_64",
            ]
        );
    }

    /// The distribution prepares this platform's `heel` without any
    /// environment variable, as the linked library's version.
    #[tokio::test]
    async fn the_distribution_prepares_heel_on_this_platform() {
        super::test_heel().await;
    }
}
