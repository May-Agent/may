//! The `may computer-agent` side of the socket — it owns every session's
//! virtual desktop: displays, capture, WebRTC streams, screenshots and
//! input injection.
//!
//! That isolation is the point: macOS evaluates Screen Recording and
//! Accessibility grants per process at launch, and a helper the daemon
//! respawns picks up a fresh grant while the daemon itself never restarts.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use waterkit_permission::{Permission, PermissionError, PermissionStatus};

use super::input::InputError;
use super::proto::{AgentRequest, AgentResponse};
use super::stream::OfferError;
use super::{SessionDisplay, SessionDisplays, SessionStreams, launch, screenshot};

/// Everything the agent holds across requests.
struct AgentState {
    /// Held across a display's creation, which waits on the main thread, so
    /// two asks for one session create one display.
    displays: async_lock::Mutex<SessionDisplays>,
    streams: Mutex<SessionStreams>,
}

impl AgentState {
    fn streams(&self) -> std::sync::MutexGuard<'_, SessionStreams> {
        self.streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
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
    let state = Arc::new(AgentState {
        displays: async_lock::Mutex::new(SessionDisplays::default()),
        streams: Mutex::new(SessionStreams::default()),
    });
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
                    Ok(request) => dispatch(request, &state).await,
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

/// The helper's grants as a fresh process reads them.
async fn preflight() -> AgentResponse {
    let (screen_recording, accessibility) = futures_lite::future::zip(
        waterkit_permission::check(Permission::ScreenRecording),
        waterkit_permission::check(Permission::Accessibility),
    )
    .await;
    AgentResponse::Permissions {
        screen_recording: screen_recording == PermissionStatus::Granted,
        accessibility: accessibility == PermissionStatus::Granted,
    }
}

/// Raises the system prompts for the grants the helper lacks. Each request
/// answers once its prompt is up, while the user may still be deciding.
///
/// From macOS 15 the first capture that bypasses the system window picker
/// is what makes `replayd` ask for a one-time consent, so once Screen
/// Recording is granted the direct-capture consent is requested too.
async fn request_permissions() -> Result<(), PermissionError> {
    waterkit_permission::request(Permission::Accessibility).await?;
    let screen_recording = waterkit_permission::request(Permission::ScreenRecording).await?;
    if screen_recording == PermissionStatus::Granted {
        waterkit_permission::request(Permission::DirectScreenCapture).await?;
    }
    Ok(())
}

#[allow(clippy::significant_drop_tightening)]
async fn dispatch(request: AgentRequest, state: &AgentState) -> AgentResponse {
    match request {
        AgentRequest::Preflight => preflight().await,
        AgentRequest::RequestPermissions => match request_permissions().await {
            Ok(()) => AgentResponse::Ok,
            Err(e) => AgentResponse::Error {
                error: e.to_string(),
            },
        },
        AgentRequest::EnsureDisplay { session_id } => {
            match state.displays.lock().await.acquire(&session_id).await {
                Ok(display) => {
                    let (width, height) = display.size();
                    AgentResponse::Display {
                        display_id: display.display_id(),
                        width,
                        height,
                    }
                }
                Err(e) => AgentResponse::Error {
                    error: e.to_string(),
                },
            }
        }
        AgentRequest::ReleaseDisplay { session_id } => {
            state.streams().release(&session_id);
            state.displays.lock().await.release(&session_id);
            AgentResponse::Ok
        }
        AgentRequest::Screenshot { session_id } => {
            let display_id = state
                .displays
                .lock()
                .await
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
            let streams = state.streams().all();
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
            let display = match state.displays.lock().await.acquire(&session_id).await {
                Ok(display) => display,
                Err(e) => {
                    return AgentResponse::Error {
                        error: e.to_string(),
                    };
                }
            };
            let stream = state.streams().stream(&session_id);
            match stream.start(display, codec).await {
                Ok(()) => AgentResponse::Ok,
                Err(e) => AgentResponse::Error {
                    error: e.to_string(),
                },
            }
        }
        AgentRequest::StopStream { session_id } => {
            state.streams().release(&session_id);
            AgentResponse::Ok
        }
        AgentRequest::StreamRunning { session_id } => {
            let stream = state.streams().get(&session_id);
            let running = match stream {
                Some(stream) => stream.is_running().await,
                None => false,
            };
            AgentResponse::Flag { value: running }
        }
        AgentRequest::Offer { session_id, sdp } => {
            let stream = state.streams().get(&session_id);
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
            let stream = state.streams().get(&session_id);
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
        AgentRequest::TakeoverActive { session_id } => AgentResponse::Flag {
            value: state
                .streams()
                .get(&session_id)
                .is_some_and(|s| s.takeover_active()),
        },
        AgentRequest::Input {
            session_id,
            message,
        } => {
            let display = match session_display(state, &session_id).await {
                Ok(display) => display,
                Err(response) => return response,
            };
            match display.post_input(message).await {
                Ok(()) => AgentResponse::Ok,
                Err(e) => AgentResponse::Error {
                    error: e.to_string(),
                },
            }
        }
        AgentRequest::Launch {
            session_id,
            app,
            items,
        } => {
            let display = match session_display(state, &session_id).await {
                Ok(display) => display,
                Err(response) => return response,
            };
            match launch::launch(app, items, display.display_id()).await {
                Ok(app) => AgentResponse::Launched { app },
                Err(error) => AgentResponse::Error { error },
            }
        }
        AgentRequest::Elements { session_id } => {
            let display = match session_display(state, &session_id).await {
                Ok(display) => display,
                Err(response) => return response,
            };
            match display.elements().await {
                Ok(listing) => AgentResponse::Elements { listing },
                Err(e) => AgentResponse::Error {
                    error: e.to_string(),
                },
            }
        }
        AgentRequest::Press {
            session_id,
            element,
        } => {
            let display = match session_display(state, &session_id).await {
                Ok(display) => display,
                Err(response) => return response,
            };
            element_outcome(display.press(element).await)
        }
        AgentRequest::TypeInto {
            session_id,
            element,
            text,
        } => {
            let display = match session_display(state, &session_id).await {
                Ok(display) => display,
                Err(response) => return response,
            };
            element_outcome(display.type_into(element, text).await)
        }
    }
}

/// The session's display, or the error answer when it has none.
async fn session_display(
    state: &AgentState,
    session_id: &str,
) -> Result<Arc<SessionDisplay>, AgentResponse> {
    state
        .displays
        .lock()
        .await
        .get(session_id)
        .ok_or_else(|| AgentResponse::Error {
            error: "the session has no desktop".to_owned(),
        })
}

/// The answer to an action on an element by reference: a reference that no
/// longer names an element of the latest listing is told apart, so the
/// agent lists again.
fn element_outcome(result: Result<(), InputError>) -> AgentResponse {
    match result {
        Ok(()) => AgentResponse::Ok,
        Err(InputError::Deliver(
            error @ (waterkit_automation::Error::StaleElement { .. }
            | waterkit_automation::Error::UnknownElement { .. }
            | waterkit_automation::Error::ElementGone(_)),
        )) => AgentResponse::StaleElement {
            error: error.to_string(),
        },
        Err(error) => AgentResponse::Error {
            error: error.to_string(),
        },
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
