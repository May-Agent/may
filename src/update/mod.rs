//! Self-update.
//!
//! A build is on one of two channels, fixed when it is compiled:
//!
//! - [`Channel::Release`], a binary the cargo-dist release workflow built (it
//!   passes the `release-build` feature), updates to the latest GitHub
//!   release through cargo-dist's own updater, `axoupdater`, which re-runs
//!   the installer that installed it.
//! - [`Channel::Dev`], any other build, updates to the debug `may` of the
//!   latest successful CI run on `main` ([`ci`]).
//!
//! Neither replaces a daemon that is running: the next client replaces it
//! because its build id differs (`daemon::lifecycle::ensure_running`).

mod ci;

pub use ci::CiPlatform;

use std::path::PathBuf;

use crate::issue::{GhCliTokenSource, GitHubTokenSource as _};

/// The commit this binary was built from; empty when git was unavailable.
const BUILT_FROM: &str = env!("MAY_GIT_SHA");

/// The package axoupdater looks the install receipt of up.
const RELEASE_APP: &str = "may-agent";

/// The GitHub REST API the dev path reads CI from.
const GITHUB_API: &str = "https://api.github.com";

/// Where a build gets its updates from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// A build from a commit: updates from the latest CI artifact on `main`.
    Dev,
    /// A cargo-dist release build: updates from GitHub releases.
    Release,
}

impl Channel {
    /// The channel of the running binary.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(feature = "release-build") {
            Self::Release
        } else {
            Self::Dev
        }
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
    /// CI builds no debug `may` for this platform.
    #[error("no CI build of May exists for this platform")]
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
    /// The downloaded artifact is not the one GitHub described.
    #[error("the downloaded artifact has sha256 {actual}, GitHub reports {expected}")]
    DigestMismatch {
        /// The digest GitHub reports.
        expected: String,
        /// The digest of the download.
        actual: String,
    },
    /// The artifact lacks an expected file.
    #[error("the artifact has no {0}")]
    MissingMember(&'static str),
    /// This release binary was not installed by the cargo-dist installer, so
    /// there is no install to update.
    #[error(
        "this May was not installed by the release installer, so it cannot update itself; \
         reinstall it from the latest GitHub release"
    )]
    NotInstalledByInstaller,
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
    /// The release updater failed.
    #[error("release update failed: {0}")]
    Release(Box<axoupdater::errors::AxoupdateError>),
}

impl From<axoupdater::errors::AxoupdateError> for UpdateError {
    /// Boxed: the updater's error is far larger than every other variant.
    fn from(error: axoupdater::errors::AxoupdateError) -> Self {
        Self::Release(Box::new(error))
    }
}

/// Updates the running binary on its channel. `progress` receives one line
/// per step, for the caller's loading indicator.
///
/// # Errors
/// Fails when the newest build cannot be found, downloaded or installed.
pub async fn update(
    channel: Channel,
    mut progress: impl FnMut(&str),
) -> Result<Outcome, UpdateError> {
    match channel {
        Channel::Dev => update_dev(&mut progress).await,
        Channel::Release => update_release(&mut progress).await,
    }
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
    mut progress: impl FnMut(&str),
) -> Result<Vec<u8>, UpdateError> {
    let commit = built_from().ok_or(UpdateError::UnknownCommit)?;
    this_commit_from(GITHUB_API, platform, commit, token, &mut progress).await
}

async fn this_commit_from(
    api_base: &str,
    platform: CiPlatform,
    commit: &str,
    token: String,
    progress: &mut impl FnMut(&str),
) -> Result<Vec<u8>, UpdateError> {
    let name = platform.artifact();
    let github = ci::GitHub::new(api_base, token);
    progress(&format!("Looking for the CI build of {}…", short(commit)));
    let found = github.artifact_of_commit(name, commit).await?;
    progress(&format!(
        "Downloading {name} ({} MB)…",
        found.artifact.size_in_bytes / 1_000_000
    ));
    let zip = github.download(&found.artifact).await?;
    blocking::unblock(move || ci::extract_tarball(&zip)).await
}

async fn update_dev(progress: &mut impl FnMut(&str)) -> Result<Outcome, UpdateError> {
    let name = CiPlatform::current()?.artifact();
    let token = GhCliTokenSource
        .token()
        .await
        .map_err(|error| UpdateError::Token(error.to_string()))?;
    let github = ci::GitHub::new(GITHUB_API, token);
    progress("Looking for the latest successful CI build…");
    let found = github.latest_artifact(name).await?;
    if !BUILT_FROM.is_empty() && found.head_sha == BUILT_FROM {
        return Ok(Outcome::UpToDate);
    }
    progress(&format!(
        "Downloading {name} ({} MB)…",
        found.artifact.size_in_bytes / 1_000_000
    ));
    let zip = github.download(&found.artifact).await?;
    let executable = blocking::unblock(move || {
        let bytes = ci::extract_executable(&zip)?;
        let target = dunce::canonicalize(std::env::current_exe()?)?;
        ci::replace_executable(&target, &bytes)?;
        Ok::<_, UpdateError>(target)
    })
    .await?;
    Ok(Outcome::Updated {
        executable,
        to: format!("the CI build of {}", short(&found.head_sha)),
    })
}

/// The leading characters of a commit id.
fn short(sha: &str) -> &str {
    sha.get(..sha.len().min(9)).unwrap_or(sha)
}

async fn update_release(progress: &mut impl FnMut(&str)) -> Result<Outcome, UpdateError> {
    let mut updater = axoupdater::AxoUpdater::new_for(RELEASE_APP);
    updater.load_receipt()?;
    // An executable the receipt is not for is not an install the updater may
    // touch; `is_update_needed` would report it as current.
    if !updater.check_receipt_is_for_this_executable()? {
        return Err(UpdateError::NotInstalledByInstaller);
    }
    if let Ok(token) = GhCliTokenSource.token().await {
        updater.set_github_token(&token);
    }
    updater.disable_installer_output();
    progress("Looking for the latest release…");
    if !updater.is_update_needed().await? {
        return Ok(Outcome::UpToDate);
    }
    progress("Installing the latest release…");
    let executable = dunce::canonicalize(std::env::current_exe()?)?;
    match updater.run().await? {
        Some(result) => Ok(Outcome::Updated {
            executable,
            to: format!("release {}", result.new_version),
        }),
        None => Ok(Outcome::UpToDate),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_without_the_release_feature_is_a_dev_build() {
        assert_eq!(
            Channel::current(),
            if cfg!(feature = "release-build") {
                Channel::Release
            } else {
                Channel::Dev
            }
        );
    }

    #[test]
    fn a_commit_id_is_shortened_on_a_char_boundary() {
        assert_eq!(
            short("6bbeeed53be58e7e1be3ecde6a781dcc5a5c6711"),
            "6bbeeed53"
        );
        assert_eq!(short("abc"), "abc");
    }
}
