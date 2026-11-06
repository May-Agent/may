//! One virtual desktop per session: a display the agent can see and
//! drive, streamed to the web UI, torn down with its session.
//!
//! The platform seam is [`SessionDisplay`]: the macOS backend is a
//! `waterkit_automation::VirtualDisplay` plus its `Input`; a Linux backend
//! lands behind the same type later.

#[cfg(target_os = "macos")]
pub mod agent;
#[cfg(target_os = "macos")]
mod capture;
pub(crate) mod client;
#[cfg(target_os = "macos")]
pub(crate) mod input;
#[cfg(target_os = "macos")]
pub mod main_thread;
pub(crate) mod proto;
#[cfg(target_os = "macos")]
pub(crate) mod screenshot;
#[cfg(target_os = "macos")]
mod stream;

pub use client::ComputerAgent;

#[cfg(target_os = "macos")]
use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::num::NonZeroU32;
#[cfg(target_os = "macos")]
use std::sync::Arc;

#[cfg(target_os = "macos")]
#[cfg(target_os = "macos")]
use main_thread::MainThreadBound;
#[cfg(target_os = "macos")]
use waterkit_automation::{Input, VirtualDisplay, VirtualDisplayConfig};

#[cfg(target_os = "macos")]
pub use stream::SessionStream;

/// The display a platform hands a session. `Error` is a plain message:
/// virtual displays fail only one way — the platform refused.
#[derive(Debug, thiserror::Error)]
#[error("the virtual display failed: {0}")]
pub struct DisplayError(pub String);

/// A session's virtual display and the input that drives it: created on
/// demand, destroyed on drop — the platform object dies with its session
/// entry.
#[cfg(target_os = "macos")]
pub struct SessionDisplay {
    /// The display, owned by the main thread; dropped there.
    display: Option<MainThreadBound<VirtualDisplay>>,
    display_id: u32,
    size: (u32, u32),
    /// Messages for the task that owns the session's one `Input`, shared by
    /// the viewer's `DataChannel` and the agent `computer` command so focus
    /// and drag state stay coherent.
    posts: async_channel::Sender<input::Posting>,
}

/// Input messages queued for a display's owner task.
#[cfg(target_os = "macos")]
const INPUT_QUEUE: usize = 64;

#[cfg(target_os = "macos")]
impl std::fmt::Debug for SessionDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionDisplay")
            .field("display_id", &self.display_id)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "macos")]
impl Drop for SessionDisplay {
    fn drop(&mut self) {
        if let Some(display) = self.display.take() {
            main_thread::release(display);
        }
    }
}

#[cfg(target_os = "macos")]
impl SessionDisplay {
    /// Creates a display `size` points big at 2x backing scale named `name`
    /// (the label in display lists). `serial` identifies it to the system,
    /// which keeps a display's arrangement by serial.
    ///
    /// The display is created on the main thread; the caller awaits.
    ///
    /// # Errors
    /// Returns [`DisplayError`] when the size is empty or waterkit cannot
    /// bring the display up or drive it.
    pub async fn create(name: String, size: (u32, u32), serial: u32) -> Result<Self, DisplayError> {
        let extent = |points: u32| {
            NonZeroU32::new(points).ok_or_else(|| DisplayError("the display has no area".into()))
        };
        let config = VirtualDisplayConfig::new(name, extent(size.0)?, extent(size.1)?, serial);
        let (display, display_id) = main_thread::run(move |main| {
            VirtualDisplay::new(&config, main).map(|display| {
                let id = display.id();
                (MainThreadBound::new(display, main), id)
            })
        })
        .await
        .map_err(|e| DisplayError(e.to_string()))?;
        let input = match Input::new(display_id) {
            Ok(input) => input,
            Err(e) => {
                // Dropping the display here would wait on the main thread.
                main_thread::release(display);
                return Err(DisplayError(e.to_string()));
            }
        };
        let (posts, queue) = async_channel::bounded(INPUT_QUEUE);
        // Ends when `posts` closes with this display.
        let owner = std::thread::Builder::new()
            .name(format!("computer-input-{display_id}"))
            .spawn(move || input::own(input, &queue));
        if let Err(e) = owner {
            main_thread::release(display);
            return Err(DisplayError(format!(
                "the input thread for display {display_id} did not start: {e}"
            )));
        }
        Ok(Self {
            display: Some(display),
            display_id,
            size,
            posts,
        })
    }

    /// The display's CoreGraphics id, for capture and input targeting.
    #[must_use]
    pub const fn display_id(&self) -> u32 {
        self.display_id
    }

