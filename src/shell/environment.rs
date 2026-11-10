//! What a session's sandbox on the host is given: the environment its
//! commands start with, the directories its toolchains live in and the paths
//! it must not write.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use heel::{Access, Grant, ResolvedPath};

use crate::tools::{PreparedTools, ToolRuntimeError, UV_PYTHON_PREFERENCE};

/// Why a session's environment could not be prepared.
#[derive(Debug, thiserror::Error)]
pub enum SessionEnvironmentError {
    /// A directory could not be created or the git shim written.
    #[error("preparing the session environment failed: {0}")]
    Io(#[from] std::io::Error),
    /// A path of the environment is not UTF-8.
    #[error("the session environment path for {name} is not valid UTF-8: {path}")]
    NotUtf8 {
        /// The variable the path is for.
        name: &'static str,
        /// The path.
        path: String,
    },
    /// The managed tools directory cannot lead the `PATH`.
    #[error(transparent)]
    Tools(#[from] ToolRuntimeError),
    /// A grant's path cannot be resolved.
    #[error("a sandbox grant cannot be resolved: {0}")]
    Grant(#[from] heel::Error),
}

/// What a session's host sandbox is built from.
#[derive(Debug, Clone)]
pub struct HostSession {
    /// The directories the sandbox grants every process.
    pub grants: Vec<Grant>,
    /// The environment every process starts with.
    pub environment: Vec<(String, String)>,
    /// The paths no process may write.
    pub write_denylist: heel::WriteDenylist,
}

impl HostSession {
    /// The session of `tools` in `working_dir`: creates the session's
    /// temporary, cache and config directories and the managed `git` shim.
    ///
    /// # Errors
    /// Fails when a directory cannot be created, a path is not UTF-8 or a
    /// grant cannot be resolved.
    pub async fn prepare(
        working_dir: &Path,
        tools: &PreparedTools<PathBuf>,
    ) -> Result<Self, SessionEnvironmentError> {
        write_git_shim(tools).await?;
        let environment = environment(working_dir, tools).await?;
        let mut grants = tool_grants(tools)?;
        grants.extend(path_directory_grants(&environment)?);
        Ok(Self {
            grants,
            environment,
            write_denylist: write_denylist(working_dir, tools),
        })
    }
}

/// The object `path` names, in the spelling a resolved grant checks: a
/// symlinked component or an 8.3 alias would not name the same object.
fn resolved(path: &Path) -> heel::Result<ResolvedPath> {
    ResolvedPath::new(dunce::simplified(
        &std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
    ))
}

fn path_value(name: &'static str, path: &Path) -> Result<String, SessionEnvironmentError> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| SessionEnvironmentError::NotUtf8 {
            name,
            path: path.display().to_string(),
        })
}

/// Host toolchain homes a local sandbox reaches: (env var, resolved existing
/// dir). `CARGO_HOME` and `RUSTUP_HOME` fall back to their `~/.cargo` /
/// `~/.rustup` defaults; only directories that exist yield, so a toolchain
/// absent from the host is never granted.
fn host_toolchain_homes() -> impl Iterator<Item = (&'static str, PathBuf)> {
    let home = std::env::home_dir;
    [
        std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|home| home.join(".cargo")))
            .map(|path| ("CARGO_HOME", path)),
        std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|home| home.join(".rustup")))
            .map(|path| ("RUSTUP_HOME", path)),
    ]
    .into_iter()
    .flatten()
    .filter(|(_, path)| path.is_dir())
}

/// The grants the managed tools need: their directory, executable; the shared
/// `uv` directory, which `uv` writes and the interpreters it installs
/// execute; and the host toolchain homes, writable and executable since
/// builds run the binaries and write the caches. Credential files inside them
/// stay denied by heel's security tables, which emit after grants.
fn tool_grants(tools: &PreparedTools<PathBuf>) -> heel::Result<Vec<Grant>> {
    std::iter::once(Ok(Grant::new(resolved(tools.bin_dir())?, Access::EXEC)))
        .chain(
            tools
                .uv_dir()
                .map(|path| Ok(Grant::new(resolved(path)?, Access::WRITE | Access::EXEC))),
        )
        .chain(
            host_toolchain_homes()
                .map(|(_, path)| Ok(Grant::new(resolved(&path)?, Access::WRITE | Access::EXEC))),
        )
        .collect()
}

/// The grants that let a command run what the session's `PATH` lists: each
/// existing absolute directory of the `PATH` in `environment`, read and
/// execute, never write.
fn path_directory_grants(environment: &[(String, String)]) -> heel::Result<Vec<Grant>> {
    let Some((_, path)) = environment.iter().find(|(name, _)| name == "PATH") else {
        return Ok(Vec::new());
    };
    let mut seen = HashSet::new();
    let mut grants = Vec::new();
    for directory in std::env::split_paths(path) {
        if !directory.is_absolute() || !directory.is_dir() {
            continue;
        }
        let resolved = resolved(&directory)?;
        if seen.insert(resolved.as_path().to_path_buf()) {
            grants.push(Grant::new(resolved, Access::READ | Access::EXEC));
        }
    }
    Ok(grants)
}

