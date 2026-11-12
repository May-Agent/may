//! The dev-build path: the `may-debug-<os>-<arch>` artifact of the latest
//! successful CI run on `main`.
//!
//! CI packs the debug `may` as `may-debug.tar.gz` (artifacts drop the
//! executable bit) and GitHub wraps every artifact in a zip, so the download
//! is a zip holding a tarball holding `may`. Downloading an artifact needs a
//! GitHub token even on a public repository.

use std::io::{Cursor, Read as _};
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use super::UpdateError;

/// The repository whose CI builds are the dev channel.
pub(super) const REPOSITORY: &str = "May-Agent/may";

/// The branch whose builds are the dev channel.
const BRANCH: &str = "main";

/// The tarball CI packs the debug `may` into (`.github/actions/test`).
const TARBALL: &str = "may-debug.tar.gz";

/// The executable inside the tarball.
const EXECUTABLE: &str = "may";

/// Runs listed per page. A run the scope job skipped the tests of has no
/// artifact, so the search walks past it; this bounds how far back it looks.
const RUNS_SCANNED: u32 = 30;

/// Upper bound of the unpacked executable, far above a debug `may`.
const EXECUTABLE_LIMIT: u64 = 4 * 1024 * 1024 * 1024;

/// The artifact name CI gives this platform's debug `may`.
///
/// # Errors
/// Fails on a platform CI builds no debug `may` for.
pub(super) const fn artifact_name() -> Result<&'static str, UpdateError> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok("may-debug-macos-arm64")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Ok("may-debug-linux-x64")
    } else {
        Err(UpdateError::UnsupportedPlatform)
    }
}

#[derive(Debug, Deserialize)]
struct Runs {
    workflow_runs: Vec<Run>,
}

/// One entry of `GET /repos/{repo}/actions/workflows/ci.yml/runs`.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(super) struct Run {
    pub(super) id: u64,
    pub(super) head_sha: String,
}

#[derive(Debug, Deserialize)]
struct Artifacts {
    artifacts: Vec<Artifact>,
}

/// One entry of `GET /repos/{repo}/actions/runs/{id}/artifacts`.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(super) struct Artifact {
    pub(super) size_in_bytes: u64,
    pub(super) archive_download_url: String,
    pub(super) expired: bool,
    /// `sha256:<hex>` of the zip, when GitHub computed one.
    pub(super) digest: Option<String>,
}

/// The newest-first runs of the listing.
pub(super) fn parse_runs(body: &[u8]) -> Result<Vec<Run>, UpdateError> {
    Ok(serde_json::from_slice::<Runs>(body)?.workflow_runs)
}

/// The unexpired artifact of a run's listing, when it has one.
pub(super) fn parse_artifact(body: &[u8]) -> Result<Option<Artifact>, UpdateError> {
    Ok(serde_json::from_slice::<Artifacts>(body)?
        .artifacts
        .into_iter()
        .find(|artifact| !artifact.expired))
}

/// A located artifact and the commit its run built.
pub(super) struct Found {
    pub(super) head_sha: String,
    pub(super) artifact: Artifact,
}

/// The GitHub REST client of the dev path.
pub(super) struct GitHub {
    client: reqwest::Client,
    api_base: String,
    token: String,
}

impl GitHub {
    pub(super) fn new(api_base: &str, token: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent(concat!("may-agent/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("the HTTP client builds with its default configuration"),
            api_base: api_base.trim_end_matches('/').to_owned(),
            token,
        }
    }

    async fn get(&self, url: &str) -> Result<reqwest::Response, UpdateError> {
        Ok(self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?
            .error_for_status()?)
    }

    /// The artifact `name` of the latest successful CI run on `main` that
    /// has one.
    pub(super) async fn latest_artifact(&self, name: &str) -> Result<Found, UpdateError> {
        let runs_url = format!(
            "{}/repos/{REPOSITORY}/actions/workflows/ci.yml/runs?branch={BRANCH}&event=push&status=success&per_page={RUNS_SCANNED}",
            self.api_base
        );
        let runs = parse_runs(&self.get(&runs_url).await?.bytes().await?)?;
        for run in runs {
            let url = format!(
                "{}/repos/{REPOSITORY}/actions/runs/{}/artifacts?name={name}",
                self.api_base, run.id
            );
            if let Some(artifact) = parse_artifact(&self.get(&url).await?.bytes().await?)? {
                return Ok(Found {
                    head_sha: run.head_sha,
                    artifact,
                });
            }
        }
        Err(UpdateError::NoArtifact {
            name: name.to_owned(),
        })
    }

    /// The artifact's zip, checked against the digest GitHub reports.
    pub(super) async fn download(&self, artifact: &Artifact) -> Result<Vec<u8>, UpdateError> {
        let bytes = self
            .get(&artifact.archive_download_url)
            .await?
            .bytes()
            .await?;
        if let Some(expected) = artifact
            .digest
            .as_deref()
            .and_then(|d| d.strip_prefix("sha256:"))
        {
            let actual = hex(&Sha256::digest(&bytes));
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(UpdateError::DigestMismatch {
                    expected: expected.to_owned(),
                    actual,
                });
            }
        }
        Ok(bytes.into())
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
        text
    })
}

