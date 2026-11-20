//! Self-update.
//!
//! The channel `[update] channel` names is read when an update runs, never
//! fixed at compile time; unset, it is the running build's kind
//! ([`Channel::of_build`]).
//!
//! - [`Channel::Stable`] is the latest GitHub release: the platform's
//!   `may-agent-<target>.tar.xz`, checked against the release's `.sha256`
//!   ([`release`]).
//! - [`Channel::Dev`] is the debug `may` of the latest successful CI run on
//!   `main`, checked against GitHub's digest of it ([`ci`]).
//!
//! Both end in one install path that replaces the executable in place. A
//! build is current on a channel only when it is that channel's build, so
//! switching channel always installs.
//!
//! A binary records how it was built ([`BuildKind`]): a release build, a CI
//! build (compiled by GitHub Actions, the build the dev channel downloads) or
//! a local build. `may update` works on every kind; the daemon updates itself
//! in the background only on release and CI builds, so a locally compiled
//! binary is never overwritten behind its developer.
//!
//! Replacing the executable does not replace a daemon that is running: the
//! callers restart it gracefully (`daemon::lifecycle`).

mod ci;
mod github;
mod install;
mod release;

pub use ci::CiPlatform;
pub use release::ReleasePlatform;

use std::path::{Path, PathBuf};

use crate::issue::{GhCliTokenSource, GitHubTokenSource as _};

use github::GitHub;

/// The commit this binary was built from; empty when git was unavailable.
const BUILT_FROM: &str = env!("MAY_GIT_SHA");

/// The GitHub REST API both channels read.
const GITHUB_API: &str = "https://api.github.com";

/// Where a build gets its updates from.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
    skyzen::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// The latest GitHub release.
    Stable,
    /// The latest successful CI build of `main`.
    Dev,
}

impl Channel {
    /// The channel of the running binary's kind.
    #[must_use]
    pub const fn of_build() -> Self {
        BuildKind::current().channel()
    }
}

/// How the running binary was built, recorded when it is compiled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildKind {
    /// A cargo-dist release build (`release-build`).
    Release,
    /// Compiled by GitHub Actions: the build the dev channel downloads.
    Ci,
    /// Compiled anywhere else, such as `cargo build` on a developer's machine.
    Local,
}

impl BuildKind {
    /// The kind of the running binary.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(feature = "release-build") {
            Self::Release
        } else if matches!(env!("MAY_BUILT_BY_CI").as_bytes(), b"true") {
            Self::Ci
        } else {
            Self::Local
        }
    }

    /// The channel a build of this kind follows when `[update] channel` is
    /// unset: a release build is `stable`, any other `dev`.
    #[must_use]
    pub const fn channel(self) -> Channel {
        match self {
            Self::Release => Channel::Stable,
            Self::Ci | Self::Local => Channel::Dev,
        }
    }

    /// Whether the daemon of a build of this kind updates itself in the
    /// background. A local build never does, so a `cargo build` binary is
    /// never overwritten behind its developer; `may update` still works on
    /// it.
    #[must_use]
    pub const fn auto_updates(self) -> bool {
        match self {
            Self::Release | Self::Ci => true,
            Self::Local => false,
        }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Stable => "stable",
            Self::Dev => "dev",
        })
    }
}

/// What an update reports to its `progress` callback.
#[derive(Debug, Clone)]
pub enum Progress {
    /// A step began; `text` is the line to show.
    Step(String),
    /// `downloaded` of `total` bytes of the build have arrived; `total` is
    /// `None` when the server announced no length.
    Download {
        /// Bytes received so far.
        downloaded: u64,
        /// The announced body length.
        total: Option<u64>,
    },
}

impl Progress {
    /// The step line `text`.
    fn step(text: impl Into<String>) -> Self {
        Self::Step(text.into())
    }
}

/// What an update did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The build already is the newest one.
    UpToDate,
    /// The executable at `executable` was replaced.
    Updated {
        /// The replaced executable.
        executable: PathBuf,
        /// What it was replaced with, for the user.
        to: String,
    },
}

