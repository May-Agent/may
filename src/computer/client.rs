//! The daemon's handle to `may computer-agent`: the whole desktop stack —
//! displays, capture, streams, screenshots, input injection — lives in
//! that helper process. Requests are newline-delimited JSON over
//! `~/.may/computer-agent.sock`; the daemon spawns the helper lazily and
//! respawns it when macOS privacy grants change (TCC evaluates per
//! process at launch, so only a fresh process sees a fresh grant).

#[cfg(target_os = "macos")]
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::sync::Mutex;

#[cfg(target_os = "macos")]
use std::os::unix::net::UnixStream;

use std::sync::Arc;

use arc_swap::ArcSwap;

use super::proto::{AgentRequest, AgentResponse};
use crate::config::MayConfig;

#[cfg(target_os = "macos")]
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
/// How long a launched helper has to start answering.
#[cfg(target_os = "macos")]
const HELPER_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// How long the `LaunchServices` request instance has to raise the prompts
/// and exit.
#[cfg(target_os = "macos")]
const PERMISSION_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// How long a terminated helper has to exit.
#[cfg(target_os = "macos")]
const HELPER_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The daemon-side client for one `computer-agent` child.
pub struct ComputerAgent {
    /// The daemon's live config; `[computer]` is read per launch.
    #[cfg(target_os = "macos")]
    config: Arc<ArcSwap<MayConfig>>,
    #[cfg(target_os = "macos")]
    socket: PathBuf,
    #[cfg(target_os = "macos")]
    may_dir: PathBuf,
    /// Serializes `open` launches — the helper reports liveness through
    /// its pid file, so a launch in flight must not overlap another.
    #[cfg(target_os = "macos")]
    spawn_lock: Mutex<()>,
}

impl std::fmt::Debug for ComputerAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputerAgent").finish_non_exhaustive()
    }
}

impl ComputerAgent {
    /// Point the client at `<may_dir>/computer-agent.sock`.
    #[must_use]
    #[allow(unused_variables, clippy::needless_pass_by_value)]
    pub fn new(may_dir: PathBuf, config: Arc<ArcSwap<MayConfig>>) -> Self {
        Self {
            #[cfg(target_os = "macos")]
            config,
            #[cfg(target_os = "macos")]
            socket: may_dir.join("computer-agent.sock"),
            #[cfg(target_os = "macos")]
            may_dir,
            #[cfg(target_os = "macos")]
            spawn_lock: Mutex::new(()),
        }
    }

