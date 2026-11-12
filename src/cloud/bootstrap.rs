//! Bootstrapping a cloud home: May installed on the machine, the fronting
//! daemon's record written, the project cloned when the machine was not
//! created from it, and the machine's daemon started.
//!
//! Every step runs through a [`RemoteShell`] — the provider's command
//! channel, `gh codespace ssh` for a codespace — and is idempotent, so the
//! same bootstrap runs on creation and on every wake. The build installed is
//! the fronting daemon's own: a dev build gets the CI artifact of its commit
//! (never a build on the machine), a release the same release through its
//! installer.

use std::future::Future;
use std::time::Duration;

use askama::Template;

use super::CloudError;
use super::home::{CloudHome, FILE as HOME_FILE};
use crate::update::{Channel, CiPlatform};
use crate::workspace::GitHubRepository;

/// Where May lives on a cloud home, under `$HOME`: the executable in its
/// `bin/`, and the identity of the installed build in `installed`.
pub const MAY_DIR: &str = ".may-cloud";

/// The repository cargo-dist releases are published from.
const RELEASE_REPOSITORY: &str = "May-Agent/may";

/// The cargo-dist installer of a release (`dist-workspace.toml`'s shell
/// installer, named after the package).
const RELEASE_INSTALLER: &str = "may-agent-installer.sh";

/// Where a codespace keeps its repositories (GitHub's default workspace
/// folder is `/workspaces/<repository>`).
const WORKSPACES: &str = "/workspaces";

/// How long a step that moves no build may take: a probe, a record write,
/// a clone's start. GitHub publishes no figure for the SSH relay; this is
/// the bootstrap window flyco gives a codespace's own `postStart`.
const STEP_DEADLINE: Duration = Duration::from_secs(180);

/// How long installing a build may take. A debug build's artifact is about
/// 100 MB (`src/update/testdata/ci_artifacts.json` records 96 MB); ten
/// minutes carries it at 160 KB/s.
const INSTALL_DEADLINE: Duration = Duration::from_secs(600);

/// A command channel to a machine: runs a `sh` script with optional input
/// on standard input, answering with its standard output.
pub trait RemoteShell: Send + Sync {
    /// Runs `script`, killed when `deadline` passes.
    ///
    /// # Errors
    /// [`CloudError::Remote`] when the script cannot be run, fails, or
    /// outlives its deadline.
    fn run(
        &self,
        step: &'static str,
        script: &str,
        input: Option<Vec<u8>>,
        deadline: Duration,
    ) -> impl Future<Output = Result<String, CloudError>> + Send;
}

/// A step of a bootstrap, for the caller's loading indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapStep {
    /// Checking which build the machine has.
    Checking,
    /// Downloading the build the machine needs.
    Downloading,
    /// Installing it on the machine.
    Installing,
    /// Cloning the session's repository.
    Cloning,
    /// Starting the machine's daemon.
    StartingDaemon,
}

/// The build a cloud home gets: the fronting daemon's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MayBuild {
    /// A dev build: CI's Linux artifact of `commit`.
    Ci {
        /// The commit the fronting daemon was built from.
        commit: String,
    },
    /// A release: the same version's installer.
    Release {
        /// The release version.
        version: String,
    },
}

impl MayBuild {
    /// The build of this binary.
    ///
    /// # Errors
    /// [`CloudError::Install`] for a dev build without its commit id, which
    /// no CI artifact can be found for.
    pub fn this_build() -> Result<Self, CloudError> {
        match Channel::current() {
            Channel::Release => Ok(Self::Release {
                version: env!("CARGO_PKG_VERSION").to_owned(),
            }),
            Channel::Dev => crate::update::built_from()
                .map(|commit| Self::Ci {
                    commit: commit.to_owned(),
                })
                .ok_or_else(|| {
                    CloudError::Install(crate::update::UpdateError::UnknownCommit.to_string())
                }),
        }
    }

    /// What the machine records as installed.
    fn identity(&self) -> String {
        match self {
            Self::Ci { commit } => format!("ci:{commit}"),
            Self::Release { version } => format!("release:{version}"),
        }
    }
}

/// What a bootstrap brings a machine to.
#[derive(Debug, Clone)]
pub struct Bootstrap<'a> {
    /// The build to install.
    pub build: &'a MayBuild,
    /// The record the machine's daemon serves the fronting daemon with.
    pub home: &'a CloudHome,
    /// The repository to clone, when the machine was not created from it.
    pub clone: Option<&'a GitHubRepository>,
    /// The GitHub token CI artifacts are downloaded and a clone is made
    /// with; it never stays on the machine.
    pub github_token: &'a str,
}