/// The paths a session sandbox must not write: heel's persistence table (git
/// hooks and config, shell startup files, `PATH` directories and the rest),
/// where a write runs as code outside the sandbox later.
///
/// Three places are exempt, because the table forbids creating a `.git` at
/// all: cargo's git cache, `$CARGO_HOME/git`, where cargo clones git
/// dependencies; the session directory `working_dir`, where `git clone` and
/// `git init` make the session's own repositories; and the prepared
/// runtime's `uv` cache, whose buckets `uv` marks with a `.git` entry.
fn write_denylist(working_dir: &Path, tools: &PreparedTools<PathBuf>) -> heel::WriteDenylist {
    let resolved = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let denylist = heel::WriteDenylist::persistence().except(resolved(working_dir));
    let denylist = match host_toolchain_homes().find(|(name, _)| *name == "CARGO_HOME") {
        Some((_, cargo_home)) => denylist.except(resolved(&cargo_home).join("git")),
        None => denylist,
    };
    match tools.uv_environment() {
        Some(uv) => denylist.except(resolved(&uv.cache)),
        None => denylist,
    }
}

/// The environment of a session's commands on the host: temporary, cache and
/// config directories inside the session, the managed tools ahead of the
/// host's `PATH`, and the toolchain homes the grants cover.
async fn environment(
    working_dir: &Path,
    tools: &PreparedTools<PathBuf>,
) -> Result<Vec<(String, String)>, SessionEnvironmentError> {
    let session_tmp = working_dir.join("tmp");
    let session_cache = working_dir.join(".cache");
    let bun_cache = session_cache.join("bun");
    let session_config = working_dir.join(".config");
    let playwright_cache = session_cache.join("ms-playwright");
    let python_path = working_dir.join("skills").join("python");
    for directory in [
        &session_tmp,
        &session_cache,
        &bun_cache,
        &session_config,
        &playwright_cache,
    ] {
        async_fs::create_dir_all(directory).await?;
    }
    let tmp = path_value("TMPDIR", &session_tmp)?;
    let python = match std::env::var_os("PYTHONPATH") {
        None => python_path.into_os_string(),
        Some(existing) => std::env::join_paths(
            std::iter::once(python_path).chain(std::env::split_paths(&existing)),
        )
        .map_err(|error| std::io::Error::other(format!("session PYTHONPATH: {error}")))?,
    };
    let python = python
        .into_string()
        .map_err(|value| SessionEnvironmentError::NotUtf8 {
            name: "PYTHONPATH",
            path: value.to_string_lossy().into_owned(),
        })?;
    // The managed tools directory leads; the host PATH follows so host
    // toolchains stay reachable. The heel sandbox path is only a fallback for
    // a host without a PATH.
    let host_path = std::env::var_os("PATH")
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| heel::default_sandbox_path().into());
    let path = tools
        .prepend_path(Some(host_path.as_os_str()))?
        .into_string()
        .map_err(|_| ToolRuntimeError::InvalidCache(tools.bin_dir().clone()))?;
    let mut env = vec![
        ("PATH".to_string(), path),
        ("TMPDIR".to_string(), tmp.clone()),
        ("TMP".to_string(), tmp.clone()),
        ("TEMP".to_string(), tmp),
        ("HOME".to_string(), path_value("HOME", working_dir)?),
        (
            "XDG_CACHE_HOME".to_string(),
            path_value("XDG_CACHE_HOME", &session_cache)?,
        ),
        (
            "XDG_CONFIG_HOME".to_string(),
            path_value("XDG_CONFIG_HOME", &session_config)?,
        ),
        (
            "BUN_INSTALL_CACHE_DIR".to_string(),
            path_value("BUN_INSTALL_CACHE_DIR", &bun_cache)?,
        ),
        (
            "PLAYWRIGHT_BROWSERS_PATH".to_string(),
            path_value("PLAYWRIGHT_BROWSERS_PATH", &playwright_cache)?,
        ),
        ("PYTHONPATH".to_string(), python),
    ];
    // `uv` keeps its cache and the Pythons it installs for projects in one
    // directory shared by every session, not under the remapped HOME.
    if let Some(uv) = tools.uv_environment() {
        env.push((
            "UV_CACHE_DIR".to_string(),
            path_value("UV_CACHE_DIR", &uv.cache)?,
        ));
        env.push((
            "UV_PYTHON_INSTALL_DIR".to_string(),
            path_value("UV_PYTHON_INSTALL_DIR", &uv.python_installs)?,
        ));
        env.push((
            "UV_PYTHON_PREFERENCE".to_string(),
            UV_PYTHON_PREFERENCE.to_string(),
        ));
    }
    // Point cargo and rustup at the same host homes the grants cover, so the
    // remapped HOME does not strand them under the working dir.
    for (name, home) in host_toolchain_homes() {
        env.push((name.to_string(), path_value(name, &home)?));
    }
    Ok(env)
}

/// The `git` shim every sandboxed command's PATH resolves first: a git run in
/// a committing worktree reads the worktree's host-owned entry instead of
/// whatever `.git` its subagent wrote.
const GIT_SHIM: &str = include_str!("../../templates/git_shim");

/// Writes the `git` shim into the prepared tools directory once.
#[cfg(unix)]
async fn write_git_shim(tools: &PreparedTools<PathBuf>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    use futures_lite::io::AsyncWriteExt as _;
    let shim = tools.bin_dir().join("git");
    match async_fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&shim)
        .await
    {
        Ok(mut file) => {
            file.write_all(GIT_SHIM.as_bytes()).await?;
            file.flush().await?;
            async_fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).await
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Windows commands run no `sh`: no shim.
#[cfg(not(unix))]
async fn write_git_shim(_tools: &PreparedTools<PathBuf>) -> std::io::Result<()> {
    Ok(())
}
