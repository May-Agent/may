//! Startup validation of May's configuration files.
//!
//! Every entry point runs [`validate_configuration`] before doing anything
//! else, so an invalid file refuses startup instead of surfacing later on a
//! path that happens to load it. Each file is loaded through its own loader;
//! a missing file is not an error. A new configuration file joins the check
//! by implementing [`StartupConfig`] and adding one line to
//! [`validate_configuration_in`]. Per-profile files are checked in
//! `check_profile_workspaces`.

use std::error::Error;
use std::path::{Path, PathBuf};

use crate::config::MayConfig;
use crate::db::ChatDb;
use crate::mcp::McpConfig;
use crate::session::MayHomePaths;
use crate::workspace::WorkspaceConfig;

/// A configuration file validated at startup through its existing loader.
pub trait StartupConfig: Sized {
    /// Error returned by the loader.
    type Error: Error + Send + Sync + 'static;

    /// Location of the file under the May home directory.
    fn path(may_dir: &Path) -> PathBuf;

    /// Loads the file at `path`; an absent file loads as the default.
    ///
    /// # Errors
    /// Returns the loader's error when the file cannot be read, parsed or
    /// validated.
    fn load(path: &Path) -> impl Future<Output = Result<Self, Self::Error>>;
}

impl StartupConfig for MayConfig {
    type Error = crate::config::ConfigError;

    fn path(may_dir: &Path) -> PathBuf {
        may_dir.join("config.toml")
    }

    fn load(path: &Path) -> impl Future<Output = Result<Self, Self::Error>> {
        Self::load_from(path)
    }
}

impl StartupConfig for McpConfig {
    type Error = crate::mcp::McpConfigError;

    fn path(may_dir: &Path) -> PathBuf {
        may_dir.join("mcp.json")
    }

    fn load(path: &Path) -> impl Future<Output = Result<Self, Self::Error>> {
        Self::load_from(path)
    }
}

impl StartupConfig for WorkspaceConfig {
    type Error = crate::workspace::WorkspaceConfigError;

    fn path(may_dir: &Path) -> PathBuf {
        MayHomePaths::new(may_dir.to_path_buf()).workspace_config()
    }

    fn load(path: &Path) -> impl Future<Output = Result<Self, Self::Error>> {
        std::future::ready(Self::load(path))
    }
}

/// A configuration file that made startup refuse.
#[derive(Debug, thiserror::Error)]
#[error("refusing to start: invalid configuration file {}: {source}", path.display())]
pub struct StartupConfigError {
    /// The invalid file.
    pub path: PathBuf,
    /// The loader's error.
    #[source]
    pub source: Box<dyn Error + Send + Sync>,
}

/// Failure of [`validate_configuration`].
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    /// No home directory to find May's files in.
    #[error("refusing to start: {0}")]
    MayDir(#[from] crate::config::ConfigError),
    /// The profiles that own per-profile configuration files cannot be listed.
    #[error("refusing to start: cannot list profiles to validate their configuration: {0}")]
    Profiles(#[from] crate::db::ChatDbError),
    /// A configuration file is invalid.
    #[error(transparent)]
    Config(#[from] StartupConfigError),
}

async fn check<C: StartupConfig>(path: PathBuf) -> Result<(), StartupConfigError> {
    match C::load(&path).await {
        Ok(_) => Ok(()),
        Err(error) => Err(StartupConfigError {
            path,
            source: Box::new(error),
        }),
    }
}

/// Validates each profile's `workspace.toml`; profiles are listed the way May
/// lists them, from the history database. No database means no profiles.
async fn check_profile_workspaces(may_dir: &Path) -> Result<(), StartupError> {
    let home = MayHomePaths::new(may_dir.to_path_buf());
    if async_fs::metadata(home.db_file()).await.is_err() {
        return Ok(());
    }
    let db = ChatDb::open(&home.db_file()).await?;
    for profile in db.list_profiles().await? {
        check::<WorkspaceConfig>(home.profile_home(&profile.id).workspace_config()).await?;
    }
    Ok(())
}

/// Validates every configuration file under `may_dir` through its loader.
///
/// # Errors
/// Returns [`StartupError`] naming the first invalid file and its problem.
pub async fn validate_configuration_in(may_dir: &Path) -> Result<(), StartupError> {
    futures_util::try_join!(
        async { Ok(check::<MayConfig>(MayConfig::path(may_dir)).await?) },
        async { Ok(check::<McpConfig>(McpConfig::path(may_dir)).await?) },
        async { Ok(check::<WorkspaceConfig>(WorkspaceConfig::path(may_dir)).await?) },
        check_profile_workspaces(may_dir),
    )?;
    Ok(())
}

/// Validates every configuration file under `~/.may`.
///
/// # Errors
/// Returns [`StartupError`] when the home directory is unknown or a file is
/// invalid.
pub async fn validate_configuration() -> Result<(), StartupError> {
    let may_dir = MayConfig::may_dir()?;
    validate_configuration_in(&may_dir).await
}