/// Why an update failed.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// No build of May exists for this platform.
    #[error("no build of May exists for this platform")]
    UnsupportedPlatform,
    /// The GitHub login could not be read.
    #[error("a GitHub login is needed to download CI artifacts: {0}")]
    Token(String),
    /// No successful run on `main` has the artifact.
    #[error("none of the latest successful CI runs on main has the artifact {name}")]
    NoArtifact {
        /// The artifact looked for.
        name: String,
    },
    /// No successful CI run of a commit has the artifact.
    #[error(
        "no successful CI run of commit {commit} has the artifact {name}; push the commit and let CI build it"
    )]
    NoCommitBuild {
        /// The artifact looked for.
        name: String,
        /// The commit.
        commit: String,
    },
    /// This binary was built without git, so no commit names its build.
    #[error("this May was built without its commit id, so no CI build of it can be found")]
    UnknownCommit,
    /// The downloaded file is not the one that was announced.
    #[error("the download has sha256 {actual}, expected {expected}")]
    DigestMismatch {
        /// The digest that was announced.
        expected: String,
        /// The digest of the download.
        actual: String,
    },
    /// The archive lacks an expected file.
    #[error("the archive has no {0}")]
    MissingMember(&'static str),
    /// The repository has no published release of May.
    #[error("the repository has no published release of May")]
    NoRelease,
    /// A release lacks the platform's asset.
    #[error("the release {tag} has no {name}")]
    NoReleaseAsset {
        /// The asset looked for.
        name: String,
        /// The release.
        tag: String,
    },
    /// A release's `.sha256` asset does not hold a sha256.
    #[error("the release's checksum file does not hold a sha256")]
    InvalidChecksum,
    /// GitHub answered with an error or could not be reached.
    #[error("GitHub request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// GitHub's answer is not the documented shape.
    #[error("unreadable GitHub response: {0}")]
    Json(#[from] serde_json::Error),
    /// The artifact is not a valid zip.
    #[error("invalid artifact archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    /// Reading the archives or replacing the executable failed.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// What the running binary is, for deciding whether a build is current.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Build {
    /// How the binary was built.
    kind: BuildKind,
    /// The crate version.
    version: semver::Version,
    /// The commit it was built from.
    commit: Option<String>,
}

impl Build {
    fn this() -> Self {
        Self {
            kind: BuildKind::current(),
            version: env!("CARGO_PKG_VERSION")
                .parse()
                .expect("the crate version is a semantic version"),
            commit: built_from().map(str::to_owned),
        }
    }

    /// Whether this is the release `latest`: a release build of its version.
    fn is_release(&self, latest: &semver::Version) -> bool {
        self.kind == BuildKind::Release && self.version == *latest
    }

    /// Whether this is the CI build of `head_sha`: a non-release build of
    /// that commit.
    fn is_ci_build_of(&self, head_sha: &str) -> bool {
        self.kind != BuildKind::Release && self.commit.as_deref() == Some(head_sha)
    }
}

/// A newer build of the channel, found and not yet installed.
#[derive(Debug)]
pub struct Available {
    github: GitHub,
    source: Source,
    to: String,
}

#[derive(Debug)]
enum Source {
    Release {
        latest: release::Latest,
        platform: ReleasePlatform,
    },
    Ci {
        found: ci::Found,
    },
}

impl Available {
    /// What the build is, for the user.
    #[must_use]
    pub fn to(&self) -> &str {
        &self.to
    }

    /// Downloads the build, checks it and replaces the executable at
    /// `target` with it. Nothing is written unless the download passed its
    /// check. `progress` receives one [`Progress::Step`] line per step and
    /// [`Progress::Download`] events as the body arrives.
    ///
    /// # Errors
    /// Fails when the download, its verification or the replacement fails.
    pub async fn install(
        self,
        target: &Path,
        progress: &mut impl FnMut(Progress),
    ) -> Result<(), UpdateError> {
        let executable = match self.source {
            Source::Release { latest, platform } => {
                progress(Progress::step(format!(
                    "Downloading May {}…",
                    latest.version
                )));
                let archive = self
                    .github
                    .download_release(&latest, &mut |downloaded, total| {
                        progress(Progress::Download { downloaded, total });
                    })
                    .await?;
                blocking::unblock(move || release::extract_executable(&archive, platform)).await?
            }
            Source::Ci { found } => {
                progress(Progress::step(format!(
                    "Downloading the CI build of {} ({} MB)…",
                    short(&found.head_sha),
                    found.artifact.size_in_bytes / 1_000_000
                )));
                let zip = self
                    .github
                    .download(&found.artifact, &mut |downloaded, total| {
                        progress(Progress::Download { downloaded, total });
                    })
                    .await?;
                blocking::unblock(move || ci::extract_executable(&zip)).await?
            }
        };
        let target = target.to_owned();
        blocking::unblock(move || install::replace_executable(&target, &executable)).await
    }
}

/// The running executable, resolved. It is the file an update replaces.
///
/// # Errors
/// Fails when the path cannot be read; on Linux also once the executable was
/// replaced, so a long-running process reads it once at its start.
pub fn running_executable() -> std::io::Result<PathBuf> {
    dunce::canonicalize(std::env::current_exe()?)
}

/// Looks for a newer build on `channel`; `None` when the running build is
/// the channel's newest. `progress` receives one [`Progress::Step`] line
/// per step.
///
/// # Errors
/// Fails when the channel's newest build cannot be found.
pub async fn check(
    channel: Channel,
    progress: &mut impl FnMut(Progress),
) -> Result<Option<Available>, UpdateError> {
    let token = GhCliTokenSource
        .token()
        .await
        .map_err(|error| error.to_string());
    check_at(GITHUB_API, channel, &Build::this(), token, progress).await
}

async fn check_at(
    api_base: &str,
    channel: Channel,
    build: &Build,
    token: Result<String, String>,
    progress: &mut impl FnMut(Progress),
) -> Result<Option<Available>, UpdateError> {
    match channel {
        Channel::Stable => {
            let platform = ReleasePlatform::current()?;
            let github = GitHub::new(api_base, token.ok());
            progress(Progress::step("Looking for the latest release…"));
            let latest = github.latest_release(platform).await?;
            if build.is_release(&latest.version) {
                return Ok(None);
            }
            let to = format!("release {}", latest.version);
            Ok(Some(Available {
                github,
                source: Source::Release { latest, platform },
                to,
            }))
        }
        Channel::Dev => {
            let platform = CiPlatform::current()?;
            let token = token.map_err(UpdateError::Token)?;
            let github = GitHub::new(api_base, Some(token));
            progress(Progress::step(
                "Looking for the latest successful CI build…",
            ));
            let found = github.latest_artifact(platform.artifact()).await?;
            if build.is_ci_build_of(&found.head_sha) {
                return Ok(None);
            }
            let to = format!("the CI build of {}", short(&found.head_sha));
            Ok(Some(Available {
                github,
                source: Source::Ci { found },
                to,
            }))
        }
    }
}

/// Updates the executable at `target` on `channel`. `progress` receives one
/// [`Progress::Step`] line per step and [`Progress::Download`] events as
/// the download arrives, for the caller's loading indicator.
///
/// # Errors
/// Fails when the newest build cannot be found, downloaded, verified or
/// installed; the executable is untouched then.
pub async fn update(
    channel: Channel,
    target: &Path,
    mut progress: impl FnMut(Progress),
) -> Result<Outcome, UpdateError> {
    let Some(available) = check(channel, &mut progress).await? else {
        return Ok(Outcome::UpToDate);
    };
    let to = available.to().to_owned();
    available.install(target, &mut progress).await?;
    Ok(Outcome::Updated {
        executable: target.to_owned(),
        to,
    })
}

/// The commit this binary was built from, when git was available.
#[must_use]
pub const fn built_from() -> Option<&'static str> {
    if BUILT_FROM.is_empty() {
        None
    } else {
        Some(BUILT_FROM)
    }
}

