//! Starting, stopping and installing the daemon.
//!
//! Clients call [`ensure_running`] before their first request: it reuses a
//! daemon of the same build, replaces one of another build, and spawns one
//! when none answers. `may daemon install` makes the daemon a login service
//! (a launchd agent on macOS, a systemd user unit on Linux) so indexing and
//! scheduled tasks keep running without an open UI.

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use askama::Template;

use super::DaemonPaths;
use crate::client::{DaemonClient, DaemonClientError};

/// launchd label and systemd unit name of the installed service.
pub const SERVICE_LABEL: &str = "com.github.may-agent.may";
/// Prefix of the launchd label of the daemon a client starts on demand on
/// macOS; [`on_demand_label`] appends a key of the May home.
const ON_DEMAND_LABEL: &str = "com.github.may-agent.may.on-demand";
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
    /// The daemon this client started exited before it answered.
    #[error("the May daemon exited while starting ({status}): {reason}; see {}", .log.display())]
    StartFailed {
        /// How it exited.
        status: String,
        /// The last line it logged.
        reason: String,
        /// The daemon log.
        log: PathBuf,
    },
    /// No one is logged in at the Mac's screen, so launchd has no GUI session
    /// to run the daemon in.
    #[error(
        "the login keychain is unavailable: nobody is logged in at this Mac's screen, so the May daemon cannot be started in the GUI session"
    )]
    NoGuiSession,
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
    /// An environment variable the daemon must inherit is not valid Unicode.
    #[error(
        "the environment variable {name} is not valid Unicode, so the daemon cannot inherit it"
    )]
    NonUnicodeEnvironment {
        /// The variable (its value is not shown).
        name: String,
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
    /// Whether launchd restarts the daemon when it exits.
    keep_alive: bool,
    /// The environment the daemon runs with; launchd's own is minimal.
    environment: &'a [(String, String)],
}

/// The calling process's environment as text. A launchd job starts with
/// launchd's default environment, so the daemon is given the client's.
///
/// Its `PATH` leads with the directory of this executable, so the `tasks`
/// binary installed beside `may` is on the `PATH` of every sandboxed command.
fn environment() -> Result<Vec<(String, String)>, LifecycleError> {
    let mut environment = text_environment(std::env::vars_os())?;
    let path = path_with_executable_dir()?;
    match environment.iter_mut().find(|(name, _)| name == "PATH") {
        Some((_, value)) => *value = path,
        None => environment.push(("PATH".to_string(), path)),
    }
    Ok(environment)
}

/// The calling process's `PATH` led by the directory of its executable.
fn path_with_executable_dir() -> Result<String, LifecycleError> {
    let executable = current_exe()?;
    let directory = executable
        .parent()
        .ok_or_else(|| std::io::Error::other("the executable has no directory"))?
        .to_path_buf();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let joined =
        std::env::join_paths(std::iter::once(directory).chain(std::env::split_paths(&path)))
            .map_err(std::io::Error::other)?;
    joined
        .into_string()
        .map_err(|_| LifecycleError::NonUnicodeEnvironment {
            name: "PATH".to_string(),
        })
}

