//! The daemon↔computer-agent wire: newline-delimited JSON over the
//! `computer-agent.sock` unix socket. One request per connection.

use serde::{Deserialize, Serialize};
#[cfg(target_os = "macos")]
use waterkit_automation::{Element, ElementRef, Elements};

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
    /// Launch an application in the background onto the session's display
    /// and open `items` (URLs, or file paths) in it.
    Launch {
        session_id: String,
        /// An application name, a bundle id or an `.app` path.
        app: String,
        items: Vec<String>,
    },
    /// List the actionable elements of the windows on the session's display;
    /// the references of the previous listing stop resolving.
    #[cfg(target_os = "macos")]
    Elements { session_id: String },
    /// Press the element `element` names.
    #[cfg(target_os = "macos")]
    Press {
        session_id: String,
        #[serde(with = "element_ref")]
        element: ElementRef,
    },
    /// Give the element `element` names keyboard focus, then type `text`.
    #[cfg(target_os = "macos")]
    TypeInto {
        session_id: String,
        #[serde(with = "element_ref")]
        element: ElementRef,
        text: String,
    },
}

/// An [`ElementRef`] on the wire: its text form.
#[cfg(target_os = "macos")]
mod element_ref {
    use serde::{Deserialize, Deserializer, Serializer, de};
    use waterkit_automation::ElementRef;

    pub fn serialize<S: Serializer>(
        element: &ElementRef,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(element)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ElementRef, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// The actionable elements on a session's display, front window first.
#[cfg(target_os = "macos")]
#[derive(Debug, Serialize, Deserialize)]
pub struct ElementListing {
    pub elements: Vec<ListedElement>,
    /// The listing left out the rest of an application or of the display.
    pub truncated: bool,
    /// How many reads of single elements failed, each leaving its element
    /// out or read as empty.
    pub skipped: usize,
}

/// One element of an [`ElementListing`], as its application reported it.
#[cfg(target_os = "macos")]
#[derive(Debug, Serialize, Deserialize)]
pub struct ListedElement {
    #[serde(with = "element_ref")]
    pub reference: ElementRef,
    /// The Accessibility role, such as `AXButton`.
    pub role: String,
    /// The Accessibility subrole, such as `AXSecureTextField`.
    pub subrole: Option<String>,
    pub title: Option<String>,
    pub label: Option<String>,
    pub value: Option<String>,
    pub placeholder: Option<String>,
    pub enabled: bool,
    /// The centre of its frame in the display's points, when it reports one.
    pub center: Option<ElementPoint>,
}

/// A point on the session display, in the space `click X Y` takes.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ElementPoint {
    pub x: f64,
    pub y: f64,
}

#[cfg(target_os = "macos")]
impl From<&Elements> for ElementListing {
    fn from(listed: &Elements) -> Self {
        Self {
            elements: listed.elements().iter().map(ListedElement::from).collect(),
            truncated: listed.truncated(),
            skipped: listed.skipped().len(),
        }
    }
}

#[cfg(target_os = "macos")]
impl From<&Element> for ListedElement {
    fn from(element: &Element) -> Self {
        Self {
            reference: element.reference(),
            role: element.role().to_owned(),
            subrole: element.subrole().map(str::to_owned),
            title: element.title().map(str::to_owned),
            label: element.label().map(str::to_owned),
            value: element.value().map(str::to_owned),
            placeholder: element.placeholder().map(str::to_owned),
            enabled: element.enabled(),
            center: element.frame().map(|frame| {
                let center = frame.center();
                ElementPoint {
                    x: center.x(),
                    y: center.y(),
                }
            }),
        }
    }
}

/// An application launched onto the session's display.
#[derive(Debug, Serialize, Deserialize)]
pub struct LaunchedApp {
    pub bundle_id: String,
    pub pid: i32,
    /// The windows the launch opened and placed on the display.
    pub windows: Vec<LaunchedWindow>,
}

/// A window the launch placed, in the session display's own points: the
/// space `click X Y` takes.
#[derive(Debug, Serialize, Deserialize)]
pub struct LaunchedWindow {
    pub id: u32,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
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
    /// `launch` result.
    Launched { app: LaunchedApp },
    /// `elements` result.
    #[cfg(target_os = "macos")]
    Elements { listing: ElementListing },
    /// An element request refused because its reference no longer names an
    /// element of the latest listing; `error` is human-readable.
    StaleElement { error: String },
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
