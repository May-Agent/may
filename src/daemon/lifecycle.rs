//! Starting, stopping and installing the daemon.
//!
//! Clients call [`ensure_running`] before their first request: it reuses a
//! daemon of the same build, replaces one of another build, and spawns one
//! when none answers. `may daemon install` makes the daemon a login service
//! (a launchd agent on macOS, a systemd user unit on Linux) so indexing and
//! scheduled tasks keep running without an open UI.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use askama::Template;

use super::DaemonPaths;
use crate::client::{DaemonClient, DaemonClientError};

/// launchd label and systemd unit name of the installed service.
pub const SERVICE_LABEL: &str = "com.github.may-agent.may";
const SYSTEMD_UNIT: &str = "may-daemon.service";

/// How long a client waits for a spawned daemon to answer.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a replaced daemon gets to release its socket.
const STOP_TIMEOUT: Duration = Duration::from_secs(40);

/// Why the daemon could not be reached or managed.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    /// Filesystem or process failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The daemon did not answer in time; its log says why.
    #[error("the May daemon did not start within {}s; see {}", START_TIMEOUT.as_secs(), .log.display())]
    StartTimeout {
        /// The daemon log.
        log: PathBuf,
    },
    /// The old daemon did not stop.
    #[error("the running May daemon (pid {pid}) did not stop")]
    StopTimeout {
        /// Its process id.
        pid: u32,
    },
    /// A service-manager command failed.
    #[error("`{command}` failed: {detail}")]
    ServiceManager {
        /// The command run.
        command: String,
        /// Its stderr.
        detail: String,
    },
    /// The service unit failed to render.
    #[error("failed to render the service definition: {0}")]
    Render(#[from] askama::Error),
    /// The daemon answered with an error.
    #[error(transparent)]
    Client(#[from] DaemonClientError),
}

#[derive(Template)]
#[template(path = "daemon_launchd.plist", escape = "html")]
struct LaunchdPlist<'a> {
    label: &'a str,
    program: &'a str,
    log: &'a str,
}

#[derive(Template)]
#[template(path = "daemon_systemd.service", escape = "none")]
struct SystemdUnit<'a> {
    program: &'a str,
}

fn current_exe() -> std::io::Result<PathBuf> {
    dunce::canonicalize(std::env::current_exe()?)
}

fn launchd_plist_path() -> std::io::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| std::io::Error::other("no home directory"))?;
    let mut name = String::from(SERVICE_LABEL);
    name.push_str(".plist");
    Ok(home.join("Library").join("LaunchAgents").join(name))
}

fn systemd_unit_path() -> std::io::Result<PathBuf> {
    let config = dirs::config_dir().ok_or_else(|| std::io::Error::other("no config directory"))?;
    Ok(config.join("systemd").join("user").join(SYSTEMD_UNIT))
}

/// Whether the daemon is installed as a login service.
#[must_use]
pub fn installed() -> bool {
    let path = if cfg!(target_os = "macos") {
        launchd_plist_path()
    } else {
        systemd_unit_path()
    };
    path.is_ok_and(|path| path.exists())
}

fn run(command: &str, args: &[&str]) -> Result<(), LifecycleError> {
    let output = std::process::Command::new(command).args(args).output()?;
    if output.status.success() {
        return Ok(());
    }
    let mut line = String::from(command);
    for arg in args {
        line.push(' ');
        line.push_str(arg);
    }
    Err(LifecycleError::ServiceManager {
        command: line,
        detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

fn uid() -> Result<String, LifecycleError> {
    let output = std::process::Command::new("id").arg("-u").output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Installs the daemon as a login service and starts it.
///
/// # Errors
/// Fails when the service file cannot be written or the service manager
/// refuses it.
pub fn install(paths: &DaemonPaths) -> Result<PathBuf, LifecycleError> {
    let program = current_exe()?;
    let program = program.display().to_string();
    std::fs::create_dir_all(paths.log().parent().unwrap_or(paths.root()))?;
    if cfg!(target_os = "macos") {
        let path = launchd_plist_path()?;
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))?;
        let plist = LaunchdPlist {
            label: SERVICE_LABEL,
            program: &program,
            log: &paths.log().display().to_string(),
        }
        .render()?;
        let domain = format!("gui/{}", uid()?);
        if path.exists() {
            let _ = run("launchctl", &["bootout", &domain, &path.display().to_string()]);
        }
        std::fs::write(&path, plist)?;
        run("launchctl", &["bootstrap", &domain, &path.display().to_string()])?;
        Ok(path)
    } else {
        let path = systemd_unit_path()?;
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))?;
        std::fs::write(&path, SystemdUnit { program: &program }.render()?)?;
        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", "--now", SYSTEMD_UNIT])?;
        Ok(path)
    }
}

