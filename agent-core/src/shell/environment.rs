//! What a session's sandbox on the host is given: the environment its
//! commands start with, the directories its toolchains live in and the paths
//! it must not write.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use heel::{Access, Grant, ResolvedPath};

use super::WritableDirs;
use super::python_shim;
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
        workspace_roots: &[PathBuf],
    ) -> Result<Self, SessionEnvironmentError> {
        write_git_shim(tools).await?;
        python_shim::install(&tools.shim_dir()).await?;
        let env_values = environment(working_dir, tools).await?;
        let working_dir = working_dir.to_path_buf();
        let tools = tools.clone();
        let workspace_roots = workspace_roots.to_vec();
        blocking::unblock(move || {
            let mut grants = tool_grants(&tools)?;
            grants.extend(path_directory_grants(&env_values)?);
            grants.extend(runtime_toolchain_grants(&working_dir, &workspace_roots)?);
            let write_denylist = write_denylist(&working_dir, &tools);
            Ok(Self {
                grants,
                environment: env_values,
                write_denylist,
            })
        })
        .await
    }
}

/// The object `path` names, in the spelling a resolved grant checks: a
/// symlinked component or an 8.3 alias would not name the same object.
#[allow(
    clippy::disallowed_methods,
    reason = "synchronous canonicalization runs inside blocking::unblock in HostSession::prepare"
)]
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

/// Host toolchain homes a local sandbox reaches: `(env var, resolved existing
/// dir)`. `CARGO_HOME` and `RUSTUP_HOME` fall back to their `~/.cargo` /
/// `~/.rustup` defaults, and when `stow` is present on the host its shared
/// artifact cache (`STOW_CACHE_DIR`, `~/.stow`) and daemon runtime directory
/// (`STOW_DAEMON_DIR`, `<cache-dir>/stow/run`) ride alongside them so `cargo`
/// wrappers inside the remapped-`HOME` sandbox reach the host `stow` daemon
/// and `build-dir`. Only directories that exist yield, so a toolchain absent
/// from the host is never granted.
#[allow(
    clippy::disallowed_methods,
    reason = "synchronous stow daemon setup runs inside blocking::unblock in environment()"
)]
fn host_toolchain_homes() -> impl Iterator<Item = (&'static str, PathBuf)> {
    let home = std::env::home_dir;
    let stow_cache = std::env::var_os("STOW_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".stow")))
        .filter(|path| path.is_dir());
    // On Linux each sandbox runs in a child PID namespace (`CLONE_NEWPID`), so
    // `stow-rustc-wrapper` reports a namespace-local `cargo` PID that only an
    // in-namespace `stow-daemon` can verify in `/proc` alongside `SO_PEERCRED`.
    // Linux sessions therefore get a `0700` `STOW_DAEMON_DIR` inside their own
    // cache while sharing the host's `STOW_CACHE_DIR`; macOS and Windows share
    // the host PID space and reach the host daemon directly.
    #[cfg(not(target_os = "linux"))]
    let stow_daemon = std::env::var_os("STOW_DAEMON_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            stow_cache
                .as_ref()
                .and_then(|_| dirs::cache_dir().map(|cache| cache.join("stow").join("run")))
        })
        .inspect(|path| {
            if stow_cache.is_some() {
                let _ = std::fs::create_dir_all(path);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
                }
            }
        })
        .filter(|path| path.is_dir());
    #[cfg(target_os = "linux")]
    let stow_daemon: Option<PathBuf> = None;
    [
        std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|h| h.join(".cargo")))
            .filter(|path| path.is_dir())
            .map(|path| ("CARGO_HOME", path)),
        std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|h| h.join(".rustup")))
            .filter(|path| path.is_dir())
            .map(|path| ("RUSTUP_HOME", path)),
        stow_cache.map(|path| ("STOW_CACHE_DIR", path)),
        stow_daemon.map(|path| ("STOW_DAEMON_DIR", path)),
    ]
    .into_iter()
    .flatten()
}

