//! The `may computer-agent` side of the socket — it owns every session's
//! virtual desktop: displays, capture, WebRTC streams, screenshots and
//! input injection.
//!
//! That isolation is the point: macOS evaluates Screen Recording and
//! Accessibility grants per process at launch, and a helper the daemon
//! respawns picks up a fresh grant while the daemon itself never restarts.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use super::input::{self, InputState};
use super::proto::{AgentRequest, AgentResponse};
use super::stream::OfferError;
use super::{SessionDisplay, SessionDisplays, SessionStreams, permissions, screenshot};

/// Everything the agent holds across requests.
struct AgentState {
    displays: SessionDisplays,
    streams: SessionStreams,
    /// One input context per session — shared by the viewer's
    /// `DataChannel` and the agent `computer` command so focus/drag
    /// state stays coherent.
    inputs: HashMap<String, Arc<Mutex<InputState>>>,
}

/// How often a helper checks that it still owns its socket path.
const OWNERSHIP_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Identity of the file at the socket path: device and inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketId {
    dev: u64,
    ino: u64,
}

impl SocketId {
    /// `None` when nothing exists at `path`.
    fn of(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path).ok()?;
        Some(Self {
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }

    /// Whether the path still holds the file this helper bound.
    fn is_current(self, now: Option<Self>) -> bool {
        now == Some(self)
    }
}

/// Serve requests on `<may_dir>/computer-agent.sock` until killed.
///
/// # Errors
/// Returns a message when the socket cannot be bound.
pub async fn serve(may_dir: &Path) -> Result<(), String> {
    let sock = may_dir.join("computer-agent.sock");
    if sock.exists() {
        std::fs::remove_file(&sock).map_err(|e| e.to_string())?;
    }
    let listener =
        UnixListener::bind(&sock).map_err(|e| format!("bind {}: {e}", sock.display()))?;
    // The daemon reads this to know the helper is alive and to kill it
    // after permission grants — `open` gives no child handle.
    let _ = std::fs::write(
        may_dir.join("computer-agent.pid"),
        std::process::id().to_string(),
    );
    let state = Arc::new(Mutex::new(AgentState {
        displays: SessionDisplays::default(),
        streams: SessionStreams::default(),
        inputs: HashMap::new(),
    }));
    let owned = SocketId::of(&sock).ok_or_else(|| format!("stat {}", sock.display()))?;
    let mut ownership = tokio::time::interval(OWNERSHIP_CHECK_INTERVAL);
    loop {
        let conn = tokio::select! {
            accepted = listener.accept() => accepted.map_err(|e| e.to_string())?.0,
            _ = ownership.tick() => {
                if owned.is_current(SocketId::of(&sock)) {
                    continue;
                }
                // A newer helper replaced the socket, or the may dir is
                // gone: nobody connects here any more.
                tracing::info!(socket = %sock.display(), "computer-agent lost its socket; exiting");
                return Ok(());
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            let (mut reader, mut writer) = conn.into_split();
            let mut line = String::new();
            let response = match BufReader::new(&mut reader).read_line(&mut line).await {
                Ok(_) => match serde_json::from_str(&line) {
                    Ok(request) => dispatch_inner(request, state).await,
                    Err(e) => AgentResponse::Error {
                        error: format!("bad request: {e}"),
                    },
                },
                Err(e) => AgentResponse::Error {
                    error: e.to_string(),
                },
            };
            if let Ok(mut payload) = serde_json::to_vec(&response) {
                payload.push(b'\n');
                let _ = writer.write_all(&payload).await;
            }
        });
    }
}

fn input_state(state: &AgentState, session_id: &str) -> Arc<Mutex<InputState>> {
    state.inputs.get(session_id).cloned().unwrap_or_default()
}

fn input_state_mut(state: &mut AgentState, session_id: &str) -> Arc<Mutex<InputState>> {
    state
        .inputs
        .entry(session_id.to_owned())
        .or_default()
        .clone()
}

#[allow(clippy::significant_drop_tightening)]
async fn dispatch_inner(request: AgentRequest, state: Arc<Mutex<AgentState>>) -> AgentResponse {
    match request {
        AgentRequest::Preflight => match blocking::unblock(permissions::fresh_grants).await {
            Ok(grants) => AgentResponse::Permissions {
                screen_recording: grants.screen_recording,
                accessibility: grants.accessibility,
            },
            Err(error) => AgentResponse::Error { error },
        },
        AgentRequest::EnsureDisplay { session_id } => {
            let result = {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state
                    .displays
                    .acquire(&session_id)
                    .map(|d| (d.display_id(), d.size()))
            };
            match result {
                Ok((display_id, (width, height))) => AgentResponse::Display {
                    display_id,
                    width,
                    height,
                },
                Err(e) => AgentResponse::Error {
                    error: e.to_string(),
                },
            }
        }
        AgentRequest::ReleaseDisplay { session_id } => {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.streams.release(&session_id);
            state.displays.release(&session_id);
            state.inputs.remove(&session_id);
            AgentResponse::Ok
        }
        AgentRequest::Screenshot { session_id } => {
            let display_id = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .displays
                .get(&session_id)
                .map(|d| d.display_id());
            let Some(display_id) = display_id else {
                return AgentResponse::Error {
                    error: "the session has no desktop".to_owned(),
                };
            };
            match tokio::task::spawn_blocking(move || screenshot::screenshot_png(display_id)).await
            {
                Ok(Ok(data)) => AgentResponse::Png { data },
                Ok(Err(e)) => AgentResponse::Error {
                    error: e.to_string(),
                },
                Err(e) => AgentResponse::Error {
                    error: e.to_string(),
                },
            }
        }
        AgentRequest::SetStreamCodec { codec } => {
            let streams = {
                let state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.streams.all()
            };
            for stream in streams {
                if let Err(e) = stream.restart(codec).await {
                    return AgentResponse::Error {
                        error: e.to_string(),
                    };
                }
            }
            AgentResponse::Ok
        }
        AgentRequest::StartStream { session_id, codec } => {
            let (display, stream, inputs) = {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match state.displays.acquire(&session_id) {
                    Ok(display) => {
                        let stream = state.streams.stream(&session_id);
                        let inputs = input_state_mut(&mut state, &session_id);
                        (display, stream, inputs)
                    }
                    Err(e) => {
                        return AgentResponse::Error {
                            error: e.to_string(),
                        };
                    }
                }
            };
            match stream.start(display, inputs, codec).await {
                Ok(()) => AgentResponse::Ok,
                Err(e) => AgentResponse::Error {
                    error: e.to_string(),
                },
            }
        }
        AgentRequest::StopStream { session_id } => {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.streams.release(&session_id);
            AgentResponse::Ok
        }
        AgentRequest::StreamRunning { session_id } => {
            let stream = {
                let state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.streams.get(&session_id)
            };
            let running = match stream {
                Some(stream) => stream.is_running().await,
                None => false,
            };
            AgentResponse::Flag { value: running }
        }
        AgentRequest::Offer { session_id, sdp } => {
            let stream = {
                let state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.streams.get(&session_id)
            };
            match stream {
                Some(stream) => match stream.handle_offer(sdp).await {
                    Ok(answer) => AgentResponse::Sdp { sdp: answer },
                    Err(OfferError::Codec(e)) => AgentResponse::OfferRefused {
                        error: e.to_string(),
                    },
                    Err(OfferError::Display(e)) => AgentResponse::Error {
                        error: e.to_string(),
                    },
                },
                None => AgentResponse::Error {
                    error: "the desktop stream is not running".to_owned(),
                },
            }
        }
        AgentRequest::SetTakeover { session_id, active } => {
            let stream = {
                let state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.streams.get(&session_id)
            };
            stream.map_or_else(
                || AgentResponse::Error {
                    error: "the desktop stream is not running".to_owned(),
                },
                |stream| {
                    stream.set_takeover(active);
                    AgentResponse::Ok
                },
            )
        }
        AgentRequest::TakeoverActive { session_id } => {
            let state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            AgentResponse::Flag {
                value: state
                    .streams
                    .get(&session_id)
                    .is_some_and(|s| s.takeover_active()),
            }
        }
        AgentRequest::Input {
            session_id,
            message,
        } => {
            let (display_id, inputs) = {
                let state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (
                    state.displays.get(&session_id).map(|d| d.display_id()),
                    input_state(&state, &session_id),
                )
            };
            let Some(display_id) = display_id else {
                return AgentResponse::Error {
                    error: "the session has no desktop".to_owned(),
                };
            };
            let mut guard = inputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match input::post_input(display_id, message, &mut guard) {
                Ok(()) => AgentResponse::Ok,
                Err(e) => AgentResponse::Error { error: e },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SocketId;

    #[test]
    fn socket_ownership_follows_the_file_at_the_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.sock");
        std::fs::write(&path, b"").expect("create");
        let owned = SocketId::of(&path).expect("exists");
        assert!(owned.is_current(SocketId::of(&path)));

        // Replaced by another file: a newer helper bound the path.
        std::fs::remove_file(&path).expect("remove");
        let _occupied = std::fs::File::create(dir.path().join("other")).expect("occupy inode");
        std::fs::write(&path, b"").expect("recreate");
        assert!(!owned.is_current(SocketId::of(&path)));

        // Removed outright, as when the may dir is deleted.
        std::fs::remove_file(&path).expect("remove");
        assert!(!owned.is_current(SocketId::of(&path)));
    }
}