/// Stops and removes the login service. A daemon started on demand keeps
/// running until it is stopped.
///
/// # Errors
/// Fails when the service file cannot be removed.
pub fn uninstall() -> Result<Option<PathBuf>, LifecycleError> {
    if cfg!(target_os = "macos") {
        let path = launchd_plist_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let domain = format!("gui/{}", uid()?);
        let _ = run("launchctl", &["bootout", &domain, &path.display().to_string()]);
        std::fs::remove_file(&path)?;
        Ok(Some(path))
    } else {
        let path = systemd_unit_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let _ = run("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]);
        std::fs::remove_file(&path)?;
        let _ = run("systemctl", &["--user", "daemon-reload"]);
        Ok(Some(path))
    }
}

fn restart_service() -> Result<(), LifecycleError> {
    if cfg!(target_os = "macos") {
        let target = format!("gui/{}/{SERVICE_LABEL}", uid()?);
        run("launchctl", &["kickstart", "-k", &target])
    } else {
        run("systemctl", &["--user", "restart", SYSTEMD_UNIT])
    }
}

fn spawn(paths: &DaemonPaths) -> Result<(), LifecycleError> {
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;

    let log_path = paths.log();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let mut command = std::process::Command::new(current_exe()?);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // Its own process group: the daemon outlives the terminal that
        // started it, and a Ctrl+C there does not reach it.
        .process_group(0);
    command.spawn()?;
    Ok(())
}

async fn wait_until_gone(client: &DaemonClient, pid: u32) -> Result<(), LifecycleError> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        if client.health().await.is_err() {
            return Ok(());
        }
        async_io::Timer::after(Duration::from_millis(200)).await;
    }
    Err(LifecycleError::StopTimeout { pid })
}

/// Asks the running daemon to stop and waits until it is gone. Returns
/// whether one was running.
///
/// # Errors
/// Fails when the daemon does not stop in time.
pub async fn stop(paths: &DaemonPaths) -> Result<bool, LifecycleError> {
    let client = DaemonClient::new(paths.socket());
    let Ok(health) = client.health().await else {
        return Ok(false);
    };
    client.shutdown().await?;
    wait_until_gone(&client, health.pid).await?;
    Ok(true)
}

/// Makes sure a daemon of this build answers on the socket, starting or
/// replacing one as needed. `progress` receives one line per step, for the
/// caller's loading indicator.
///
/// # Errors
/// Fails when no daemon answers within the start timeout.
pub async fn ensure_running(
    paths: &DaemonPaths,
    mut progress: impl FnMut(&str),
) -> Result<DaemonClient, LifecycleError> {
    let client = DaemonClient::new(paths.socket());
    let build = super::current_build_id()?;
    if let Ok(health) = client.health().await {
        if health.build == build {
            return Ok(client);
        }
        progress("Restarting the May daemon for this build…");
        if installed() {
            restart_service()?;
        } else {
            client.shutdown().await?;
            wait_until_gone(&client, health.pid).await?;
            spawn(paths)?;
        }
    } else {
        progress("Starting the May daemon…");
        if installed() {
            restart_service()?;
        } else {
            spawn(paths)?;
        }
    }
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(health) = client.health().await
            && health.build == build
        {
            return Ok(client);
        }
        async_io::Timer::after(Duration::from_millis(200)).await;
    }
    Err(LifecycleError::StartTimeout { log: paths.log() })
}