impl Bootstrap<'_> {
    /// Runs the bootstrap over `shell`, reporting each step to `progress`.
    ///
    /// # Errors
    /// Fails when a step fails on the machine, or the build cannot be
    /// downloaded.
    ///
    /// # Panics
    /// Never: every script renders and the record always serializes.
    pub async fn run(
        &self,
        shell: &impl RemoteShell,
        mut progress: impl FnMut(BootstrapStep) + Send,
    ) -> Result<(), CloudError> {
        let identity = self.build.identity();
        progress(BootstrapStep::Checking);
        let installed = shell
            .run(
                "check the installed May",
                &Probe { dir: MAY_DIR }.render_script(),
                None,
                STEP_DEADLINE,
            )
            .await?;
        if installed.trim() != identity {
            self.install(shell, &identity, &mut progress).await?;
        }
        if let Some(project) = self.clone {
            progress(BootstrapStep::Cloning);
            let script = CloneProject {
                directory: quote(&workspace_of(project)),
                url: quote(&format!("https://github.com/{project}.git")),
            }
            .render_script();
            let mut token = self.github_token.as_bytes().to_vec();
            token.push(b'\n');
            shell
                .run(
                    "clone the repository",
                    &script,
                    Some(token),
                    INSTALL_DEADLINE,
                )
                .await?;
        }
        let record = serde_json::to_vec(self.home).expect("a cloud home always serializes");
        shell
            .run(
                "write the cloud home record",
                &WriteHome { file: HOME_FILE }.render_script(),
                Some(record),
                STEP_DEADLINE,
            )
            .await?;
        progress(BootstrapStep::StartingDaemon);
        shell
            .run(
                "start the May daemon",
                &StartDaemon { dir: MAY_DIR }.render_script(),
                None,
                STEP_DEADLINE,
            )
            .await?;
        Ok(())
    }

    async fn install(
        &self,
        shell: &impl RemoteShell,
        identity: &str,
        progress: &mut (impl FnMut(BootstrapStep) + Send),
    ) -> Result<(), CloudError> {
        match self.build {
            MayBuild::Ci { .. } => {
                progress(BootstrapStep::Downloading);
                let tarball = crate::update::this_build_tarball_for(
                    CiPlatform::LinuxX64,
                    self.github_token.to_owned(),
                    |line| tracing::info!(step = line, "cloud home build"),
                )
                .await
                .map_err(|error| CloudError::Install(error.to_string()))?;
                progress(BootstrapStep::Installing);
                let script = InstallCi {
                    dir: MAY_DIR,
                    identity: quote(identity),
                }
                .render_script();
                shell
                    .run("install May", &script, Some(tarball), INSTALL_DEADLINE)
                    .await?;
            }
            MayBuild::Release { version } => {
                progress(BootstrapStep::Installing);
                let script = InstallRelease {
                    dir: MAY_DIR,
                    installer: quote(&format!(
                        "https://github.com/{RELEASE_REPOSITORY}/releases/download/v{version}/{RELEASE_INSTALLER}"
                    )),
                    identity: quote(identity),
                }
                .render_script();
                shell
                    .run("install May", &script, None, INSTALL_DEADLINE)
                    .await?;
            }
        }
        Ok(())
    }
}

/// Where a codespace keeps `repository`'s checkout.
#[must_use]
pub fn workspace_of(repository: &GitHubRepository) -> String {
    format!("{WORKSPACES}/{}", repository.repo())
}

/// `value` as one `sh` word.
fn quote(value: &str) -> String {
    shell_words::quote(value).into_owned()
}

/// A bootstrap script, rendered.
trait Script: Template {
    fn render_script(&self) -> String {
        self.render().expect("a bootstrap script always renders")
    }
}

impl<T: Template> Script for T {}

#[derive(Template)]
#[template(path = "cloud_probe.sh", escape = "none")]
struct Probe {
    dir: &'static str,
}

#[derive(Template)]
#[template(path = "cloud_install_ci.sh", escape = "none")]
struct InstallCi {
    dir: &'static str,
    identity: String,
}

#[derive(Template)]
#[template(path = "cloud_install_release.sh", escape = "none")]
struct InstallRelease {
    dir: &'static str,
    installer: String,
    identity: String,
}

#[derive(Template)]
#[template(path = "cloud_write_home.sh", escape = "none")]
struct WriteHome {
    file: &'static str,
}

#[derive(Template)]
#[template(path = "cloud_start_daemon.sh", escape = "none")]
struct StartDaemon {
    dir: &'static str,
}

#[derive(Template)]
#[template(path = "cloud_clone_project.sh", escape = "none")]
struct CloneProject {
    directory: String,
    url: String,
}

/// The command a devcontainer runs at every start: the daemon, once May
/// is installed.
#[derive(Debug, Template)]
#[template(path = "cloud_post_start.sh", escape = "none")]
pub struct PostStart {
    dir: &'static str,
}

impl Default for PostStart {
    fn default() -> Self {
        Self { dir: MAY_DIR }
    }
}

#[cfg(test)]
#[path = "bootstrap_tests.rs"]
mod tests;
