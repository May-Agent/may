//! The stable path: the latest GitHub release.
//!
//! cargo-dist attaches `may-agent-<target>.tar.xz` and its
//! `may-agent-<target>.tar.xz.sha256` to each release; the archive holds the
//! executable as `may-agent-<target>/may`. The repository also carries
//! releases of its other crates (`patch/v…`, `sed/v…`), which are told apart
//! by their tag: May's own is `v<version>`.

use std::io::Read as _;
use std::path::PathBuf;

use serde::Deserialize;

use super::UpdateError;
use super::github::{GitHub, verify_sha256};

/// The repository whose releases are the stable channel.
const REPOSITORY: &str = super::ci::REPOSITORY;

/// Releases listed per page: far more than the other crates' releases that
/// can sit in front of May's newest.
const RELEASES_SCANNED: u32 = 30;

/// The package cargo-dist names its archives after.
const ARCHIVE_PREFIX: &str = "may-agent";

/// The executable inside the archive.
const EXECUTABLE: &str = "may";

/// Upper bound of the unpacked executable, far above a release `may`.
const EXECUTABLE_LIMIT: u64 = 4 * 1024 * 1024 * 1024;

/// Memory the xz decoder may use, in KiB: above the dictionary of the
/// highest preset cargo-dist's `xz` uses.
const XZ_MEMORY_LIMIT_KIB: u32 = 256 * 1024;

/// A platform the release workflow builds (`targets` in
/// `dist-workspace.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleasePlatform {
    /// macOS on Apple silicon.
    MacosArm64,
    /// Linux on arm64.
    LinuxArm64,
    /// Linux on x86-64.
    LinuxX64,
}

impl ReleasePlatform {
    /// The platform this binary runs on.
    ///
    /// # Errors
    /// Fails on a platform no release is built for.
    pub const fn current() -> Result<Self, UpdateError> {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Ok(Self::MacosArm64)
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            Ok(Self::LinuxArm64)
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Ok(Self::LinuxX64)
        } else {
            Err(UpdateError::UnsupportedPlatform)
        }
    }

    /// The target triple cargo-dist names the platform's archive after.
    #[must_use]
    pub const fn target(self) -> &'static str {
        match self {
            Self::MacosArm64 => "aarch64-apple-darwin",
            Self::LinuxArm64 => "aarch64-unknown-linux-gnu",
            Self::LinuxX64 => "x86_64-unknown-linux-gnu",
        }
    }

    fn archive(self) -> String {
        format!("{ARCHIVE_PREFIX}-{}.tar.xz", self.target())
    }

    fn executable(self) -> PathBuf {
        PathBuf::from(format!("{ARCHIVE_PREFIX}-{}", self.target())).join(EXECUTABLE)
    }
}

/// One entry of `GET /repos/{repo}/releases`.
#[derive(Debug, Deserialize)]
struct Entry {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<AssetEntry>,
}

#[derive(Debug, Deserialize)]
struct AssetEntry {
    name: String,
    browser_download_url: String,
}

/// The newest release of May itself, with the platform's assets.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Latest {
    pub(super) version: semver::Version,
    archive_url: String,
    checksum_url: String,
}

/// The newest published release whose tag is `v<semver>`, with the assets of
/// `platform`.
///
/// # Errors
/// [`UpdateError::NoRelease`] when no release is May's own,
/// [`UpdateError::NoReleaseAsset`] when the newest lacks the platform's
/// archive or its checksum.
pub(super) fn parse_latest(body: &[u8], platform: ReleasePlatform) -> Result<Latest, UpdateError> {
    let entries = serde_json::from_slice::<Vec<Entry>>(body)?;
    let (version, entry) = entries
        .into_iter()
        .filter(|entry| !entry.draft && !entry.prerelease)
        .find_map(|entry| {
            let version = entry.tag_name.strip_prefix('v')?.parse().ok()?;
            Some((version, entry))
        })
        .ok_or(UpdateError::NoRelease)?;
    let archive = platform.archive();
    let checksum = format!("{archive}.sha256");
    let find = |name: &str| {
        entry
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .map(|asset| asset.browser_download_url.clone())
            .ok_or_else(|| UpdateError::NoReleaseAsset {
                name: name.to_owned(),
                tag: entry.tag_name.clone(),
            })
    };
    Ok(Latest {
        version,
        archive_url: find(&archive)?,
        checksum_url: find(&checksum)?,
    })
}

