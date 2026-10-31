//! One virtual desktop per session: a display the agent can see and
//! drive, streamed to the web UI, torn down with its session.
//!
//! The platform seam is [`SessionDisplay`]: the macOS backend is a
//! SkyLight `CGVirtualDisplay` ([`skylight`]); a Linux backend lands
//! behind the same trait later.

#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
pub(crate) mod input;
#[cfg(target_os = "macos")]
pub(crate) mod screenshot;
#[cfg(target_os = "macos")]
mod skylight;
#[cfg(target_os = "macos")]
mod stream;

#[cfg(target_os = "macos")]
use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::sync::Arc;

/// A session's virtual display: created on demand, destroyed on drop —
/// the platform object dies with its session entry.
pub trait SessionDisplay: Send + Sync {
    /// The display's CoreGraphics id, for capture and input targeting.
    fn display_id(&self) -> u32;
    /// `(width, height)` in points.
    fn size(&self) -> (u32, u32);
}

#[cfg(target_os = "macos")]
pub use stream::SessionStream;

/// The display a platform hands a session. `Error` is a plain message:
/// virtual displays fail only one way — the platform refused.
#[derive(Debug, thiserror::Error)]
#[error("the virtual display failed: {0}")]
pub struct DisplayError(pub String);

/// The platform's display backend: macOS is `SkyLight`, everything else
/// fails until a backend lands for it.
#[cfg(target_os = "macos")]
pub type PlatformDisplay = skylight::SkylightDisplay;

/// Creates a session's virtual display on demand.
///
/// `name` is the display's label in display lists (the session id's
/// short form); `size` is in points at 1x — the backend applies 2x
/// scaling for the framebuffer.
///
/// # Errors
/// Returns [`DisplayError`] when the platform refuses the display —
/// unsupported OS, missing entitlement, or a `SkyLight` failure.
#[cfg(target_os = "macos")]
pub fn create_display(name: &str, size: (u32, u32)) -> Result<PlatformDisplay, DisplayError> {
    PlatformDisplay::create(name, size, 60.0)
}

/// The daemon's live displays, one per session that asked for one.
/// Entries drop — and so destroy their displays — when their session
/// ends or the daemon stops.
#[cfg(target_os = "macos")]
#[derive(Default)]
pub struct SessionDisplays {
    displays: HashMap<String, Arc<PlatformDisplay>>,
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
    /// Returns [`DisplayError`] as [`create_display`] does.
    pub fn acquire(&mut self, session_id: &str) -> Result<Arc<PlatformDisplay>, DisplayError> {
        if let Some(display) = self.displays.get(session_id) {
            return Ok(display.clone());
        }
        let short = session_id.get(..8).unwrap_or(session_id);
        let display = Arc::new(create_display(&format!("May {short}"), (1920, 1200))?);
        self.displays.insert(session_id.to_owned(), display.clone());
        Ok(display)
    }

    /// Ends the session's display: nothing else touches it afterwards.
    pub fn release(&mut self, session_id: &str) {
        self.displays.remove(session_id);
    }

    /// The session's display if it has one, without creating it.
    #[must_use]
    pub fn get(&self, session_id: &str) -> Option<Arc<PlatformDisplay>> {
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

    /// Ends the session's stream when its session ends.
    pub fn release(&mut self, session_id: &str) {
        self.streams.remove(session_id);
    }

    /// Ends every stream — the daemon's sessions are all gone.
    pub fn clear(&mut self) {
        self.streams.clear();
    }
}