fn text_environment(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<Vec<(String, String)>, LifecycleError> {
    vars.into_iter()
        .map(|(key, value)| {
            let name = key.to_string_lossy().into_owned();
            match (key.into_string(), value.into_string()) {
                (Ok(key), Ok(value)) => Ok((key, value)),
                _ => Err(LifecycleError::NonUnicodeEnvironment { name }),
            }
        })
        .collect()
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

/// Runs `command` off the executor and returns its stdout.
async fn run_output(command: &str, args: &[&str]) -> Result<String, LifecycleError> {
    let command = command.to_owned();
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    blocking::unblock(move || {
        let output = std::process::Command::new(&command).args(&args).output()?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        let mut line = command;
        for arg in &args {
            line.push(' ');
            line.push_str(arg);
        }
        Err(LifecycleError::ServiceManager {
            command: line,
            detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    })
    .await
}

async fn run(command: &str, args: &[&str]) -> Result<(), LifecycleError> {
    run_output(command, args).await.map(drop)
}

async fn uid() -> Result<String, LifecycleError> {
    Ok(run_output("id", &["-u"]).await?.trim().to_string())
}

/// Longest `launchctl bootout` is given to take the service out of launchd.
const BOOTOUT_TIMEOUT: Duration = Duration::from_secs(10);
const BOOTOUT_POLL: Duration = Duration::from_millis(100);

/// Boots the service out of `domain` and waits until launchd has dropped it:
/// `bootout` returns first, and a `bootstrap` issued in that window fails with
/// an input/output error. A service that was not loaded needs no wait.
async fn boot_out(domain: &str, label: &str, plist: &Path) -> Result<(), LifecycleError> {
    let _ = run(
        "launchctl",
        &["bootout", domain, &plist.display().to_string()],
    )
    .await;
    let target = format!("{domain}/{label}");
    wait_until_unloaded(
        || async { run("launchctl", &["print", &target]).await.is_ok() },
        BOOTOUT_TIMEOUT,
        BOOTOUT_POLL,
    )
    .await
    .then_some(())
    .ok_or_else(|| LifecycleError::ServiceManager {
        command: format!("launchctl bootout {domain}"),
        detail: format!("{label} was still loaded after {BOOTOUT_TIMEOUT:?}"),
    })
}

/// Polls `is_loaded` until it reports false, for at most `timeout`. Returns
/// whether the service is gone.
async fn wait_until_unloaded<Fut>(
    mut is_loaded: impl FnMut() -> Fut,
    timeout: Duration,
    poll: Duration,
) -> bool
where
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if !is_loaded().await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        async_io::Timer::after(poll).await;
    }
}

/// Installs the daemon as a login service and starts it.
///
/// # Errors
/// Fails when the service file cannot be written or the service manager
/// refuses it.
pub async fn install(paths: &DaemonPaths) -> Result<PathBuf, LifecycleError> {
    let program = current_exe()?;
    let program = program.display().to_string();
    std::fs::create_dir_all(paths.log().parent().unwrap_or_else(|| paths.root()))?;
    if cfg!(target_os = "macos") {
        let path = launchd_plist_path()?;
        std::fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new("/")))?;
        let plist = LaunchdPlist {
            label: SERVICE_LABEL,
            program: &program,
            log: &paths.log().display().to_string(),
            keep_alive: true,
            environment: &environment()?,
        }
        .render()?;
        let domain = format!("gui/{}", uid().await?);
        if path.exists() {
            boot_out(&domain, SERVICE_LABEL, &path).await?;
        }
        write_private(&path, &plist)?;
        run(
            "launchctl",
            &["bootstrap", &domain, &path.display().to_string()],
        )
        .await?;
        Ok(path)
    } else {
        let path = systemd_unit_path()?;
        std::fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new("/")))?;
        std::fs::write(&path, SystemdUnit { program: &program }.render()?)?;
        run("systemctl", &["--user", "daemon-reload"]).await?;
        run("systemctl", &["--user", "enable", "--now", SYSTEMD_UNIT]).await?;
        Ok(path)
    }
}

