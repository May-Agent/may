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
    socket: PathBuf,
    may_dir: PathBuf,
    #[cfg(target_os = "macos")]
    child: Mutex<Option<std::process::Child>>,
}

impl std::fmt::Debug for ComputerAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputerAgent")
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

impl ComputerAgent {
    /// Point the client at `<may_dir>/computer-agent.sock`.
    #[must_use]
    pub fn new(may_dir: PathBuf) -> Self {
        Self {
            socket: may_dir.join("computer-agent.sock"),
            may_dir,
            #[cfg(target_os = "macos")]
            child: Mutex::new(None),
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
    #[allow(clippy::unused_async)]
    pub async fn request(&self, _request: AgentRequest) -> Result<AgentResponse, String> {
        Err("virtual desktops need macOS".to_owned())
    }

    /// Kill the helper so the next request respawns it — used after the
    /// user grants permissions, since TCC applies at process launch.
    pub fn restart(&self) {
        #[cfg(target_os = "macos")]
        {
            let mut child = self
                .child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(child) = child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[allow(clippy::significant_drop_tightening)]
    fn ensure_running(&self) -> Result<(), String> {
        let mut guard = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let alive = guard
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
        if !alive {
            *guard = None;
        }
        if guard.is_none() {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let child = std::process::Command::new(exe)
                .arg("computer-agent")
                .arg("--may-dir")
                .arg(&self.may_dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| format!("spawn computer-agent: {e}"))?;
            *guard = Some(child);
            // The listener needs a beat to bind its socket.
            for _ in 0..30 {
                if self.socket.exists() {
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            return Err("computer-agent did not bind its socket".to_owned());
        }
        Ok(())
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