/// The debug `may` CI built for `platform` from this binary's commit.
///
/// It is the same build, for another machine, downloaded with `token`,
/// checked against GitHub's digest and answered as the gzip tarball CI
/// packs it in (`may` with its mode). `progress` receives one line per
/// step.
///
/// # Errors
/// [`UpdateError::UnknownCommit`] for a build without its commit id,
/// [`UpdateError::NoCommitBuild`] when CI has not built the commit;
/// otherwise the download's failure.
pub async fn this_build_tarball_for(
    platform: CiPlatform,
    token: String,
    mut progress: impl FnMut(Progress),
) -> Result<Vec<u8>, UpdateError> {
    let commit = built_from().ok_or(UpdateError::UnknownCommit)?;
    this_commit_from(GITHUB_API, platform, commit, token, &mut progress).await
}

async fn this_commit_from(
    api_base: &str,
    platform: CiPlatform,
    commit: &str,
    token: String,
    progress: &mut impl FnMut(Progress),
) -> Result<Vec<u8>, UpdateError> {
    let name = platform.artifact();
    let github = GitHub::new(api_base, Some(token));
    progress(Progress::step(format!(
        "Looking for the CI build of {}…",
        short(commit)
    )));
    let found = github.artifact_of_commit(name, commit).await?;
    progress(Progress::step(format!(
        "Downloading {name} ({} MB)…",
        found.artifact.size_in_bytes / 1_000_000
    )));
    let zip = github
        .download(&found.artifact, &mut |downloaded, total| {
            progress(Progress::Download { downloaded, total });
        })
        .await?;
    blocking::unblock(move || ci::extract_tarball(&zip)).await
}