/// Stops and removes the login service. A daemon started on demand keeps
/// running until it is stopped.
///
/// # Errors
/// Fails when the service file cannot be removed.
pub async fn uninstall() -> Result<Option<PathBuf>, LifecycleError> {
    if cfg!(target_os = "macos") {
        let path = launchd_plist_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let domain = format!("gui/{}", uid().await?);
        boot_out(&domain, SERVICE_LABEL, &path).await?;
        std::fs::remove_file(&path)?;
        Ok(Some(path))
    } else {
        let path = systemd_unit_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let _ = run("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]).await;
        std::fs::remove_file(&path)?;
        let _ = run("systemctl", &["--user", "daemon-reload"]).await;
        Ok(Some(path))
    }
}

async fn restart_service() -> Result<(), LifecycleError> {
    if cfg!(target_os = "macos") {
        let target = format!("gui/{}/{SERVICE_LABEL}", uid().await?);
        run("launchctl", &["kickstart", "-k", &target]).await
    } else {
        run("systemctl", &["--user", "restart", SYSTEMD_UNIT]).await
    }
}

fn spawn(paths: &DaemonPaths) -> Result<std::process::Child, LifecycleError> {
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
        .env("PATH", path_with_executable_dir()?)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Its own process session: the daemon outlives the terminal that started
    // it, a Ctrl+C there does not reach it, and it leads the session every
    // process it starts shares.
    // SAFETY: `setsid` is async-signal-safe and touches no memory of the
    // parent; the child is not a process-group leader before it runs.
    unsafe {
        command.pre_exec(|| rustix::process::setsid().map(drop).map_err(Into::into));
    }
    Ok(command.spawn()?)
}

/// A daemon this client started on demand, watched until it answers.
enum Started {
    /// A child process (Linux).
    Child(std::process::Child),
    /// A launchd job in the user's GUI domain (macOS), by `domain/label`.
    Job(String),
}

impl Started {
    /// How the daemon exited, once it has.
    async fn exited(&mut self) -> Option<String> {
        match self {
            Self::Child(child) => child
                .try_wait()
                .ok()
                .flatten()
                .map(|status| status.to_string()),
            Self::Job(target) => {
                let print = run_output("launchctl", &["print", target]).await.ok()?;
                job_exit(&print)
            }
        }
    }
}

/// How a launchd job last exited, from `launchctl print` output. A job that
/// has not run yet, or is running, reports nothing.
fn job_exit(print: &str) -> Option<String> {
    print.lines().map(str::trim).find_map(|line| {
        let (key, value) = line.split_once(" = ")?;
        match key {
            // `78: EX_CONFIG` once launchd knows the code's name.
            "last exit code" => value
                .split(':')
                .next()?
                .trim()
                .parse::<i32>()
                .ok()
                .map(|code| format!("exit code {code}")),
            "last terminating signal" => Some(format!("signal {value}")),
            _ => None,
        }
    })
}

/// Fails with [`LifecycleError::NoGuiSession`] unless `domain_exists`.
fn require_gui_domain(domain_exists: bool) -> Result<(), LifecycleError> {
    domain_exists
        .then_some(())
        .ok_or(LifecycleError::NoGuiSession)
}

/// Writes `contents` to `path`, readable by the owner alone: the plist holds
/// the client's environment. `mode` only applies when the file is created, so
/// an existing file is restricted before anything is written to it.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.set_len(0)?;
    file.write_all(contents.as_bytes())
}

fn on_demand_plist_path(paths: &DaemonPaths) -> PathBuf {
    paths.root().join("daemon.plist")
}

/// The on-demand job's label for the May home `root`: one job per home, so
/// starting one home's daemon never boots out another home's, the user's own
/// included. `root` is canonical, so every spelling of a home names one job.
fn on_demand_label(root: &Path) -> String {
    use sha2::Digest as _;

    let digest = sha2::Sha256::digest(root.as_os_str().as_encoded_bytes());
    let key = digest.get(..8).expect("a SHA-256 digest has 32 bytes");
    format!("{ON_DEMAND_LABEL}.{}", hex::encode(key))
}

/// Whether `label` names an on-demand job of some May home.
#[cfg(target_os = "macos")]
fn is_on_demand_label(label: &str) -> bool {
    label
        .strip_prefix(ON_DEMAND_LABEL)
        .is_some_and(|key| key.starts_with('.'))
}