/// The `may` executable inside an artifact zip.
pub(super) fn extract_executable(zip: &[u8]) -> Result<Vec<u8>, UpdateError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(zip))?;
    let mut tarball = None;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        if entry.name()? == TARBALL {
            let mut bytes = Vec::new();
            entry.take(EXECUTABLE_LIMIT).read_to_end(&mut bytes)?;
            tarball = Some(bytes);
            break;
        }
    }
    let tarball = tarball.ok_or(UpdateError::MissingMember(TARBALL))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(tarball.as_slice()));
    for entry in tar.entries()? {
        let entry = entry?;
        if entry.path()?.as_ref() == Path::new(EXECUTABLE) {
            let mut bytes = Vec::new();
            entry.take(EXECUTABLE_LIMIT).read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
    }
    Err(UpdateError::MissingMember(EXECUTABLE))
}

/// Replaces the executable at `target` with `bytes`: written beside it and
/// renamed over it, so a running process keeps its old image.
pub(super) fn replace_executable(target: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    let directory = target
        .parent()
        .ok_or_else(|| std::io::Error::other("the executable has no directory"))?;
    let mut staged = tempfile::NamedTempFile::new_in(directory)?;
    staged.write_all(bytes)?;
    staged
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    staged.persist(target).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    const RUNS: &[u8] = include_bytes!("testdata/ci_runs.json");
    const ARTIFACTS: &[u8] = include_bytes!("testdata/ci_artifacts.json");

    #[test]
    fn the_runs_listing_is_newest_first() {
        let runs = parse_runs(RUNS).expect("a recorded runs listing parses");
        assert_eq!(runs[0].id, 43_155_546_229);
        assert_eq!(runs[0].head_sha, "6bbeeed53be58e7e1be3ecde6a781dcc5a5c6711");
    }

    #[test]
    fn a_runs_artifact_carries_its_download_url_and_digest() {
        let artifact = parse_artifact(ARTIFACTS)
            .expect("a recorded artifacts listing parses")
            .expect("the run has the artifact");
        assert_eq!(
            artifact.archive_download_url,
            "https://api.github.com/repos/May-Agent/may/actions/artifacts/13916715809/zip"
        );
        assert_eq!(
            artifact.digest.as_deref(),
            Some("sha256:256d53e061d0fd433c854a7d7df28a17bb5a52bb73627e3514c0d78e477eaceb")
        );
    }

    #[test]
    fn a_run_without_the_artifact_has_none() {
        let empty = br#"{"total_count":0,"artifacts":[]}"#;
        assert_eq!(
            parse_artifact(empty).expect("an empty listing parses"),
            None
        );
    }

    #[test]
    fn an_expired_artifact_is_skipped() {
        let text = std::str::from_utf8(ARTIFACTS)
            .expect("the recording is text")
            .replace("\"expired\":false", "\"expired\":true");
        assert_eq!(
            parse_artifact(text.as_bytes()).expect("the listing parses"),
            None
        );
    }

    /// An artifact zip as CI uploads it: `may-debug.tar.gz` holding `may`.
    fn artifact_zip(executable: &[u8]) -> Vec<u8> {
        let mut tarball = Vec::new();
        {
            let encoder =
                flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(executable.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "may", executable)
                .expect("the tarball is written");
            builder
                .into_inner()
                .expect("the tarball is finished")
                .finish()
                .expect("the gzip stream is finished");
        }
        let mut zip = Vec::new();
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut zip));
        writer
            .start_file(
                TARBALL,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .expect("the zip entry starts");
        writer
            .write_all(&tarball)
            .expect("the zip entry is written");
        writer.finish().expect("the zip is finished");
        zip
    }

    #[test]
    fn the_executable_is_unpacked_from_the_artifact() {
        let zip = artifact_zip("日本語 한국어 中文 may".as_bytes());
        assert_eq!(
            extract_executable(&zip).expect("the artifact unpacks"),
            "日本語 한국어 中文 may".as_bytes()
        );
    }

    #[test]
    fn a_zip_without_the_tarball_is_refused() {
        let mut zip = Vec::new();
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut zip));
        writer
            .start_file("other", zip::write::SimpleFileOptions::default())
            .expect("the zip entry starts");
        writer.finish().expect("the zip is finished");
        assert!(matches!(
            extract_executable(&zip),
            Err(UpdateError::MissingMember(TARBALL))
        ));
    }

    #[test]
    fn replacing_swaps_the_file_and_makes_it_executable() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("a temp dir");
        let target = dir.path().join("may");
        std::fs::write(&target, b"old").expect("the old executable is written");
        replace_executable(&target, b"new").expect("the executable is replaced");
        assert_eq!(std::fs::read(&target).expect("it reads"), b"new");
        let mode = std::fs::metadata(&target)
            .expect("it has metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(std::fs::read_dir(dir.path()).expect("it lists").count(), 1);
    }
}