/// The grants the managed tools need: their directory, executable; the shared
/// `uv` directory, which `uv` writes and the interpreters it installs
/// execute; and the host toolchain homes, readable, writable and executable
/// since builds read configs/caches, connect to `stow`'s daemon socket, run
/// toolchain binaries and write caches. Credential files inside them stay
/// denied by heel's security tables, which emit after grants.
fn tool_grants(tools: &PreparedTools<PathBuf>) -> heel::Result<Vec<Grant>> {
    std::iter::once(Ok(Grant::new(resolved(tools.bin_dir())?, Access::EXEC)))
        .chain(
            tools
                .uv_dir()
                .map(|path| Ok(Grant::new(resolved(path)?, Access::WRITE | Access::EXEC))),
        )
        .chain(host_toolchain_homes().map(|(name, path)| {
            // `stow-rustc-wrapper` calls `create_dir_all(STOW_DAEMON_DIR)` on
            // startup, and Linux's seccomp `mkdir` broker checks write access
            // on the parent directory before returning `EEXIST`. When
            // `STOW_DAEMON_DIR` lives inside stow's own `<cache>/stow/run`
            // directory, granting `<cache>/stow` covers that parent check
            // without exposing sibling cache directories.
            let grant_root = if name == "STOW_DAEMON_DIR"
                && path
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|dir| dir == "stow")
            {
                path.parent().unwrap_or(&path)
            } else {
                path.as_path()
            };
            Ok(Grant::new(
                resolved(grant_root)?,
                Access::READ | Access::WRITE | Access::EXEC,
            ))
        }))
        .chain(
            dirs::data_local_dir()
                .map(|data| data.join("stow").join("tools"))
                .filter(|path| path.is_dir())
                .map(|path| Ok(Grant::new(resolved(&path)?, Access::READ | Access::EXEC))),
        )
        .collect()
}

/// Toolchain executables on `PATH` whose canonical symlink target may live in
/// a separate version-manager or alternatives tree (such as `uv`, `pyenv`,
/// `mise`, Homebrew, or `/etc/alternatives`).
const PATH_TOOLCHAIN_BINS: &[&str] = &[
    "python", "python3", "pytest", "cc", "c++", "gcc", "g++", "clang", "ld", "node", "bun",
    "cargo", "rustc", "uv",
];

/// The runtime prefix root containing an executable: `<prefix>` when the binary
/// sits inside `<prefix>/bin`, else its immediate parent directory.
fn runtime_root_for_executable(canonical_exe: &Path) -> Option<PathBuf> {
    let parent = canonical_exe.parent()?;
    if parent.file_name().is_some_and(|name| name == "bin")
        && let Some(grandparent) = parent.parent()
        && grandparent != Path::new("/")
        && grandparent != Path::new("/usr")
        && grandparent != Path::new("/usr/local")
    {
        return Some(grandparent.to_path_buf());
    }
    Some(parent.to_path_buf())
}

/// Parses `home = <path>` from a Python virtual environment's `pyvenv.cfg`.
#[allow(
    clippy::disallowed_methods,
    reason = "synchronous pyvenv.cfg inspection runs inside blocking::unblock in HostSession::prepare"
)]
fn pyvenv_home_dir(venv_dir: &Path) -> Option<PathBuf> {
    let cfg = std::fs::read_to_string(venv_dir.join("pyvenv.cfg")).ok()?;
    for line in cfg.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("home") {
            let dir = PathBuf::from(value.trim());
            if dir.is_absolute() && dir.is_dir() {
                return Some(dir);
            }
        }
    }
    None
}

/// Inserts a read/execute grant for `dir` when it exists and has not already
/// been added to `seen`.
#[allow(
    clippy::disallowed_methods,
    reason = "synchronous canonicalization runs inside blocking::unblock in HostSession::prepare"
)]
fn push_read_exec_grant(
    dir: &Path,
    seen: &mut HashSet<PathBuf>,
    grants: &mut Vec<Grant>,
) -> heel::Result<()> {
    if !dir.is_absolute() || !dir.is_dir() {
        return Ok(());
    }
    let resolved = resolved(dir)?;
    if seen.insert(resolved.as_path().to_path_buf()) {
        grants.push(Grant::new(resolved, Access::READ | Access::EXEC));
    }
    Ok(())
}