    /// `(width, height)` in points.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Posts one wire input message into the display's windows.
    ///
    /// # Errors
    /// Returns [`input::InputError`] when the message cannot be mapped or
    /// delivered.
    pub async fn post_input(&self, message: proto::InputMessage) -> Result<(), input::InputError> {
        let (reply, answer) = async_channel::bounded(1);
        self.posts
            .send(input::Posting {
                message,
                reply: Some(reply),
            })
            .await
            .map_err(|_| input::InputError::OwnerGone)?;
        answer
            .recv()
            .await
            .map_err(|_| input::InputError::OwnerGone)?
    }

    /// Queues one wire input message without waiting for its delivery; the
    /// owner logs a failure at `warn`.
    pub async fn queue_input(&self, message: proto::InputMessage) {
        if self
            .posts
            .send(input::Posting {
                message,
                reply: None,
            })
            .await
            .is_err()
        {
            tracing::warn!("computer input owner is gone");
        }
    }
}

/// The serial a session's display carries: the same session gets the same
/// display identity every time, so the system restores its arrangement.
#[cfg(target_os = "macos")]
fn display_serial(session_id: &str) -> u32 {
    let hash = ahash::RandomState::with_seeds(0, 0, 0, 0).hash_one(session_id);
    // The high half folds into the low one; only the low 32 bits are kept.
    #[expect(clippy::cast_possible_truncation, reason = "folded to 32 bits")]
    let folded = (hash ^ (hash >> 32)) as u32;
    folded
}

/// The daemon's live displays, one per session that asked for one.
/// Entries drop — and so destroy their displays — when their session
/// ends or the daemon stops.
#[cfg(target_os = "macos")]
#[derive(Default)]
pub struct SessionDisplays {
    displays: HashMap<String, Arc<SessionDisplay>>,
}

#[cfg(target_os = "macos")]
impl std::fmt::Debug for SessionDisplays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionDisplays")
            .field("displays", &self.displays.keys())
            .finish()
    }
}

#[cfg(target_os = "macos")]
impl SessionDisplays {
    /// The session's display, creating it on first ask.
    ///
    /// # Errors
    /// Returns [`DisplayError`] as [`SessionDisplay::create`] does.
    pub async fn acquire(&mut self, session_id: &str) -> Result<Arc<SessionDisplay>, DisplayError> {
        if let Some(display) = self.displays.get(session_id) {
            return Ok(display.clone());
        }
        let short = session_id.get(..8).unwrap_or(session_id);
        let display = Arc::new(
            SessionDisplay::create(
                format!("May {short}"),
                (1920, 1200),
                display_serial(session_id),
            )
            .await?,
        );
        self.displays.insert(session_id.to_owned(), display.clone());
        Ok(display)
    }

    /// Ends the session's display: nothing else touches it afterwards.
    pub fn release(&mut self, session_id: &str) {
        self.displays.remove(session_id);
    }

    /// The session's display if it has one, without creating it.
    #[must_use]
    pub fn get(&self, session_id: &str) -> Option<Arc<SessionDisplay>> {
        self.displays.get(session_id).cloned()
    }

    /// Ends every display — the daemon's sessions are all gone.
    pub fn clear(&mut self) {
        self.displays.clear();
    }
}

/// The daemon's live desktop streams, one per session that started
/// one. Cheap holders are created on first ask; a stopped stream is
/// the surface's asleep state.
#[cfg(target_os = "macos")]
#[derive(Default)]
pub struct SessionStreams {
    streams: HashMap<String, Arc<SessionStream>>,
}

#[cfg(target_os = "macos")]
impl std::fmt::Debug for SessionStreams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStreams")
            .field("streams", &self.streams.keys())
            .finish()
    }
}

#[cfg(target_os = "macos")]
impl SessionStreams {
    /// The session's stream, creating the holder on first ask.
    pub fn stream(&mut self, session_id: &str) -> Arc<SessionStream> {
        self.streams
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(SessionStream::new()))
            .clone()
    }

    /// The session's stream if it has one, without creating it.
    #[must_use]
    pub fn get(&self, session_id: &str) -> Option<Arc<SessionStream>> {
        self.streams.get(session_id).cloned()
    }

    /// Every stream holder, running or asleep.
    #[must_use]
    pub fn all(&self) -> Vec<Arc<SessionStream>> {
        self.streams.values().cloned().collect()
    }

    /// Ends the session's stream when its session ends.
    pub fn release(&mut self, session_id: &str) {
        self.streams.remove(session_id);
    }

    /// Ends every stream — the daemon's sessions are all gone.
    pub fn clear(&mut self) {
        self.streams.clear();
    }
}
