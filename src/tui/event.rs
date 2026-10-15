//! Event handling for the TUI.

use std::time::Duration;

use async_channel::{Receiver, Sender};
use blocking::unblock;
use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseEvent};
use executor_core::spawn;

use crate::profile::{ProfileInfo, ProfileSessions};
use crate::{SessionSummary, SkillInfo};

use super::error::TuiError;

const ERR_TUI_EVENT_POLL: &str = include_str!("../texts/event_poll_failed.txt");

/// Application events.
#[derive(Debug)]
pub enum AppEvent {
    /// Key press
    Key(KeyCode, KeyModifiers),
    /// Tick (for polling)
    Tick,
    /// Terminal resize
    Resize(u16, u16),
    /// Pasted text (from bracketed paste)
    Paste(String),
    /// Mouse event (for wheel scrolling in alternate screen)
    Mouse(MouseEvent),

    // ─── Async operation results ───────────────────────────────────────
    /// Session list loaded from background task
    SessionsLoaded(Result<Vec<SessionSummary>, TuiError>),
    /// Session deleted from background task (includes refreshed list)
    SessionDeleted {
        /// The session removed from the database, when deletion got that far.
        deleted: Option<crate::session::SessionId>,
        /// Whether the deleted session was the active one.
        was_current: bool,
        /// Refreshed session list after the delete, or the failure.
        result: Result<Vec<SessionSummary>, TuiError>,
    },

    // ─── Skill operation results ───────────────────────────────────────
    /// Skills list loaded from background task
    SkillsLoaded(Result<Vec<SkillInfo>, TuiError>),
    /// Skill installed from GitHub
    SkillInstalled(Result<Vec<SkillInfo>, TuiError>),
    /// Skill deleted
    SkillDeleted(Result<Vec<SkillInfo>, TuiError>),

    // ─── Profile operation results ──────────────────────────────────
    /// Profile list loaded (for `ProfileSelect` mode)
    ProfilesLoaded(Result<Vec<ProfileInfo>, TuiError>),
    /// Profile sessions loaded (pinned/regular/archived groups)
    ProfileSessionsLoaded(Result<ProfileSessions, TuiError>),

    /// Background config save completed.
    ConfigSaved(Result<(), TuiError>),

    // ─── Image loading ────────────────────────────────────────────────
    /// Image fetch finished from URL (data=None means fetch failed)
    ImageFetchResult {
        /// URL the image was fetched from.
        url: String,
        /// Fetched image bytes; `None` when the fetch failed.
        data: Option<Vec<u8>>,
    },
}

/// Sender type for sending events back to the main loop.
pub type EventSender = Sender<AppEvent>;

/// Event handler that polls terminal events.
#[derive(Debug)]
pub struct EventHandler {
    rx: Receiver<AppEvent>,
    tx: EventSender,
}

impl EventHandler {
    /// Create a new event handler.
    ///
    /// # Panics
    ///
    /// Panics if the spawned terminal-event polling thread fails to start.
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = async_channel::unbounded();
        let tx_clone = tx.clone();

        // Spawn event polling task
        spawn(async move {
            loop {
                // Poll with timeout for tick events
                let has_event = unblock(|| event::poll(Duration::from_millis(50)))
                    .await
                    .unwrap_or_else(|e| panic!("{}: {}", ERR_TUI_EVENT_POLL.trim(), e));
                if has_event {
                    let evt = unblock(event::read)
                        .await
                        .unwrap_or_else(|e| panic!("{}: {}", ERR_TUI_EVENT_POLL.trim(), e));
                    let app_event = match evt {
                        Event::Key(key) => AppEvent::Key(key.code, key.modifiers),
                        Event::Resize(w, h) => AppEvent::Resize(w, h),
                        Event::Paste(text) => AppEvent::Paste(text),
                        Event::Mouse(mouse) => AppEvent::Mouse(mouse),
                        _ => continue,
                    };
                    if tx_clone.send(app_event).await.is_err() {
                        break;
                    }
                } else if tx_clone.send(AppEvent::Tick).await.is_err() {
                    break;
                }
            }
        })
        .detach();

        Self { rx, tx }
    }

    /// Get a clone of the sender for sending events from background tasks.
    #[must_use]
    pub fn sender(&self) -> EventSender {
        self.tx.clone()
    }

    /// Get the next event.
    pub async fn next(&mut self) -> Option<AppEvent> {
        self.rx.recv().await.ok()
    }
}

impl Default for EventHandler {
    fn default() -> Self {
        Self::new()
    }
}