/// Collects read/execute grants for a Python virtual environment: the venv
/// itself, any canonical symlink targets of its `bin/python*` interpreters,
/// and the runtime root named by `pyvenv.cfg`'s `home` entry.
#[allow(
    clippy::disallowed_methods,
    reason = "synchronous venv inspection runs inside blocking::unblock in HostSession::prepare"
)]
fn collect_venv_grants(
    venv_dir: &Path,
    seen: &mut HashSet<PathBuf>,
    grants: &mut Vec<Grant>,
) -> heel::Result<()> {
    if !venv_dir.is_dir() {
        return Ok(());
    }
    push_read_exec_grant(venv_dir, seen, grants)?;
    for bin_name in ["python", "python3"] {
        let candidate = venv_dir.join("bin").join(bin_name);
        if let Ok(canonical) = std::fs::canonicalize(&candidate)
            && let Some(root) = runtime_root_for_executable(&canonical)
        {
            push_read_exec_grant(&root, seen, grants)?;
        }
    }
    if let Some(home_dir) = pyvenv_home_dir(venv_dir) {
        let canonical = std::fs::canonicalize(&home_dir).unwrap_or(home_dir);
        let root = if canonical.file_name().is_some_and(|name| name == "bin")
            && let Some(parent) = canonical.parent()
            && parent != Path::new("/")
            && parent != Path::new("/usr")
            && parent != Path::new("/usr/local")
        {
            parent.to_path_buf()
        } else {
            canonical
        };
        push_read_exec_grant(&root, seen, grants)?;
    }
    Ok(())
}

/// Read/execute grants for host read-only runtime directories (`~/.local/share/uv`,
/// `~/.pyenv`, `$VIRTUAL_ENV`) and canonical targets of existing workspace
/// virtual environments (`.venv` / `venv`). Never grants write access; heel's
/// credential denylist still applies on top.
#[allow(
    clippy::disallowed_methods,
    reason = "synchronous runtime discovery runs inside blocking::unblock in HostSession::prepare"
)]
fn runtime_toolchain_grants(
    working_dir: &Path,
    workspace_roots: &[PathBuf],
) -> heel::Result<Vec<Grant>> {
    let mut seen = HashSet::new();
    let mut grants = Vec::new();
    let home = std::env::home_dir();

    if let Some(data_local) = dirs::data_local_dir() {
        push_read_exec_grant(&data_local.join("uv"), &mut seen, &mut grants)?;
    }
    if let Some(home) = &home {
        push_read_exec_grant(
            &home.join(".local").join("share").join("uv"),
            &mut seen,
            &mut grants,
        )?;
        push_read_exec_grant(&home.join(".pyenv"), &mut seen, &mut grants)?;
    }
    if let Some(uv_pythons) = std::env::var_os("UV_PYTHON_INSTALL_DIR").map(PathBuf::from) {
        push_read_exec_grant(&uv_pythons, &mut seen, &mut grants)?;
    }
    if let Some(pyenv_root) = std::env::var_os("PYENV_ROOT").map(PathBuf::from) {
        push_read_exec_grant(&pyenv_root, &mut seen, &mut grants)?;
    }
    if let Some(virtual_env) = std::env::var_os("VIRTUAL_ENV").map(PathBuf::from) {
        collect_venv_grants(&virtual_env, &mut seen, &mut grants)?;
    }

    for root in std::iter::once(working_dir).chain(workspace_roots.iter().map(PathBuf::as_path)) {
        for venv_name in [".venv", "venv"] {
            collect_venv_grants(&root.join(venv_name), &mut seen, &mut grants)?;
        }
    }

    Ok(grants)
}

