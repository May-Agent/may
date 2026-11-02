//! The daemon↔computer-agent wire: newline-delimited JSON over the
//! `computer-agent.sock` unix socket. One request per connection.

use serde::{Deserialize, Serialize};

use super::input::InputMessage;

/// What the daemon asks the agent to do.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AgentRequest {
    /// TCC grant state of the agent process.
    Preflight,
    /// Raise the macOS permission prompts (Screen Recording + Accessibility).
    RequestPermissions,
    /// Create the session's display if needed; report its id and size.
    EnsureDisplay { session_id: String },
    /// Destroy the session's display (session ended).
    ReleaseDisplay { session_id: String },
    /// PNG of the display's current framebuffer.
    Screenshot { session_id: String },
    /// Begin capturing + accepting WebRTC peers for the session.
    StartStream { session_id: String },
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
    /// Boolean-bearing replies (`stream_running`, `takeover_active`).
    Flag { value: bool },
}
