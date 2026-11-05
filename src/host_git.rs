//! The one way git runs on the host.
//!
//! Host git works on the user's checkouts, on scratch repositories and on
//! nothing the agent wrote. Where its configuration comes from is the one
//! thing a caller varies: the host's own (`inherit`), or a file standing in
//! for the user's global configuration (`from_file`), which is how tests give
//! each fixture a hermetic user with their own hooks and signing key.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A host git invocation that did not succeed.
#[derive(Debug, thiserror::Error)]
pub enum HostGitError {
    /// `git` could not run.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// `git` exited nonzero.
    #[error("git {args} failed: {stderr}")]
    Failed {
        /// The command line.
        args: String,
        /// Captured stderr.
        stderr: String,
    },
}

/// Where the host's git configuration is read from.
#[derive(Debug, Clone, Default)]
pub struct HostGit {
    /// A file standing in for the user's global configuration, with the
    /// system one ignored; `None` reads the host's own.
    global_config: Option<PathBuf>,
}

impl HostGit {
    /// The host's own git configuration.
    #[must_use]
    pub const fn inherit() -> Self {
        Self {
            global_config: None,
        }
    }

    /// `config` standing in for the user's global git configuration, with no
    /// system configuration.
    #[must_use]
    pub fn from_file(config: impl Into<PathBuf>) -> Self {
        Self {
            global_config: Some(config.into()),
        }
    }

    /// A host `git`, started outside every repository.
    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = Command::new("git");
        command.current_dir("/");
        for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
            command.env_remove(var);
        }
        if let Some(config) = &self.global_config {
            command
                .env("GIT_CONFIG_GLOBAL", config)
                .env("GIT_CONFIG_SYSTEM", "/dev/null");
        }
        command
    }

    /// Runs `git` (with `-C dir` when given) and returns its stdout.
    ///
    /// # Errors
    /// Fails when git cannot run or exits nonzero.
    pub fn output(&self, dir: Option<&Path>, args: &[&str]) -> Result<String, HostGitError> {
        let mut command = self.command();
        if let Some(dir) = dir {
            command.arg("-C").arg(dir);
        }
        let output = command.args(args).output()?;
        if !output.status.success() {
            return Err(HostGitError::Failed {
                args: format!("{command:?}"),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// The boolean `key` in the host's configuration the way git reads it
    /// (`true`, `yes`, `on`, `1`, ...), `None` when unset.
    ///
    /// # Errors
    /// Fails when git cannot read its configuration or the value is not a
    /// boolean.
    pub fn config_bool(&self, key: &str) -> Result<Option<bool>, HostGitError> {
        let output = self
            .command()
            .args(["config", "--type=bool", "--get", key])
            .output()?;
        match output.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim() == "true",
            )),
            Some(1) => Ok(None),
            _ => Err(HostGitError::Failed {
                args: format!("config --type=bool --get {key}"),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
        }
    }

    /// The value of `key` in the host's configuration, `None` when unset.
    ///
    /// # Errors
    /// Fails when git cannot read its configuration.
    pub fn config_value(&self, key: &str) -> Result<Option<String>, HostGitError> {
        let output = self.command().args(["config", "--get", key]).output()?;
        match output.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            )),
            Some(1) => Ok(None),
            _ => Err(HostGitError::Failed {
                args: format!("config --get {key}"),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
        }
    }
}