/// Boots out the on-demand jobs in `domain` whose plist is gone, returning
/// the services it removed. The plist lives under its May home, so a job
/// whose plist is gone belongs to a deleted home (a test's or a relocated
/// one) and its daemon would otherwise run on until logout. A job launchd
/// reports no plist for is kept: nothing proves its home gone.
#[cfg(target_os = "macos")]
fn sweep_stale_jobs(
    launchctl: &mut impl repo_search_client::launcher::install::Launchctl,
    domain: &str,
) -> Result<Vec<String>, LifecycleError> {
    let launch = |error: repo_search_client::launcher::install::LaunchctlError| {
        LifecycleError::ServiceManager {
            command: "launchctl".to_string(),
            detail: error.to_string(),
        }
    };
    let mut removed = Vec::new();
    for label in launchctl.labels(domain).map_err(launch)? {
        if !is_on_demand_label(&label) {
            continue;
        }
        let service = format!("{domain}/{label}");
        let Some(plist) = launchctl
            .service(&service)
            .map_err(launch)?
            .and_then(|loaded| loaded.path)
        else {
            continue;
        };
        if !plist.try_exists()? {
            launchctl.bootout(&service).map_err(launch)?;
            removed.push(service);
        }
    }
    Ok(removed)
}

/// Renders the on-demand job: no `KeepAlive`, so a stopped daemon stays
/// stopped.
fn on_demand_plist(
    label: &str,
    program: &str,
    log: &str,
    environment: &[(String, String)],
) -> Result<String, LifecycleError> {
    Ok(LaunchdPlist {
        label,
        program,
        log,
        keep_alive: false,
        environment,
    }
    .render()?)
}

/// Submits the daemon to the user's `gui/<uid>` launchd domain, so it runs in
/// the login session (and reaches its keychain) whichever session the client
/// is in. The plist lives under the May home, so nothing starts at login.
async fn start_job(paths: &DaemonPaths) -> Result<Started, LifecycleError> {
    let domain = format!("gui/{}", uid().await?);
    require_gui_domain(run("launchctl", &["print", &domain]).await.is_ok())?;
    let log = paths.log();
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(paths.root())?;
    let label = on_demand_label(&std::fs::canonicalize(paths.root())?);
    // Jobs of deleted homes go first, so a start never leaves them behind.
    #[cfg(target_os = "macos")]
    {
        let sweep_domain = domain.clone();
        let removed = blocking::unblock(move || {
            sweep_stale_jobs(
                &mut repo_search_client::launcher::install::SystemLaunchctl,
                &sweep_domain,
            )
        })
        .await?;
        for service in removed {
            tracing::info!(%service, "booted out the on-demand daemon of a deleted May home");
        }
    }
    let plist = on_demand_plist_path(paths);
    write_private(
        &plist,
        &on_demand_plist(
            &label,
            &current_exe()?.display().to_string(),
            &log.display().to_string(),
            &environment()?,
        )?,
    )?;
    // A job left from an earlier start of this home (a stopped or replaced
    // daemon) is still loaded and would refuse the new bootstrap.
    boot_out(&domain, &label, &plist).await?;
    run(
        "launchctl",
        &["bootstrap", &domain, &plist.display().to_string()],
    )
    .await?;
    Ok(Started::Job(format!("{domain}/{label}")))
}

async fn start(paths: &DaemonPaths) -> Result<Started, LifecycleError> {
    if cfg!(target_os = "macos") {
        start_job(paths).await
    } else {
        spawn(paths).map(Started::Child)
    }
}

/// Why the daemon's last run failed, from its log: anyhow's `Error:` line
/// and the first cause under it, or the log's last line.
fn last_error_line(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let Some(start) = lines.iter().rposition(|line| line.starts_with("Error:")) else {
        return lines
            .last()
            .map_or_else(|| "no output".to_string(), |line| (*line).to_string());
    };
    let error = lines[start].trim_start_matches("Error:").trim();
    match lines.get(start + 1..start + 3) {
        Some(["Caused by:", cause]) => [error, cause].join(": "),
        _ => error.to_string(),
    }
}