/// The sha256 a `.sha256` asset states: its first word, `<hex> *<name>`.
///
/// # Errors
/// [`UpdateError::InvalidChecksum`] when the file does not start with a
/// sha256.
pub(super) fn parse_checksum(text: &str) -> Result<&str, UpdateError> {
    text.split_whitespace()
        .next()
        .filter(|word| word.len() == 64 && word.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or(UpdateError::InvalidChecksum)
}

impl GitHub {
    /// The newest release of May for `platform`.
    pub(super) async fn latest_release(
        &self,
        platform: ReleasePlatform,
    ) -> Result<Latest, UpdateError> {
        let url = format!(
            "{}/repos/{REPOSITORY}/releases?per_page={RELEASES_SCANNED}",
            self.api_base()
        );
        parse_latest(&self.get(&url).await?.bytes().await?, platform)
    }

    /// The release's archive, checked against the checksum the release
    /// publishes beside it. `progress` reports each chunk's running
    /// `downloaded`/`total` bytes.
    pub(super) async fn download_release(
        &self,
        latest: &Latest,
        progress: &mut impl FnMut(u64, Option<u64>),
    ) -> Result<Vec<u8>, UpdateError> {
        let checksum = self.get_public(&latest.checksum_url).await?.bytes().await?;
        let archive =
            super::github::body_bytes(self.get_public(&latest.archive_url).await?, None, progress)
                .await?;
        let checksum = String::from_utf8_lossy(&checksum);
        verify_sha256(&archive, parse_checksum(&checksum)?)?;
        Ok(archive.into())
    }
}

/// The `may` executable inside a release archive.
pub(super) fn extract_executable(
    archive: &[u8],
    platform: ReleasePlatform,
) -> Result<Vec<u8>, UpdateError> {
    let wanted = platform.executable();
    let decoder = lzma_rust2::XzReader::new_mem_limit(archive, false, XZ_MEMORY_LIMIT_KIB);
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries()? {
        let entry = entry?;
        if entry.path()?.as_ref() == wanted {
            let mut bytes = Vec::new();
            entry.take(EXECUTABLE_LIMIT).read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
    }
    Err(UpdateError::MissingMember(EXECUTABLE))
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use sha2::Digest as _;

    use super::*;

    const RELEASES: &[u8] = include_bytes!("testdata/releases.json");

    #[test]
    fn the_newest_release_of_may_is_the_one_tagged_with_a_bare_version() {
        let latest =
            parse_latest(RELEASES, ReleasePlatform::MacosArm64).expect("a recorded listing parses");
        // The listing leads with a tool release and a draft.
        assert_eq!(latest.version, semver::Version::new(0, 1, 1));
        assert_eq!(
            latest.archive_url,
            "https://github.com/May-Agent/may/releases/download/v0.1.1/may-agent-aarch64-apple-darwin.tar.xz"
        );
        assert_eq!(
            latest.checksum_url,
            "https://github.com/May-Agent/may/releases/download/v0.1.1/may-agent-aarch64-apple-darwin.tar.xz.sha256"
        );
    }

    #[test]
    fn each_platform_gets_its_own_archive() {
        let latest = parse_latest(RELEASES, ReleasePlatform::LinuxX64).expect("the listing parses");
        assert!(
            latest
                .archive_url
                .ends_with("may-agent-x86_64-unknown-linux-gnu.tar.xz")
        );
    }

    #[test]
    fn a_listing_without_a_release_of_may_has_none() {
        let listing =
            br#"[{"tag_name":"sed/v0.1.0","draft":false,"prerelease":false,"assets":[]}]"#;
        assert!(matches!(
            parse_latest(listing, ReleasePlatform::LinuxX64),
            Err(UpdateError::NoRelease)
        ));
    }

    #[test]
    fn a_release_without_the_platforms_archive_is_refused() {
        let listing = br#"[{"tag_name":"v0.2.0","draft":false,"prerelease":false,"assets":[]}]"#;
        let error = parse_latest(listing, ReleasePlatform::LinuxX64).expect_err("no assets");
        assert!(
            matches!(&error, UpdateError::NoReleaseAsset { name, tag }
                if name == "may-agent-x86_64-unknown-linux-gnu.tar.xz" && tag == "v0.2.0"),
            "{error:?}"
        );
    }

    #[test]
    fn the_checksum_is_the_first_word_of_the_sha256_file() {
        let text = "49149f726178db62509e13e736c121d0bb232716b52c1e1c8056103cec9189c9 *may-agent-aarch64-apple-darwin.tar.xz\n";
        assert_eq!(
            parse_checksum(text).expect("a recorded checksum file parses"),
            "49149f726178db62509e13e736c121d0bb232716b52c1e1c8056103cec9189c9"
        );
        assert!(matches!(
            parse_checksum("<html>Not Found</html>"),
            Err(UpdateError::InvalidChecksum)
        ));
    }

    #[tokio::test]
    async fn the_archive_is_checked_against_the_published_checksum() {
        use crate::test_support::{ApiReply, serve_api};

        // The archive is text so the loopback API can serve it.
        let archive = "release archive 日本語 한국어 中文";
        let digest = hex::encode(sha2::Sha256::digest(archive));
        let (base, requests) = serve_api(move |request, _| match request.path.as_str() {
            "/may.tar.xz" | "/bad.tar.xz" => ApiReply::json(200, archive),
            "/may.tar.xz.sha256" => ApiReply::json(200, format!("{digest} *may.tar.xz\n")),
            "/bad.tar.xz.sha256" => {
                ApiReply::json(200, format!("{} *bad.tar.xz\n", "0".repeat(64)))
            }
            _ => ApiReply::json(404, r#"{"message":"Not Found"}"#),
        })
        .await;
        let github = GitHub::new(&base, Some("api-token".into()));
        let latest = |name: &str| Latest {
            version: semver::Version::new(0, 1, 1),
            archive_url: format!("{base}/{name}.tar.xz"),
            checksum_url: format!("{base}/{name}.tar.xz.sha256"),
        };

        assert_eq!(
            github
                .download_release(&latest("may"), &mut |_, _| {})
                .await
                .expect("the archive matches its checksum"),
            archive.as_bytes()
        );
        let error = github
            .download_release(&latest("bad"), &mut |_, _| {})
            .await
            .expect_err("the checksum differs");
        assert!(
            matches!(error, UpdateError::DigestMismatch { .. }),
            "{error:?}"
        );
        // Public downloads carry no credentials.
        assert!(
            requests
                .lock()
                .expect("requests")
                .iter()
                .all(|request| request.authorization.is_empty())
        );
    }

    /// A release archive as cargo-dist packs it: an xz tarball holding
    /// `may-agent-<target>/may` beside the documents.
    fn release_archive(platform: ReleasePlatform, executable: &[u8]) -> Vec<u8> {
        let mut tarball = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tarball);
            for (name, bytes) in [("README.md", b"readme".as_slice()), ("may", executable)] {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder
                    .append_data(
                        &mut header,
                        format!("{ARCHIVE_PREFIX}-{}/{name}", platform.target()),
                        bytes,
                    )
                    .expect("the tarball is written");
            }
            builder.finish().expect("the tarball is finished");
        }
        let mut writer =
            lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1))
                .expect("the xz stream starts");
        writer
            .write_all(&tarball)
            .expect("the xz stream is written");
        writer.finish().expect("the xz stream is finished")
    }

    #[test]
    fn the_executable_is_unpacked_from_the_release_archive() {
        let archive = release_archive(
            ReleasePlatform::MacosArm64,
            "日本語 한국어 中文 may".as_bytes(),
        );
        assert_eq!(
            extract_executable(&archive, ReleasePlatform::MacosArm64).expect("the archive unpacks"),
            "日本語 한국어 中文 may".as_bytes()
        );
    }

    #[test]
    fn another_platforms_archive_has_no_executable_for_this_one() {
        let archive = release_archive(ReleasePlatform::LinuxX64, b"may");
        assert!(matches!(
            extract_executable(&archive, ReleasePlatform::MacosArm64),
            Err(UpdateError::MissingMember(EXECUTABLE))
        ));
    }
}
