//! The daemon↔computer-agent wire: newline-delimited JSON over the
//! `computer-agent.sock` unix socket. One request per connection.

use serde::{Deserialize, Serialize};

use crate::config::StreamCodec;

/// What the daemon asks the agent to do.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AgentRequest {
    /// TCC grant state of the agent process.
    Preflight,
    /// Raise the system prompts for the missing grants. The agent runs from
    /// its app bundle, which is where TCC credits a request.
    RequestPermissions,
    /// Create the session's display if needed; report its id and size.
    EnsureDisplay { session_id: String },
    /// Destroy the session's display (session ended).
    ReleaseDisplay { session_id: String },
    /// PNG of the display's current framebuffer.
    Screenshot { session_id: String },
    /// Begin capturing + accepting WebRTC peers for the session.
    StartStream {
        session_id: String,
        codec: StreamCodec,
    },
    /// Move every running stream to another codec; its viewers reconnect.
    SetStreamCodec { codec: StreamCodec },
    /// Stop the session's stream and its peers.
    StopStream { session_id: String },
    /// Whether the session's stream is up.
    StreamRunning { session_id: String },
    /// WHEP-style offer for a viewer; returns the answer SDP.
    Offer { session_id: String, sdp: String },
    /// A viewer owns (true) or releases (false) the desktop's input.
    SetTakeover { session_id: String, active: bool },
    /// Whether a viewer currently owns the desktop.
    TakeoverActive { session_id: String },
    /// Agent-driven input (the `computer` command path).
    Input {
        session_id: String,
        message: InputMessage,
    },
}

/// What the agent answers.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentResponse {
    /// Operation worked.
    Ok,
    /// Operation failed; `error` is human-readable.
    Error { error: String },
    /// TCC preflight result.
    Permissions {
        screen_recording: bool,
        accessibility: bool,
    },
    /// `ensure_display` result.
    Display {
        display_id: u32,
        width: u32,
        height: u32,
    },
    /// `screenshot` result — PNG bytes.
    Png { data: Vec<u8> },
    /// `offer` result — the answer SDP.
    Sdp { sdp: String },
    /// `offer` refused because of the viewer's offer itself (unparsable, or
    /// without the stream's codec); `error` is human-readable.
    OfferRefused { error: String },
    /// Boolean-bearing replies (`stream_running`, `takeover_active`).
    Flag { value: bool },
}

// ── Wire input messages (the same shape the browser input uses) ────────────────

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InputMessage {
    Mouse(MouseInput),
    Keyboard(KeyboardInput),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MouseInput {
    pub event_type: MouseEventKind,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub button: Option<MouseButtonInput>,
    #[serde(default)]
    pub click_count: Option<i32>,
    #[serde(default)]
    pub delta_x: Option<f64>,
    #[serde(default)]
    pub delta_y: Option<f64>,
    #[serde(default)]
    pub modifiers: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MouseEventKind {
    #[serde(rename = "mousePressed")]
    Pressed,
    #[serde(rename = "mouseReleased")]
    Released,
    #[serde(rename = "mouseMoved")]
    Moved,
    #[serde(rename = "mouseWheel")]
    Wheel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButtonInput {
    Left,
    Right,
    Middle,
}

/// Bits of the `modifiers` field of [`MouseInput`] and [`KeyboardInput`], in
/// the web client's CDP order. Only the macOS input path reads them; the web
/// client sends them as numbers.
#[cfg(target_os = "macos")]
pub mod modifier_bits {
    /// Alt / Option.
    pub const ALT: u64 = 1;
    /// Control.
    pub const CTRL: u64 = 2;
    /// Meta / Command.
    pub const META: u64 = 4;
    /// Shift.
    pub const SHIFT: u64 = 8;
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyboardInput {
    pub event_type: KeyboardEventKind,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub modifiers: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum KeyboardEventKind {
    KeyDown,
    KeyUp,
}