/// The leading characters of a commit id.
fn short(sha: &str) -> &str {
    sha.get(..sha.len().min(9)).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ApiReply, serve_api};

    const RELEASES: &[u8] = include_bytes!("testdata/releases.json");
    const RUNS: &[u8] = include_bytes!("testdata/ci_runs.json");
    const ARTIFACTS: &[u8] = include_bytes!("testdata/ci_artifacts.json");
    const HEAD: &str = "6bbeeed53be58e7e1be3ecde6a781dcc5a5c6711";

    fn build(kind: BuildKind, version: &str, commit: Option<&str>) -> Build {
        Build {
            kind,
            version: version.parse().expect("a version"),
            commit: commit.map(str::to_owned),
        }
    }

    #[test]
    fn an_unset_channel_is_the_kind_of_the_build() {
        assert_eq!(BuildKind::Release.channel(), Channel::Stable);
        assert_eq!(BuildKind::Ci.channel(), Channel::Dev);
        assert_eq!(BuildKind::Local.channel(), Channel::Dev);
        assert_eq!(Channel::of_build(), BuildKind::current().channel());
    }

    #[test]
    fn only_installed_builds_update_themselves() {
        assert!(BuildKind::Release.auto_updates());
        assert!(BuildKind::Ci.auto_updates());
        assert!(!BuildKind::Local.auto_updates());
    }

    #[test]
    fn a_build_is_a_release_only_with_the_release_feature() {
        assert_eq!(
            BuildKind::current() == BuildKind::Release,
            cfg!(feature = "release-build")
        );
    }

    #[test]
    fn a_ci_build_is_the_dev_channels_build_like_a_local_one() {
        assert!(build(BuildKind::Ci, "0.1.1", Some(HEAD)).is_ci_build_of(HEAD));
        assert!(
            !build(BuildKind::Ci, "0.1.1", Some(HEAD)).is_release(&semver::Version::new(0, 1, 1))
        );
    }

    #[test]
    fn a_release_build_is_current_only_at_the_latest_release() {
        let latest = semver::Version::new(0, 1, 1);
        assert!(build(BuildKind::Release, "0.1.1", None).is_release(&latest));
        assert!(!build(BuildKind::Release, "0.1.0", None).is_release(&latest));
        // A non-release build of the same version is not the release.
        assert!(!build(BuildKind::Local, "0.1.1", Some(HEAD)).is_release(&latest));
    }

    #[test]
    fn a_non_release_build_is_current_only_at_the_ci_head() {
        assert!(build(BuildKind::Local, "0.1.1", Some(HEAD)).is_ci_build_of(HEAD));
        assert!(!build(BuildKind::Local, "0.1.1", Some("0123abc")).is_ci_build_of(HEAD));
        // Without its commit a build cannot be told current.
        assert!(!build(BuildKind::Local, "0.1.1", None).is_ci_build_of(HEAD));
        // A release build is never the dev channel's build, even of that commit.
        assert!(!build(BuildKind::Release, "0.1.1", Some(HEAD)).is_ci_build_of(HEAD));
    }

    #[test]
    fn a_commit_id_is_shortened_on_a_char_boundary() {
        assert_eq!(short(HEAD), "6bbeeed53");
        assert_eq!(short("abc"), "abc");
    }

    /// The loopback API answering what GitHub does for the recorded listings
    /// of platform `platform`.
    async fn serve_listings(platform: &'static str) -> String {
        let releases = String::from_utf8_lossy(RELEASES).into_owned();
        let (base, _) = serve_api(move |request, _| match request.path.as_str() {
            path if path.starts_with("/repos/May-Agent/may/releases?") => {
                ApiReply::json(200, releases.clone())
            }
            path if path.starts_with("/repos/May-Agent/may/actions/workflows/ci.yml/runs?") => {
                ApiReply::json(200, String::from_utf8_lossy(RUNS).into_owned())
            }
            path if path
                .starts_with("/repos/May-Agent/may/actions/runs/43155546229/artifacts?")
                && path.ends_with(platform) =>
            {
                ApiReply::json(200, String::from_utf8_lossy(ARTIFACTS).into_owned())
            }
            _ => ApiReply::json(404, r#"{"message":"Not Found"}"#),
        })
        .await;
        base
    }

    #[tokio::test]
    async fn stable_installs_unless_the_build_is_the_latest_release() {
        let Ok(platform) = ReleasePlatform::current() else {
            return;
        };
        let base = serve_listings("").await;
        let token = || Err("no login".to_owned());
        let mut ignore = |_: Progress| {};

        let current = build(BuildKind::Release, "0.1.1", None);
        let found = check_at(&base, Channel::Stable, &current, token(), &mut ignore)
            .await
            .expect("the check runs without a login");
        assert!(found.is_none());

        // A dev build of the same version, and an older release, install.
        for build in [
            build(BuildKind::Local, "0.1.1", Some(HEAD)),
            build(BuildKind::Release, "0.1.0", None),
        ] {
            let found = check_at(&base, Channel::Stable, &build, token(), &mut ignore)
                .await
                .expect("the check runs")
                .expect("a newer build");
            assert_eq!(found.to(), "release 0.1.1");
            let Source::Release {
                latest,
                platform: found_platform,
            } = found.source
            else {
                panic!("the stable channel found a release");
            };
            assert_eq!(found_platform, platform);
            assert_eq!(latest.version, semver::Version::new(0, 1, 1));
        }
    }

    #[tokio::test]
    async fn dev_installs_unless_the_build_is_the_ci_head() {
        let Ok(platform) = CiPlatform::current() else {
            return;
        };
        let base = serve_listings(platform.artifact()).await;
        let token = || Ok("test-token".to_owned());
        let mut ignore = |_: Progress| {};

        let current = build(BuildKind::Local, "0.1.1", Some(HEAD));
        let found = check_at(&base, Channel::Dev, &current, token(), &mut ignore)
            .await
            .expect("the check runs");
        assert!(found.is_none());

        // A release build, even of this commit, moves to the dev channel.
        let release = build(BuildKind::Release, "0.1.1", Some(HEAD));
        let found = check_at(&base, Channel::Dev, &release, token(), &mut ignore)
            .await
            .expect("the check runs")
            .expect("a newer build");
        assert_eq!(found.to(), "the CI build of 6bbeeed53");
    }

    #[tokio::test]
    async fn dev_without_a_github_login_cannot_check() {
        let Ok(platform) = CiPlatform::current() else {
            return;
        };
        let base = serve_listings(platform.artifact()).await;
        let mut ignore = |_: Progress| {};
        let error = check_at(
            &base,
            Channel::Dev,
            &build(BuildKind::Local, "0.1.1", None),
            Err("no login".to_owned()),
            &mut ignore,
        )
        .await
        .expect_err("the dev channel needs a login");
        assert!(matches!(error, UpdateError::Token(_)), "{error:?}");
    }

    #[test]
    fn channels_are_written_in_lowercase() {
        assert_eq!(
            serde_json::to_string(&Channel::Stable).expect("serializes"),
            "\"stable\""
        );
        assert_eq!(
            serde_json::from_str::<Channel>("\"dev\"").expect("parses"),
            Channel::Dev
        );
        assert_eq!(Channel::Dev.to_string(), "dev");
    }
}
