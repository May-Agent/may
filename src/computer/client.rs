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

use super::proto::{AgentRequest, AgentResponse};

/// The daemon-side client for one `computer-agent` child.
pub struct ComputerAgent {
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
    pub fn new(may_dir: PathBuf) -> Self {
        Self {
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
    pub async fn request(&self, request: AgentRequest) -> Result<AgentResponse, String> {
        self.ensure_running()?;
        let socket = self.socket.clone();
        let payload = serde_json::to_vec(&request)
            .map_err(|e| e.to_string())
            .map(|mut v| {
                v.push(b'\n');
                v
            })?;
        tokio::task::spawn_blocking(move || round_trip(&socket, &payload))
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

    /// Kill the helper so the next request respawns it — used after the
    /// user grants permissions, since TCC applies at process launch.
    #[cfg(target_os = "macos")]
    pub fn restart(&self) {
        if let Some(pid) = self.helper_pid() {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }

    /// Nothing to respawn outside macOS.
    #[cfg(not(target_os = "macos"))]
    pub const fn restart(&self) {}

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
    #[cfg(target_os = "macos")]
    fn materialize_app(&self) -> Result<PathBuf, String> {
        let app = self.may_dir.join("MayComputerAgent.app");
        let macos_dir = app.join("Contents/MacOS");
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let dst = macos_dir.join("may");
        // Refresh when the binary changed — compares cheaply, good enough
        // for dev rebuilds and reinstalls alike.
        let stale = match (std::fs::metadata(&dst), std::fs::metadata(&exe)) {
            (Ok(d), Ok(e)) => d.len() != e.len() || d.modified().ok() < e.modified().ok(),
            _ => true,
        };
        if stale {
            std::fs::create_dir_all(&macos_dir).map_err(|e| e.to_string())?;
            std::fs::copy(&exe, &dst).map_err(|e| format!("copy helper into bundle: {e}"))?;
            std::fs::write(
                app.join("Contents/Info.plist"),
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>ai.may.computer-agent</string>
<key>CFBundleExecutable</key><string>may</string>
<key>CFBundleName</key><string>May Computer Agent</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSUIElement</key><true/>
</dict></plist>
"#,
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(app)
    }

    #[cfg(target_os = "macos")]
    #[allow(clippy::significant_drop_tightening)]
    fn ensure_running(&self) -> Result<(), String> {
        let _launch = self
            .spawn_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.helper_pid().is_some() && self.socket.exists() {
            return Ok(());
        }
        let app = self.materialize_app()?;
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
        // The listener needs a beat to bind its socket.
        for _ in 0..50 {
            if self.socket.exists() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Err("computer-agent did not bind its socket".to_owned())
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