/// Whether process `pid` still exists. A daemon this process spawned and
/// that has exited is reaped first, so it does not linger as a zombie.
fn process_alive(pid: u32) -> bool {
    let pid = libc::pid_t::try_from(pid).expect("a pid fits in pid_t");
    // SAFETY: `waitpid` with WNOHANG and no status pointer only reaps an
    // exited child of this process (nothing else waits on a spawned daemon
    // once it answered) and returns at once otherwise.
    unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
    // SAFETY: signal 0 only checks that the process exists.
    let signalled = unsafe { libc::kill(pid, 0) };
    signalled == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

async fn wait_until_gone(client: &DaemonClient, pid: u32) -> Result<(), LifecycleError> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        // Its socket goes before its shutdown finishes; gone is when the
        // process has exited, so nothing it still does outlives the stop.
        if client.health().await.is_err() && !process_alive(pid) {
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
    let mut child = if let Ok(health) = client.health().await {
        if health.build == build {
            return Ok(client);
        }
        progress("Restarting the May daemon for this build…");
        if installed() {
            // Drain first: a service restart alone hard-kills every in-flight
            // turn. `restart_service` below doubles as the forced kill when
            // the daemon does not stop in time.
            if client.shutdown().await.is_ok() {
                let _ = wait_until_gone(&client, health.pid).await;
            }
            restart_service().await?;
            None
        } else {
            client.shutdown().await?;
            wait_until_gone(&client, health.pid).await?;
            Some(start(paths).await?)
        }
    } else {
        progress("Starting the May daemon…");
        if installed() {
            restart_service().await?;
            None
        } else {
            Some(start(paths).await?)
        }
    };
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(health) = client.health().await
            && health.build == build
        {
            return Ok(client);
        }
        // A daemon that fails at startup (a held port, an unusable home
        // directory) exits; there is nothing left to wait for.
        let exited = match child.as_mut() {
            Some(child) => child.exited().await,
            None => None,
        };
        if let Some(status) = exited {
            return Err(LifecycleError::StartFailed {
                status,
                reason: last_error_line(&paths.log()),
                log: paths.log(),
            });
        }
        async_io::Timer::after(Duration::from_millis(200)).await;
    }
    Err(LifecycleError::StartTimeout { log: paths.log() })
}

#[cfg(test)]
mod bootout_wait_tests {
    use std::time::Duration;

    use super::wait_until_unloaded;

    const POLL: Duration = Duration::from_millis(1);

    #[test]
    fn returns_once_the_service_is_gone() {
        let mut probes = 0;
        let gone = futures_lite::future::block_on(wait_until_unloaded(
            || {
                probes += 1;
                std::future::ready(probes < 4)
            },
            Duration::from_secs(5),
            POLL,
        ));
        assert!(gone);
        assert_eq!(probes, 4);
    }

    #[test]
    fn an_unloaded_service_is_probed_once() {
        let mut probes = 0;
        let gone = futures_lite::future::block_on(wait_until_unloaded(
            || {
                probes += 1;
                std::future::ready(false)
            },
            Duration::from_secs(5),
            POLL,
        ));
        assert!(gone);
        assert_eq!(probes, 1);
    }

    #[test]
    fn gives_up_after_the_timeout() {
        let gone = futures_lite::future::block_on(wait_until_unloaded(
            || std::future::ready(true),
            Duration::from_millis(20),
            POLL,
        ));
        assert!(!gone);
    }
}

#[cfg(test)]
mod launchd_job_tests {
    use std::ffi::OsString;

    use super::{
        LifecycleError, ON_DEMAND_LABEL, current_exe, environment, job_exit, on_demand_label,
        on_demand_plist, path_with_executable_dir, require_gui_domain, text_environment,
        write_private,
    };