/// The grants that let a command run what the session's `PATH` lists: each
/// existing absolute directory of the `PATH` in `environment`, plus canonical
/// symlink targets of toolchain executables and parent `pyvenv.cfg` homes
/// inside those directories — read and execute, never write.
#[allow(
    clippy::disallowed_methods,
    reason = "synchronous PATH symlink resolution runs inside blocking::unblock in HostSession::prepare"
)]
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
        push_read_exec_grant(&directory, &mut seen, &mut grants)?;
        if directory.file_name().is_some_and(|name| name == "bin")
            && let Some(parent) = directory.parent()
            && parent.join("pyvenv.cfg").is_file()
        {
            collect_venv_grants(parent, &mut seen, &mut grants)?;
        }
        for bin_name in PATH_TOOLCHAIN_BINS {
            let candidate = directory.join(bin_name);
            if std::fs::symlink_metadata(&candidate).is_ok_and(|meta| meta.file_type().is_symlink())
                && let Ok(canonical) = std::fs::canonicalize(&candidate)
                && let Some(root) = runtime_root_for_executable(&canonical)
            {
                push_read_exec_grant(&root, &mut seen, &mut grants)?;
            }
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
#[allow(
    clippy::disallowed_methods,
    reason = "synchronous canonicalization runs inside blocking::unblock in HostSession::prepare"
)]
fn write_denylist(working_dir: &Path, tools: &PreparedTools<PathBuf>) -> heel::WriteDenylist {
    let resolved = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let denylist = heel::WriteDenylist::persistence().except(resolved(working_dir));
    let denylist = match host_toolchain_homes().find(|(name, _)| *name == "CARGO_HOME") {
        Some((_, cargo_home)) => denylist.except(resolved(&cargo_home).join("git")),
        None => denylist,
    };
    let denylist = match host_toolchain_homes().find(|(name, _)| *name == "STOW_CACHE_DIR") {
        Some((_, stow_cache)) => denylist.except(resolved(&stow_cache)),
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
    let writable = WritableDirs::under(working_dir);
    let playwright_cache = writable.cache().join("ms-playwright");
    let python_path = working_dir.join("skills").join("python");
    writable.create().await?;
    async_fs::create_dir_all(&playwright_cache).await?;
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
    let managed_path = tools.prepend_path(Some(host_path.as_os_str()))?;
    let path = std::env::join_paths(
        std::iter::once(tools.shim_dir()).chain(std::env::split_paths(&managed_path)),
    )
    .map_err(|error| std::io::Error::other(format!("session PATH: {error}")))?
    .into_string()
    .map_err(|_| ToolRuntimeError::InvalidCache(tools.bin_dir().clone()))?;
    let mut env = vec![("PATH".to_string(), path)];
    for (name, directory) in writable.variables() {
        env.push((name.to_string(), path_value(name, &directory)?));
    }
    env.push((
        "PLAYWRIGHT_BROWSERS_PATH".to_string(),
        path_value("PLAYWRIGHT_BROWSERS_PATH", &playwright_cache)?,
    ));
    env.push(("PYTHONPATH".to_string(), python));
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
    let mut has_stow_cache = false;
    let homes: Vec<_> = blocking::unblock(|| host_toolchain_homes().collect()).await;
    for (name, home) in homes {
        if name == "STOW_CACHE_DIR" {
            has_stow_cache = true;
        }
        env.push((name.to_string(), path_value(name, &home)?));
    }
    #[cfg(target_os = "linux")]
    if has_stow_cache {
        use std::os::unix::fs::PermissionsExt as _;
        let stow_run = writable.cache().join("stow").join("run");
        async_fs::create_dir_all(&stow_run).await?;
        async_fs::set_permissions(&stow_run, std::fs::Permissions::from_mode(0o700)).await?;
        env.push((
            "STOW_DAEMON_DIR".to_string(),
            path_value("STOW_DAEMON_DIR", &stow_run)?,
        ));
    }
    let _ = has_stow_cache;
    Ok(env)
}

/// The `git` shim every sandboxed command's PATH resolves first: a git run in
/// a committing worktree reads the worktree's host-owned entry instead of
/// whatever `.git` its subagent wrote.
const GIT_SHIM: &str = include_str!("../../templates/git_shim");

/// Writes the `git` shim into the prepared tools directory.
#[cfg(unix)]
async fn write_git_shim(tools: &PreparedTools<PathBuf>) -> std::io::Result<()> {
    write_shim(tools.bin_dir().join("git"), GIT_SHIM).await
}

/// Makes the executable at `path` hold `content`: a shim another build wrote
/// is replaced by a rename, so a command already running it keeps its file.
#[cfg(unix)]
async fn write_shim(path: PathBuf, content: &'static str) -> std::io::Result<()> {
    match async_fs::read(&path).await {
        Ok(current) if current == content.as_bytes() => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    blocking::unblock(move || {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let dir = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("the shim path {} has no parent", path.display()),
            )
        })?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.write_all(content.as_bytes())?;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o755))?;
        file.persist(&path).map_err(|error| error.error)?;
        Ok(())
    })
    .await
}

