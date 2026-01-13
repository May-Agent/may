//! Event handling for the TUI.

use std::time::Duration;

use async_channel::{Receiver, Sender};
use blocking::unblock;
use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use executor_core::spawn;

use crate::{SessionSummary, SkillInfo};

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
    /// Mouse wheel scroll (positive = up, negative = down)
    Scroll(i16),
    /// Mouse button pressed at (column, row)
    MouseDown { x: u16, y: u16 },
    /// Mouse dragged to (column, row)
    MouseDrag { x: u16, y: u16 },
    /// Mouse button released at (column, row)
    MouseUp { x: u16, y: u16 },

    // ─── Async operation results ───────────────────────────────────────
    /// Session list loaded from background task
    SessionsLoaded(Result<Vec<SessionSummary>, String>),
    /// Session deleted from background task (includes refreshed list)
    SessionDeleted {
        was_current: bool,
        result: Result<Vec<SessionSummary>, String>,
    },

    // ─── Skill operation results ───────────────────────────────────────
    /// Skills list loaded from background task
    SkillsLoaded(Result<Vec<SkillInfo>, String>),
    /// Skill installed from GitHub
    SkillInstalled(Result<Vec<SkillInfo>, String>),
    /// Skill deleted
    SkillDeleted(Result<Vec<SkillInfo>, String>),

    // ─── Image loading ────────────────────────────────────────────────
    /// Image fetched from URL
    ImageFetched { url: String, data: Vec<u8> },
}

/// Sender type for sending events back to the main loop.
pub type EventSender = Sender<AppEvent>;

/// Event handler that polls terminal events.
pub struct EventHandler {
    rx: Receiver<AppEvent>,
    tx: EventSender,
}

impl EventHandler {
    /// Create a new event handler.
    pub fn new() -> Self {
        let (tx, rx) = async_channel::unbounded();
        let tx_clone = tx.clone();

        // Spawn event polling task
        spawn(async move {
            loop {
                // Poll with timeout for tick events
                let has_event = unblock(|| event::poll(Duration::from_millis(50)))
                    .await
                    .expect(ERR_TUI_EVENT_POLL.trim());
                if has_event {
                    let evt = unblock(event::read)
                        .await
                        .expect(ERR_TUI_EVENT_POLL.trim());
                    let app_event = match evt {
                        Event::Key(key) => AppEvent::Key(key.code, key.modifiers),
                        Event::Resize(w, h) => AppEvent::Resize(w, h),
                        Event::Paste(text) => AppEvent::Paste(text),
                        Event::Mouse(mouse) => match mouse.kind {
                            MouseEventKind::ScrollUp => AppEvent::Scroll(1),
                            MouseEventKind::ScrollDown => AppEvent::Scroll(-1),
                            MouseEventKind::Down(MouseButton::Left) => {
                                AppEvent::MouseDown {
                                    x: mouse.column,
                                    y: mouse.row,
                                }
                            }
                            MouseEventKind::Drag(MouseButton::Left) => {
                                AppEvent::MouseDrag {
                                    x: mouse.column,
                                    y: mouse.row,
                                }
                            }
                            MouseEventKind::Up(MouseButton::Left) => {
                                AppEvent::MouseUp {
                                    x: mouse.column,
                                    y: mouse.row,
                                }
                            }
                            _ => continue,
                        },
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