    #[test]
    fn the_on_demand_plist_is_not_kept_alive() {
        let label = on_demand_label(std::path::Path::new("/home/.may"));
        let plist =
            on_demand_plist(&label, "/bin/may", "/home/.may/daemon.log", &[]).expect("renders");
        assert!(plist.contains(&format!("<string>{label}</string>")));
        assert!(plist.contains("<key>KeepAlive</key>\n  <false/>"));
    }

    /// Two May homes on one machine, a test's and the user's, run their
    /// on-demand daemons side by side: a start in one boots out only that
    /// home's earlier job.
    #[test]
    fn each_may_home_gets_its_own_on_demand_job() {
        let one = on_demand_label(std::path::Path::new("/Users/me/.may"));
        let other = on_demand_label(std::path::Path::new("/private/var/folders/x/T/.tmp1/.may"));
        assert_ne!(one, other);
        assert_eq!(one, on_demand_label(std::path::Path::new("/Users/me/.may")));
        for label in [&one, &other] {
            assert!(label.starts_with(&format!("{ON_DEMAND_LABEL}.")));
            assert_ne!(label.as_str(), super::SERVICE_LABEL);
        }
    }

    #[test]
    fn the_daemon_path_leads_with_the_executable_directory() {
        let executable = current_exe().expect("the test binary has a path");
        let directory = executable.parent().expect("the binary has a directory");
        let path = path_with_executable_dir().expect("the path is text");
        assert_eq!(
            std::env::split_paths(&path).next().as_deref(),
            Some(directory)
        );
        let environment = environment().expect("the environment is text");
        assert_eq!(
            environment
                .iter()
                .filter(|(name, _)| name == "PATH")
                .count(),
            1
        );
    }

    #[test]
    fn environment_values_are_xml_escaped() {
        let environment = vec![
            ("PATH".to_string(), "/opt/homebrew/bin:/usr/bin".to_string()),
            ("PS1".to_string(), "a & b <c>".to_string()),
        ];
        let plist = on_demand_plist(
            &on_demand_label(std::path::Path::new("/home/.may")),
            "/bin/may",
            "/log",
            &environment,
        )
        .expect("renders");
        assert!(plist.contains("<key>PATH</key>\n    <string>/opt/homebrew/bin:/usr/bin</string>"));
        // askama's escaper uses numeric character references.
        assert!(plist.contains("<string>a &#38; b &#60;c&#62;</string>"));
        assert!(!plist.contains("a & b"));
    }

    #[test]
    fn the_plist_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let mode = |path: &std::path::Path| {
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        };

        let fresh = dir.path().join("fresh.plist");
        write_private(&fresh, "secret").expect("written");
        assert_eq!(mode(&fresh), 0o600);

        let older = dir.path().join("older.plist");
        std::fs::write(&older, "a much longer earlier plist").expect("written");
        std::fs::set_permissions(&older, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        write_private(&older, "secret").expect("written");
        assert_eq!(mode(&older), 0o600);
        assert_eq!(std::fs::read_to_string(&older).expect("read"), "secret");
    }

    #[test]
    fn a_non_unicode_variable_is_named_in_the_error() {
        use std::os::unix::ffi::OsStringExt as _;
        let bad = OsString::from_vec(vec![0x66, 0xff]);
        let error = text_environment([(OsString::from("BAD"), bad)]).unwrap_err();
        assert!(matches!(
            error,
            LifecycleError::NonUnicodeEnvironment { ref name } if name == "BAD"
        ));
        let error =
            text_environment([(OsString::from_vec(vec![0xff]), OsString::from("v"))]).unwrap_err();
        assert!(matches!(
            error,
            LifecycleError::NonUnicodeEnvironment { .. }
        ));
    }

