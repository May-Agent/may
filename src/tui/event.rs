//! Event handling for the TUI.

use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseEventKind};
use tokio::sync::mpsc;

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
}

/// Event handler that polls terminal events.
pub struct EventHandler {
    rx: mpsc::UnboundedReceiver<AppEvent>,
    _tx: mpsc::UnboundedSender<AppEvent>,
}

impl EventHandler {
    /// Create a new event handler.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let tx_clone = tx.clone();

        // Spawn event polling task
        tokio::spawn(async move {
            loop {
                // Poll with timeout for tick events
                if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                    if let Ok(evt) = event::read() {
                        let app_event = match evt {
                            Event::Key(key) => AppEvent::Key(key.code, key.modifiers),
                            Event::Resize(w, h) => AppEvent::Resize(w, h),
                            Event::Paste(text) => AppEvent::Paste(text),
                            Event::Mouse(mouse) => match mouse.kind {
                                MouseEventKind::ScrollUp => AppEvent::Scroll(1),
                                MouseEventKind::ScrollDown => AppEvent::Scroll(-1),
                                _ => continue,
                            },
                            _ => continue,
                        };
                        if tx_clone.send(app_event).is_err() {
                            break;
                        }
                    }
                } else {
                    // Send tick event
                    if tx_clone.send(AppEvent::Tick).is_err() {
                        break;
                    }
                }
            }
        });

        Self { rx, _tx: tx }
    }

    /// Get the next event.
    pub async fn next(&mut self) -> Option<AppEvent> {
        self.rx.recv().await
    }
}

impl Default for EventHandler {
    fn default() -> Self {
        Self::new()
    }
}