/// Windows commands run no `sh`: no shim.
#[cfg(not(unix))]
async fn write_git_shim(_tools: &PreparedTools<PathBuf>) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::write_shim;

    #[test]
    fn a_stale_shim_is_replaced_and_a_current_one_kept() {
        futures_lite::future::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let shim = dir.path().join("git");
            std::fs::write(&shim, "#!/bin/sh\necho stale\n").unwrap();

            write_shim(shim.clone(), "#!/bin/sh\necho current\n")
                .await
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(&shim).unwrap(),
                "#!/bin/sh\necho current\n"
            );
            let mode = std::fs::metadata(&shim).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);

            let before = std::fs::metadata(&shim).unwrap().modified().unwrap();
            write_shim(shim.clone(), "#!/bin/sh\necho current\n")
                .await
                .unwrap();
            assert_eq!(
                std::fs::metadata(&shim).unwrap().modified().unwrap(),
                before
            );
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        });
    }

    /// Runs the `git` shim traced (`sh -x`) with `args` from `cwd`, pinned
    /// to `worktree`'s host-owned `gitdir`, and returns `(stdout, stderr)`:
    /// the trace on stderr shows the `GIT_DIR=... exec` a pin applies.
    fn git_shim(
        args: &[&str],
        cwd: &std::path::Path,
        worktree: &std::path::Path,
        gitdir: &std::path::Path,
    ) -> (String, String) {
        use std::os::unix::fs::PermissionsExt as _;
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("git");
        std::fs::write(&shim, super::GIT_SHIM).unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new("sh")
            .arg("-x")
            .arg(&shim)
            .args(args)
            .current_dir(cwd)
            .env("AITHER_GIT_WORKTREES_COUNT", "1")
            .env("AITHER_GIT_WORKTREE_PATH_0", worktree)
            .env("AITHER_GIT_WORKTREE_GITDIR_0", gitdir)
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// `-C <dir>` names the directory git resolves from: under a committing
    /// worktree the repository it runs is the host-owned one.
    #[test]
    fn the_git_shim_pins_the_repository_under_dash_c() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let worktree = dir.path().join("worktree");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .output()
            .unwrap();
        let gitdir = repo.join(".git");
        // rev-parse reports the canonical path.
        let pinned = dunce::canonicalize(&gitdir).unwrap();
        let pinned = pinned.to_str().unwrap();

        let (resolved, trace) = git_shim(
            &[
                "-C",
                worktree.to_str().unwrap(),
                "rev-parse",
                "--absolute-git-dir",
            ],
            &elsewhere,
            &worktree,
            &gitdir,
        );
        assert_eq!(resolved, pinned, "{trace}");
        // `-C<dir>` — the attached spelling — resolves the same directory,
        // so the pin is applied to the exec it forwards.
        let (_, trace) = git_shim(
            &[
                &format!("-C{}", worktree.display()),
                "rev-parse",
                "--absolute-git-dir",
            ],
            &elsewhere,
            &worktree,
            &gitdir,
        );
        assert!(
            trace.contains("GIT_DIR="),
            "-C<dir> did not resolve to the pinned worktree:\n{trace}"
        );
    }

    /// A `--git-dir` after `--` is a pathspec, not plumbing: the shim still
    /// pins the worktree's repository.
    #[test]
    fn the_git_shim_keeps_pinning_past_the_option_terminator() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let worktree = dir.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .output()
            .unwrap();
        let gitdir = repo.join(".git");

        let (resolved, trace) = git_shim(
            &[
                "-C",
                worktree.to_str().unwrap(),
                "rev-parse",
                "--absolute-git-dir",
                "--",
                "--git-dir",
            ],
            dir.path(),
            &worktree,
            &gitdir,
        );
        // rev-parse answers --absolute-git-dir first; the args after `--`
        // are echoed too, so the pin is its first line.
        let pinned = dunce::canonicalize(&gitdir).unwrap();
        assert_eq!(
            resolved.lines().next().unwrap_or_default(),
            pinned.to_str().unwrap(),
            "{trace}"
        );
    }

    #[test]
    fn venv_symlink_and_pyvenv_home_receive_read_exec_grants_only() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let venv = workspace.join(".venv");
        let runtime_a = tmp.path().join("uv-python/cpython-3.10");
        let runtime_b = tmp.path().join("pyenv-python/3.10");
        std::fs::create_dir_all(venv.join("bin")).unwrap();
        std::fs::create_dir_all(runtime_a.join("bin")).unwrap();
        std::fs::create_dir_all(runtime_b.join("bin")).unwrap();
        let exe = runtime_a.join("bin/python3.10");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        symlink(&exe, venv.join("bin/python3")).unwrap();
        std::fs::write(
            venv.join("pyvenv.cfg"),
            format!(
                "home = {}\ninclude-system-site-packages = false\n",
                runtime_b.join("bin").display()
            ),
        )
        .unwrap();

        let grants = super::runtime_toolchain_grants(tmp.path(), std::slice::from_ref(&workspace))
            .expect("grants collect");
        let canon = |p: &std::path::Path| {
            dunce::simplified(&std::fs::canonicalize(p).unwrap()).to_path_buf()
        };
        let has_read_exec_only = |target: &std::path::Path| {
            let expected = canon(target);
            grants.iter().any(|grant| {
                grant.path() == expected.as_path()
                    && grant.access() == (heel::Access::READ | heel::Access::EXEC)
            })
        };
        assert!(
            has_read_exec_only(&venv),
            "venv dir should be granted READ|EXEC"
        );
        assert!(
            has_read_exec_only(&runtime_a),
            "symlink runtime root should be granted READ|EXEC"
        );
        assert!(
            has_read_exec_only(&runtime_b),
            "pyvenv.cfg home root should be granted READ|EXEC"
        );
    }

    #[test]
    fn path_directory_grants_follows_toolchain_symlinks_and_parent_pyvenv() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("custom-venv/bin");
        let tool_root = tmp.path().join("alt-gcc");
        let py_root = tmp.path().join("uv-py");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(tool_root.join("bin")).unwrap();
        std::fs::create_dir_all(py_root.join("bin")).unwrap();
        std::fs::write(tool_root.join("bin/gcc-14"), "#!/bin/sh\n").unwrap();
        symlink(tool_root.join("bin/gcc-14"), bin_dir.join("cc")).unwrap();
        std::fs::write(
            tmp.path().join("custom-venv/pyvenv.cfg"),
            format!("home = {}\n", py_root.join("bin").display()),
        )
        .unwrap();

        let env = vec![("PATH".to_string(), bin_dir.display().to_string())];
        let grants = super::path_directory_grants(&env).expect("path grants");
        let canon = |p: &std::path::Path| {
            dunce::simplified(&std::fs::canonicalize(p).unwrap()).to_path_buf()
        };
        let has_read_exec_only = |target: &std::path::Path| {
            let expected = canon(target);
            grants.iter().any(|grant| {
                grant.path() == expected.as_path()
                    && grant.access() == (heel::Access::READ | heel::Access::EXEC)
            })
        };
        assert!(
            has_read_exec_only(&tool_root),
            "cc symlink target root granted READ|EXEC"
        );
        assert!(
            has_read_exec_only(&py_root),
            "parent pyvenv.cfg home root granted READ|EXEC"
        );
    }
}