    #[test]
    fn a_missing_gui_domain_is_a_typed_error() {
        assert!(matches!(
            require_gui_domain(false),
            Err(LifecycleError::NoGuiSession)
        ));
        assert!(require_gui_domain(true).is_ok());
        assert!(
            LifecycleError::NoGuiSession
                .to_string()
                .contains("login keychain is unavailable")
        );
    }

    #[test]
    fn reads_how_a_job_exited() {
        assert_eq!(
            job_exit("\tstate = running\n\tlast exit code = (never exited)\n"),
            None
        );
        assert_eq!(
            job_exit("\tstate = not running\n\tlast exit code = 1\n").as_deref(),
            Some("exit code 1")
        );
        assert_eq!(
            job_exit("\tlast exit code = 78: EX_CONFIG\n").as_deref(),
            Some("exit code 78")
        );
        assert_eq!(
            job_exit("\tlast terminating signal = Killed: 9\n").as_deref(),
            Some("signal Killed: 9")
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod stale_job_tests {
    use std::path::{Path, PathBuf};

    use repo_search_client::launcher::install::{Launchctl, LaunchctlError, Service};

    use super::{ON_DEMAND_LABEL, on_demand_label, sweep_stale_jobs};

    /// A launchd holding `loaded` labels with their plists, recording every
    /// `bootout`.
    struct Fake {
        loaded: Vec<(String, Option<PathBuf>)>,
        booted_out: Vec<String>,
    }

    impl Launchctl for Fake {
        fn service(&mut self, service: &str) -> Result<Option<Service>, LaunchctlError> {
            Ok(self
                .loaded
                .iter()
                .find(|(label, _)| service.strip_prefix("gui/1/") == Some(label.as_str()))
                .map(|(_, path)| Service {
                    pid: None,
                    path: path.clone(),
                }))
        }

        fn labels(&mut self, _: &str) -> Result<Vec<String>, LaunchctlError> {
            Ok(self.loaded.iter().map(|(label, _)| label.clone()).collect())
        }

        fn bootout(&mut self, service: &str) -> Result<(), LaunchctlError> {
            self.booted_out.push(service.to_owned());
            Ok(())
        }

        fn bootstrap(&mut self, _: &str, _: &Path) -> Result<(), LaunchctlError> {
            unreachable!("sweeping never bootstraps")
        }
    }

    #[test]
    fn boots_out_only_the_jobs_of_deleted_homes() {
        let live = tempfile::tempdir().expect("tempdir");
        let live_plist = live.path().join("daemon.plist");
        std::fs::write(&live_plist, "").expect("plist written");
        let gone = live.path().join("deleted").join("daemon.plist");
        let deleted = on_demand_label(Path::new("/deleted/.may"));
        let mut launchctl = Fake {
            loaded: vec![
                (on_demand_label(live.path()), Some(live_plist)),
                (deleted.clone(), Some(gone.clone())),
                (on_demand_label(Path::new("/unknown/.may")), None),
                ("com.example.other".to_string(), Some(gone.clone())),
                (format!("{ON_DEMAND_LABEL}x"), Some(gone)),
            ],
            booted_out: Vec::new(),
        };
        let removed = sweep_stale_jobs(&mut launchctl, "gui/1").expect("sweeps");
        let expected = vec![format!("gui/1/{deleted}")];
        assert_eq!(removed, expected);
        assert_eq!(launchctl.booted_out, expected);
    }
}

#[cfg(test)]
mod start_failure_tests {
    use super::last_error_line;

    #[test]
    fn reports_the_error_and_its_first_cause() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("daemon.log");
        std::fs::write(
            &log,
            "May daemon listening on x\nError: failed to open stored sessions\n\nCaused by:\n    socket path too long\n\nStack backtrace:\n   0: foo\n",
        )
        .expect("log written");
        assert_eq!(
            last_error_line(&log),
            "failed to open stored sessions: socket path too long"
        );
        std::fs::write(&log, "just a line\n").expect("log written");
        assert_eq!(last_error_line(&log), "just a line");
    }
}
