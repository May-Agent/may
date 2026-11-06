//! The `may computer-agent` side of the socket — it owns every session's
//! virtual desktop: displays, capture, WebRTC streams, screenshots and
//! input injection.
//!
//! That isolation is the point: macOS evaluates Screen Recording and
//! Accessibility grants per process at launch, and a helper the daemon
//! respawns picks up a fresh grant while the daemon itself never restarts.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// How often a running helper checks that its socket is still the one in
/// its May dir.
const SOCKET_WATCH_INTERVAL: Duration = Duration::from_secs(5);

/// Serve requests on `<may_dir>/computer-agent.sock` until killed, or
/// until the socket in the May dir is gone or replaced.
///
/// # Errors
/// Returns a message when the socket cannot be bound or has disappeared.
pub async fn serve(may_dir: &Path) -> Result<(), String> {
    serve_watching(may_dir, SOCKET_WATCH_INTERVAL).await
}

/// [`serve`] with an explicit socket check interval.
async fn serve_watching(may_dir: &Path, watch_interval: Duration) -> Result<(), String> {
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
    let bound_inode = std::fs::metadata(&sock)
        .map_err(|e| format!("stat {}: {e}", sock.display()))?
        .ino();
    let state = Arc::new(Mutex::new(AgentState {
        displays: SessionDisplays::default(),
        streams: SessionStreams::default(),
        inputs: HashMap::new(),
    }));
    let mut watch = tokio::time::interval(watch_interval);
    loop {
        let conn = tokio::select! {
            accepted = listener.accept() => accepted.map_err(|e| e.to_string())?.0,
            _ = watch.tick() => {
                match std::fs::metadata(&sock) {
                    Ok(meta) if meta.ino() == bound_inode => {}
                    Ok(_) => {
                        tracing::error!(socket = %sock.display(), "computer-agent socket was replaced, exiting");
                        return Err(format!("{} was replaced", sock.display()));
                    }
                    Err(e) => {
                        tracing::error!(socket = %sock.display(), error = %e, "computer-agent socket is gone, exiting");
                        return Err(format!("{} is gone: {e}", sock.display()));
                    }
                }
                continue;
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
        AgentRequest::Preflight => AgentResponse::Permissions {
            screen_recording: permissions::screen_recording_granted(),
            accessibility: permissions::accessibility_granted(),
        },
        AgentRequest::RequestPermissions => {
            permissions::request_permissions();
            AgentResponse::Ok
        }
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
                .map(|d| (d.display_id(), d.size()));
            let Some((display_id, size)) = display_id else {
                return AgentResponse::Error {
                    error: "the session has no desktop".to_owned(),
                };
            };
            match screenshot::screenshot_png(display_id, size).await {
                Ok(data) => AgentResponse::Png { data },
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
    use super::*;

    #[tokio::test]
    async fn serve_exits_once_the_may_dir_is_removed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let may_dir = dir.path().join(".may");
        std::fs::create_dir(&may_dir).expect("may dir");
        let server = tokio::spawn({
            let may_dir = may_dir.clone();
            async move { serve_watching(&may_dir, Duration::from_millis(20)).await }
        });
        while !may_dir.join("computer-agent.sock").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        std::fs::remove_dir_all(&may_dir).expect("remove may dir");
        let result = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("helper keeps running after its may dir is gone")
            .expect("serve task");
        assert!(result.is_err_and(|e| e.contains("is gone")));
    }
}
