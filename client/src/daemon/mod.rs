//! The client-side view of the daemon: its wire API and lifecycle.

pub mod forge_auth;
pub mod lifecycle;
pub mod tool_summary;
/// The daemon's request and response wire types.
pub mod types;

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use may_config::config::MayConfig;

/// Files the daemon keeps under `~/.may`.
#[derive(Debug, Clone)]
pub struct DaemonPaths {
    root: PathBuf,
    socket: PathBuf,
}

impl DaemonPaths {
    /// Paths under `may_dir`.
    #[must_use]
    pub fn new(may_dir: PathBuf) -> Self {
        Self {
            socket: may_dir.join("daemon.sock"),
            root: may_dir,
        }
    }

    /// Paths under the user's `~/.may`.
    ///
    /// # Errors
    /// Fails when the home directory cannot be determined or resolved.
    pub fn user() -> anyhow::Result<Self> {
        Self::for_home(dirs::home_dir().context("no home directory")?)
    }

    /// Paths under `home`'s `.may`. Files live at the resolved home, which
    /// is what the sandbox grants; the socket keeps `home`'s own spelling,
    /// since a unix socket path is limited to about 104 bytes and the
    /// resolved spelling of a home reached through a link can exceed it.
    #[doc(hidden)]
    pub fn for_home(home: PathBuf) -> anyhow::Result<Self> {
        let socket = home.join(".may").join("daemon.sock");
        let root = MayConfig::resolve_home(home)
            .context("failed to resolve ~/.may")?
            .join(".may");
        Ok(Self { root, socket })
    }

    /// The unix socket clients connect to.
    #[must_use]
    pub fn socket(&self) -> PathBuf {
        self.socket.clone()
    }

    /// The single-instance lock.
    #[must_use]
    pub fn lock(&self) -> PathBuf {
        self.root.join("daemon.lock")
    }

    /// The log a spawned daemon writes to.
    #[must_use]
    pub fn log(&self) -> PathBuf {
        self.root.join("logs").join("daemon.log")
    }

    /// The `~/.may` directory itself.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Identity of an executable build: device, inode, size and mtime of the
/// file. A client whose own build differs from the running daemon's
/// replaces the daemon, so a rebuilt `may` never talks to a stale one.
///
/// # Errors
/// Fails when the file's metadata cannot be read.
#[allow(
    clippy::disallowed_methods,
    reason = "synchronous stat of local executable binary"
)]
pub fn build_id_of(path: &Path) -> std::io::Result<String> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::metadata(path)?;
    Ok(format!(
        "{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.size(),
        metadata.mtime_nsec() + metadata.mtime() * 1_000_000_000
    ))
}

/// Build id of the running executable.
///
/// # Errors
/// Fails when the executable path or its metadata cannot be read.
pub fn current_build_id() -> std::io::Result<String> {
    build_id_of(&dunce::canonicalize(std::env::current_exe()?)?)
}