    /// Send one request; spawns the helper when it is not up.
    ///
    /// # Errors
    /// Returns the message text when the helper is unreachable or the
    /// request itself failed.
    #[cfg(target_os = "macos")]
    pub async fn request(self: &Arc<Self>, request: AgentRequest) -> Result<AgentResponse, String> {
        let payload = serde_json::to_vec(&request)
            .map_err(|e| e.to_string())
            .map(|mut v| {
                v.push(b'\n');
                v
            })?;
        // Launching may sign the bundle and wait for the helper; none of it
        // runs on the async thread.
        let agent = self.clone();
        tokio::task::spawn_blocking(move || {
            agent.ensure_running()?;
            round_trip(&agent.socket, &payload)
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Every computer request is unsupported outside macOS.
    ///
    /// # Errors
    /// Always fails — virtual desktops are macOS-only for now.
    #[cfg(not(target_os = "macos"))]
    pub async fn request(&self, request: AgentRequest) -> Result<AgentResponse, String> {
        let _ = request;
        std::future::ready(()).await;
        Err("virtual desktops need macOS".to_owned())
    }

    /// Whether a helper answers on the socket; asking never spawns it.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.responds()
    }

    /// A connect succeeds only while a live helper owns the socket; a
    /// leftover socket file refuses it.
    #[cfg(target_os = "macos")]
    fn responds(&self) -> bool {
        UnixStream::connect(&self.socket).is_ok()
    }

    /// There is no helper outside macOS.
    #[cfg(not(target_os = "macos"))]
    #[must_use]
    pub const fn is_running(&self) -> bool {
        false
    }

    /// Stop the helper and wait until it has exited, so the next request
    /// spawns exactly one fresh helper — used after the user grants
    /// permissions, since TCC applies at process launch.
    ///
    /// # Errors
    /// Returns a message when the helper is still alive after the wait.
    #[cfg(target_os = "macos")]
    pub fn restart(&self) -> Result<(), String> {
        let _launch = self
            .spawn_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.stop_helper()
    }

    /// Nothing to respawn outside macOS.
    ///
    /// # Errors
    /// Never fails.
    #[cfg(not(target_os = "macos"))]
    pub const fn restart(&self) -> Result<(), String> {
        Ok(())
    }

    /// Raise the system prompts for the missing grants from an instance of
    /// the helper bundle that `LaunchServices` launches on its own: TCC
    /// registers a Screen Recording request only from such an app
    /// instance, never from a child the helper execs.
    ///
    /// # Errors
    /// Returns a message when the bundle cannot be prepared, `open` fails,
    /// or the request instance does not exit in time.
    #[cfg(target_os = "macos")]
    pub fn request_permissions(&self) -> Result<(), String> {
        let _launch = self
            .spawn_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let identity = self.config.load().computer.signing_identity.clone();
        let app = self.materialize_app(identity.as_deref())?;
        // `-W` waits for the request instance; `-g` keeps it in the
        // background so only the system's own prompt takes focus.
        let mut child = std::process::Command::new("open")
            .args(["-n", "-W", "-g"])
            .arg(&app)
            .arg("--args")
            .arg(super::permissions::PROBE_COMMAND)
            .arg(super::permissions::PROBE_REQUEST_FLAG)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("launch permission request: {e}"))?;
        let deadline = std::time::Instant::now() + PERMISSION_REQUEST_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                if status.success() {
                    return Ok(());
                }
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    use std::io::Read;
                    let _ = pipe.read_to_string(&mut stderr);
                }
                return Err(format!(
                    "permission request exited with {status}: {}",
                    stderr.trim()
                ));
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                return Err("permission request did not finish".to_owned());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// There are no grants to request outside macOS.
    ///
    /// # Errors
    /// Always fails — virtual desktops are macOS-only for now.
    #[cfg(not(target_os = "macos"))]
    pub fn request_permissions(&self) -> Result<(), String> {
        Err("virtual desktops need macOS".to_owned())
    }

    /// SIGTERM the pid-file helper and wait for that process to exit.
    #[cfg(target_os = "macos")]
    fn stop_helper(&self) -> Result<(), String> {
        let Some(pid) = self.helper_pid() else {
            return Ok(());
        };
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + HELPER_EXIT_TIMEOUT;
        while self.helper_pid() == Some(pid) {
            if std::time::Instant::now() >= deadline {
                return Err(format!("computer-agent (pid {pid}) did not exit"));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        Ok(())
    }

    /// A stale pid file or a dead pid both mean "not running".
    #[cfg(target_os = "macos")]
    fn helper_pid(&self) -> Option<libc::pid_t> {
        let pid: libc::pid_t = std::fs::read_to_string(self.may_dir.join("computer-agent.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        (unsafe { libc::kill(pid, 0) } == 0).then_some(pid)
    }

    /// `~/.may/MayComputerAgent.app` — the bundle screen recording needs.
    ///
    /// TCC only credits a kTCCServiceScreenCapture request to a process
    /// with its own bundle identity; a bare binary inherits the
    /// responsible process (the terminal). Materializing the app here and
    /// launching it through `open` gives the helper a stable
    /// `ai.may.computer-agent` subject — the same shape cua-driver and
    /// Codex Computer Use take.
    ///
    /// The whole bundle is signed with `identity` (ad hoc when `None`) so
    /// the Info.plist is bound and the designated requirement is the
    /// identifier plus certificate; the bundle is rebuilt, and a running
    /// helper stopped, whenever the source binary or the identity differs
    /// from what the bundle was built from.
    #[cfg(target_os = "macos")]
    fn materialize_app(&self, identity: Option<&str>) -> Result<PathBuf, String> {
        let app = self.may_dir.join("MayComputerAgent.app");
        let stamp_path = self.may_dir.join(BUNDLE_STAMP_FILE);
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let dst = app.join("Contents/MacOS/may");
        let wanted = BundleStamp::of(&exe, identity)?;
        let recorded = std::fs::read(&stamp_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<BundleStamp>(&bytes).ok());
        if !needs_rebuild(recorded.as_ref(), &wanted, dst.exists()) {
            return Ok(app);
        }
        // The running helper executes the old bundle; replace it only
        // once that process is gone.
        self.stop_helper()?;
        match std::fs::remove_file(&stamp_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("remove bundle stamp: {e}")),
        }
        let macos_dir = app.join("Contents/MacOS");
        std::fs::create_dir_all(&macos_dir).map_err(|e| e.to_string())?;
        std::fs::copy(&exe, &dst).map_err(|e| format!("copy helper into bundle: {e}"))?;
        std::fs::write(app.join("Contents/Info.plist"), include_str!("Info.plist"))
            .map_err(|e| e.to_string())?;
        sign_bundle(&app, identity)?;
        let stamp = serde_json::to_vec(&wanted).map_err(|e| e.to_string())?;
        std::fs::write(&stamp_path, stamp).map_err(|e| format!("write bundle stamp: {e}"))?;
        Ok(app)
    }

    #[cfg(target_os = "macos")]
    #[allow(clippy::significant_drop_tightening)]
    fn ensure_running(&self) -> Result<(), String> {
        let _launch = self
            .spawn_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Read live: a changed identity rebuilds the bundle and stops the
        // running helper before the liveness check below.
        let identity = self.config.load().computer.signing_identity.clone();
        let app = self.materialize_app(identity.as_deref())?;
        if self.responds() {
            return Ok(());
        }
        // `open` routes through LaunchServices, which is what breaks the
        // responsible-process chain; a plain spawn would inherit the
        // daemon's (or a terminal's) TCC subject.
        let status = std::process::Command::new("open")
            .arg("-n")
            .arg(&app)
            .arg("--args")
            .arg("computer-agent")
            .arg("--may-dir")
            .arg(&self.may_dir)
            .arg("--stderr")
            .arg(self.may_dir.join("computer-agent.log"))
            .output()
            .map_err(|e| format!("launch computer-agent app: {e}"))?;
        if !status.status.success() {
            return Err(format!(
                "open {} failed: {}",
                app.display(),
                String::from_utf8_lossy(&status.stderr)
            ));
        }
        // A socket file proves nothing — the previous helper's survives —
        // so wait until a connect is accepted.
        let deadline = std::time::Instant::now() + HELPER_START_TIMEOUT;
        while std::time::Instant::now() < deadline {
            if self.responds() {
                return Ok(());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        Err("computer-agent did not start answering on its socket".to_owned())
    }
}

/// Where the stamp of what the bundle was built from lives — beside the
/// bundle, because a file inside it would break its seal.
#[cfg(target_os = "macos")]
const BUNDLE_STAMP_FILE: &str = "MayComputerAgent.stamp";

/// What a materialized bundle was built from.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct BundleStamp {
    source_len: u64,
    source_modified: Option<std::time::SystemTime>,
    /// The `codesign` identity, `-` for ad hoc.
    identity: String,
}

#[cfg(target_os = "macos")]
impl BundleStamp {
    fn of(exe: &std::path::Path, identity: Option<&str>) -> Result<Self, String> {
        let meta = std::fs::metadata(exe).map_err(|e| format!("stat {}: {e}", exe.display()))?;
        Ok(Self {
            source_len: meta.len(),
            source_modified: meta.modified().ok(),
            identity: identity.unwrap_or("-").to_owned(),
        })
    }
}

/// The bundle is rebuilt unless it exists and was built from exactly
/// what is wanted now.
#[cfg(target_os = "macos")]
fn needs_rebuild(
    recorded: Option<&BundleStamp>,
    wanted: &BundleStamp,
    bundle_present: bool,
) -> bool {
    !bundle_present || recorded != Some(wanted)
}

/// Sign the whole bundle so the Info.plist is bound and the designated
/// requirement is identifier plus certificate.
#[cfg(target_os = "macos")]
fn sign_bundle(app: &std::path::Path, identity: Option<&str>) -> Result<(), String> {
    let output = std::process::Command::new("codesign")
        .args(["--force", "--sign", identity.unwrap_or("-")])
        .args(["--identifier", "ai.may.computer-agent"])
        .arg(app)
        .output()
        .map_err(|e| format!("run codesign: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "codesign {} failed: {}",
            app.display(),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(target_os = "macos")]
fn round_trip(socket: &PathBuf, payload: &[u8]) -> Result<AgentResponse, String> {
    let mut conn = UnixStream::connect(socket).map_err(|e| format!("connect: {e}"))?;
    conn.write_all(payload).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(&conn)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|e| format!("bad reply: {e}"))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{BundleStamp, needs_rebuild};

    fn stamp(len: u64, identity: &str) -> BundleStamp {
        BundleStamp {
            source_len: len,
            source_modified: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(len)),
            identity: identity.to_owned(),
        }
    }

    #[test]
    fn bundle_is_reused_only_when_built_from_the_same_source_and_identity() {
        let built = stamp(10, "-");
        assert!(!needs_rebuild(Some(&built), &built.clone(), true));
        // No stamp, or a missing bundle.
        assert!(needs_rebuild(None, &built, true));
        assert!(needs_rebuild(Some(&built), &built, false));
        // Rebuilt binary.
        assert!(needs_rebuild(Some(&built), &stamp(11, "-"), true));
        let mut touched = built.clone();
        touched.source_modified = Some(std::time::UNIX_EPOCH);
        assert!(needs_rebuild(Some(&built), &touched, true));
        // Changed identity.
        assert!(needs_rebuild(Some(&built), &stamp(10, "ABCDEF"), true));
    }

    #[test]
    fn stamp_survives_its_file_form() {
        let built = stamp(10, "ABCDEF");
        let bytes = serde_json::to_vec(&built).expect("serialize");
        let read: BundleStamp = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(read, built);
    }
}
