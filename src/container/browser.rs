//! Browser automation with WebRTC live streaming.
//!
//! Architecture:
//! 1. chromiumoxide launches Chrome and manages CDP protocol
//! 2. CDP screencast frames are captured and piped to a dedicated encoding thread
//! 3. H.264 encoding via `OpenH264` with Screen Content mode (optimized for UI/text)
//! 4. Encoded video streamed to clients via WebRTC media tracks (RTP/UDP)
//! 5. User input received via WebRTC `DataChannel`, forwarded as CDP commands
//!
//! Encoding backend:
//! - Default: `OpenH264` (compiled from C source, Screen Content Real-Time mode)
//! - Optional: `GStreamer` (hardware accel via VideoToolbox/NVENC/VAAPI, AV1 SCC)
//!   Enable with `--features gstreamer`. Requires `GStreamer` installed on system.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use base64::Engine;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::accessibility::{
    AxNode, AxProperty, AxValue, EnableParams as AccessibilityEnableParams,
    GetFullAxTreeParams as GetFullAccessibilityTreeParams,
};
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    MouseButton,
};
use chromiumoxide::cdp::browser_protocol::page::{
    AddScriptToEvaluateOnNewDocumentParams, CaptureScreenshotFormat, EventScreencastFrame,
    ScreencastFrameAckParams, StartScreencastFormat, StartScreencastParams,
};
use chromiumoxide::cdp::browser_protocol::target::{TargetId, TargetInfo};
use chromiumoxide::error::CdpError;
use chromiumoxide::page::Page;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide_fetcher::{
    BrowserFetcher, BrowserFetcherOptions, BrowserKind as ManagedBrowserKind,
    BrowserVersion as ManagedBrowserVersion, Channel as ManagedBrowserChannel,
};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot, watch};
use walkdir::WalkDir;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MIME_TYPE_H264, MediaEngine};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::browser_service::{
    BrowserAccessibilityNode, BrowserAccessibilityProperty, BrowserAccessibilitySnapshot,
    BrowserCapturedScreenshot, BrowserInteractiveElement, BrowserPageCapture, BrowserPageSummary,
    BrowserReadDetail, BrowserReadOptions, BrowserScreenshotMode,
};
use crate::config::{BrowserBackendKind, BrowserConfig as MayBrowserConfig};

// ── Error ──────────────────────────────────────────────────────────────────────

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Validation failure for a browser input payload received over the `DataChannel`.
#[derive(Debug, thiserror::Error)]
pub enum BrowserInputError {
    /// A `navigate` input arrived with an empty URL.
    /// Navigate input requires a non-empty url.
    #[error("navigate input requires a non-empty url")]
    EmptyNavigationUrl,
    /// A press/release mouse input arrived without naming a button.
    #[error("mouse input with event type {event_type:?} requires a button")]
    MissingMouseButton {
        /// Mouse event type that required a button.
        event_type: MouseEventType,
    },
}

/// Failures from the managed browser runtime and its WebRTC pipeline.
#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    /// No browser session is currently running.
    #[error("Browser not running")]
    NotRunning,
    /// The managed browser binary/profile has not been provisioned yet.
    #[error("Managed browser runtime is not provisioned")]
    NotProvisioned,
    /// The browser actor's command channel closed mid-operation.
    #[error("browser actor unexpectedly closed while handling {operation}")]
    ActorClosed {
        /// Operation being handled when the actor exited.
        operation: &'static str,
    },
    /// A CDP browser operation failed.
    #[error("browser operation failed during {operation}")]
    BrowserOperation {
        /// Operation that failed.
        operation: &'static str,
        /// Underlying driver error.
        #[source]
        source: BoxError,
    },
    /// A WebRTC negotiation or track operation failed.
    /// Webrtc operation failed during.
    #[error("webrtc operation failed during {operation}")]
    WebRtcOperation {
        /// Operation that failed.
        operation: &'static str,
        /// Underlying driver error.
        #[source]
        source: BoxError,
    },
    /// A managed-browser provisioning step failed.
    #[error("provisioning operation failed during {operation}")]
    ProvisioningOperation {
        /// Operation that failed.
        operation: &'static str,
        /// Underlying error.
        #[source]
        source: BoxError,
    },
    /// A `DataChannel` input message failed to deserialize.
    #[error("browser input payload is invalid")]
    InvalidInputPayload {
        /// JSON decoding error.
        #[source]
        source: serde_json::Error,
    },
    /// The input payload failed validation.
    #[error(transparent)]
    InvalidInput(#[from] BrowserInputError),
    /// The managed browser failed to start.
    #[error(transparent)]
    ManagedBrowserStartup(#[from] ManagedBrowserStartupError),
    /// No usable page appeared during startup.
    #[error(transparent)]
    BrowserPageReady(#[from] BrowserPageReadyError),
}

impl BrowserError {
    fn browser_operation(
        operation: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::BrowserOperation {
            operation,
            source: Box::new(source),
        }
    }

    fn web_rtc_operation(
        operation: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::WebRtcOperation {
            operation,
            source: Box::new(source),
        }
    }

    fn provisioning_operation(
        operation: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::ProvisioningOperation {
            operation,
            source: Box::new(source),
        }
    }
}

/// Failures while bringing up the managed Chrome process.
#[derive(Debug, thiserror::Error)]
pub enum ManagedBrowserStartupError {
    /// The debugger metadata endpoint responded but lacked `webSocketDebuggerUrl`.
    #[error("managed browser debugger metadata at {url} did not include webSocketDebuggerUrl")]
    MissingWebSocketDebuggerUrl {
        /// Metadata endpoint that was queried.
        url: reqwest::Url,
    },
    /// The debugger metadata endpoint did not return decodable JSON.
    /// Failed to decode managed browser debugger metadata from.
    /// Failed to decode managed browser debugger metadata from.
    #[error("failed to decode managed browser debugger metadata from {url}")]
    DecodeDebuggerMetadata {
        /// Metadata endpoint that was queried.
        url: reqwest::Url,
        /// HTTP/JSON decoding error.
        #[source]
        source: reqwest::Error,
    },
    /// The debugger websocket endpoint did not appear before the deadline.
    #[error(
        "managed browser did not expose a websocket endpoint on port {port} within {timeout:?}"
    )]
    Timeout {
        /// Port Chrome was expected to listen on.
        port: u16,
        /// How long startup waited before giving up.
        timeout: Duration,
    },
}

/// Failures waiting for the managed browser's initial page.
#[derive(Debug, thiserror::Error)]
pub enum BrowserPageReadyError {
    /// No page target appeared before the deadline.
    #[error("browser did not expose an initial page within {timeout:?}")]
    Timeout {
        /// How long startup waited for an initial page.
        timeout: Duration,
    },
}

// ── DataChannel input messages ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum InputMessage {
    Mouse(MouseInput),
    Keyboard(KeyboardInput),
    Navigate { url: String },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Mouse event kinds accepted over the browser input `DataChannel`,
/// deserialized from `camelCase` JSON.
pub enum MouseEventType {
    /// Button press (`mousePressed`).
    MousePressed,
    /// Button release (`mouseReleased`).
    MouseReleased,
    /// Pointer motion (`mouseMoved`).
    MouseMoved,
    /// Scroll wheel (`mouseWheel`).
    MouseWheel,
}

impl MouseEventType {
    const fn requires_button(&self) -> bool {
        matches!(self, Self::MousePressed | Self::MouseReleased)
    }
}

impl From<MouseEventType> for DispatchMouseEventType {
    fn from(value: MouseEventType) -> Self {
        match value {
            MouseEventType::MousePressed => Self::MousePressed,
            MouseEventType::MouseReleased => Self::MouseReleased,
            MouseEventType::MouseMoved => Self::MouseMoved,
            MouseEventType::MouseWheel => Self::MouseWheel,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MouseButtonInput {
    Left,
    Right,
    Middle,
}

impl From<MouseButtonInput> for MouseButton {
    fn from(value: MouseButtonInput) -> Self {
        match value {
            MouseButtonInput::Left => Self::Left,
            MouseButtonInput::Right => Self::Right,
            MouseButtonInput::Middle => Self::Middle,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MouseInput {
    event_type: MouseEventType,
    x: f64,
    y: f64,
    #[serde(default)]
    button: Option<MouseButtonInput>,
    #[serde(default)]
    click_count: Option<i32>,
    #[serde(default)]
    delta_x: Option<f64>,
    #[serde(default)]
    delta_y: Option<f64>,
    #[serde(default)]
    modifiers: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum KeyboardEventType {
    KeyDown,
    KeyUp,
    Char,
    RawKeyDown,
}

impl From<KeyboardEventType> for DispatchKeyEventType {
    fn from(value: KeyboardEventType) -> Self {
        match value {
            KeyboardEventType::KeyDown => Self::KeyDown,
            KeyboardEventType::KeyUp => Self::KeyUp,
            KeyboardEventType::Char => Self::Char,
            KeyboardEventType::RawKeyDown => Self::RawKeyDown,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyboardInput {
    event_type: KeyboardEventType,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    modifiers: Option<i32>,
}

// ── Internal types ─────────────────────────────────────────────────────────────

/// An encoded video frame ready for WebRTC.
struct EncodedSample {
    data: Vec<u8>,
    duration: Duration,
}

/// Active browser session with all associated tasks.
struct BrowserSession {
    browser: Browser,
    page: Arc<Page>,
    backing: BrowserSessionBacking,
    handler: tokio::task::JoinHandle<()>,
    cdp_reader: tokio::task::JoinHandle<()>,
    sample_writer: tokio::task::JoinHandle<()>,
}

enum BrowserSessionBacking {
    Managed { owns_process: bool },
    RemoteCdp,
}

#[derive(Debug, Clone, Default)]
struct BrowserRuntimeSnapshot {
    running: bool,
    peer_count: usize,
    cdp_endpoint: Option<Arc<str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedBrowserListenerState {
    NotListening,
    Managed,
    ExternalProcess,
}

#[derive(Debug, Clone)]
struct ManagedBrowserPaths {
    executable: PathBuf,
    profile_dir: PathBuf,
}

impl ManagedBrowserPaths {
    fn discover(may_dir: &Path) -> Option<Self> {
        let profile_dir =
            crate::session::MayHomePaths::new(may_dir.to_path_buf()).browser_profile_dir();
        managed_browser_executable(may_dir).map(|executable| Self {
            executable,
            profile_dir,
        })
    }

    fn owns_process_command(&self, command_line: &str) -> bool {
        let profile_arg = format!("--user-data-dir={}", self.profile_dir.display());
        command_line.contains(profile_arg.as_str())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserVersionMetadata {
    web_socket_debugger_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserDomSummary {
    url: String,
    title: String,
    main_heading: Option<String>,
    headings: Vec<String>,
    text_excerpt: String,
    interactive_elements: Vec<BrowserInteractiveElement>,
}

#[derive(Debug, Clone, Copy)]
struct BrowserCaptureLimits {
    text_excerpt_chars: usize,
    interactive_elements: usize,
    accessibility_nodes: usize,
    accessibility_depth: i64,
    settle_delay: Duration,
}

enum BrowserCommand {
    Start {
        reply: oneshot::Sender<Result<(), BrowserError>>,
    },
    Stop {
        reply: oneshot::Sender<Result<(), BrowserError>>,
    },
    HandleOffer {
        /// Session that originated the offer; the actor records it on the
        /// peer so the takeover gate knows which `DataChannel` may drive
        /// browser input.
        session_id: String,
        offer_sdp: String,
        reply: oneshot::Sender<Result<String, BrowserError>>,
    },
    ReadPage {
        url: String,
        options: BrowserReadOptions,
        reply: oneshot::Sender<Result<BrowserPageCapture, BrowserError>>,
    },
    DropPeer {
        peer_id: String,
    },
    /// Mirror takeover ownership from the database into the actor so the
    /// `DataChannel` input gate has a consistent, lock-free view without
    /// async DB round-trips on the hot path.
    SetTakeover {
        session_id: Option<String>,
    },
    /// Resize the rendered viewport to match the live-view panel. Updates
    /// the CDP emulation metrics and re-issues `StartScreencast` so the
    /// next encoded frames carry the new resolution.
    ResizeViewport {
        width: u32,
        height: u32,
        device_scale_factor: f64,
        reply: oneshot::Sender<Result<(), BrowserError>>,
    },
}

/// Sequential, transient lifecycle facts produced by the browser actor.
///
/// Unlike `BrowserStatusEventPayload`, which is a snapshot reconciled on
/// every topic notification, lifecycle events are point-in-time and must
/// be streamed in order. They ride the existing `/api/events` SSE stream
/// as the `browser_lifecycle` event kind (see `AGENTS.md`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum BrowserLifecycleEvent {
    /// Managed browser binary is being downloaded.
    ChromeFetching {
        /// Bytes downloaded so far.
        progress_bytes: u64,
        /// Total download size when the server reports it.
        total_bytes: Option<u64>,
    },
    /// Browser process is launching.
    Starting,
    /// Browser is up and the encoding pipeline is running.
    Ready,
    /// The page is navigating to a new URL.
    Navigating {
        /// URL being navigated to.
        url: String,
    },
    /// A navigation finished and the page is interactive.
    PageLoaded {
        /// URL that finished loading.
        url: String,
        /// Document title after load, when known.
        title: Option<String>,
    },
    /// A WebRTC peer attached to the video stream.
    PeerConnected {
        /// Number of peers attached after this connect.
        count: usize,
    },
    /// A WebRTC peer detached.
    PeerDisconnected {
        /// Number of peers still attached after this disconnect.
        count: usize,
    },
    /// Input takeover ownership moved to another session.
    TakeoverChanged {
        /// Session that now owns input takeover; `None` means the agent owns the page.
        session_id: Option<String>,
    },
    /// The H.264 encoder produced no frame within its watchdog window.
    EncoderStalled,
    /// The browser session shut down.
    Stopped,
    /// An unrecoverable runtime failure occurred.
    Error {
        /// Human-readable failure description.
        message: String,
    },
}

/// Lightweight publisher used by the browser actor to emit lifecycle
/// events without holding an `Option<broadcast::Sender>` everywhere.
/// Cloning is cheap; emissions are fire-and-forget.
#[derive(Debug, Clone)]
pub struct BrowserLifecyclePublisher {
    tx: tokio::sync::broadcast::Sender<BrowserLifecycleEvent>,
}

impl BrowserLifecyclePublisher {
    /// Wrap an existing broadcast sender used to fan out lifecycle events.
    #[must_use]
    pub const fn new(tx: tokio::sync::broadcast::Sender<BrowserLifecycleEvent>) -> Self {
        Self { tx }
    }

    /// A publisher that drops every event. Used by standalone CLI flows
    /// (`may browser` IPC subprocess) where there is no SSE consumer.
    #[must_use]
    pub fn noop() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(1);
        Self { tx }
    }

    /// Publish a lifecycle event; silently dropped when nobody is subscribed.
    pub fn emit(&self, event: BrowserLifecycleEvent) {
        // Fire-and-forget: a closed channel just means nobody is currently
        // subscribed (no SSE clients), which is fine.
        let _ = self.tx.send(event);
    }
}

struct BrowserRuntime {
    session: Option<BrowserSession>,
    video_track: Arc<TrackLocalStaticSample>,
    webrtc_api: webrtc::api::API,
    peers: HashMap<String, Arc<RTCPeerConnection>>,
    lifecycle: BrowserLifecyclePublisher,
    /// Set to `true` when a new peer is about to subscribe to the video
    /// track. The encoder loop swaps it back to `false` and forces an IDR
    /// so the new viewer doesn't have to wait up to 2 s for the next
    /// scheduled keyframe.
    keyframe_request: Arc<AtomicBool>,
    /// The session currently authorised to drive the browser via its
    /// `DataChannel`. `None` means the agent owns the page and any peer
    /// input is silently dropped.
    takeover_owner: Arc<ArcSwapOption<String>>,
}

impl BrowserRuntime {
    fn new(
        video_track: Arc<TrackLocalStaticSample>,
        webrtc_api: webrtc::api::API,
        lifecycle: BrowserLifecyclePublisher,
    ) -> Self {
        Self {
            session: None,
            video_track,
            webrtc_api,
            peers: HashMap::new(),
            lifecycle,
            keyframe_request: Arc::new(AtomicBool::new(false)),
            takeover_owner: Arc::new(ArcSwapOption::empty()),
        }
    }

    fn set_takeover(&self, session_id: Option<String>) {
        let next = session_id.clone().map(Arc::new);
        let previous = self.takeover_owner.load_full();
        let previous_id = previous.as_ref().map(|s| s.as_ref().clone());
        if previous_id == session_id {
            return;
        }
        self.takeover_owner.store(next);
        self.lifecycle
            .emit(BrowserLifecycleEvent::TakeoverChanged { session_id });
    }

    fn publish_snapshot(&self, snapshot_tx: &watch::Sender<BrowserRuntimeSnapshot>) {
        let _ = snapshot_tx.send(BrowserRuntimeSnapshot {
            running: self.session.is_some(),
            peer_count: self.peers.len(),
            cdp_endpoint: self
                .session
                .as_ref()
                .map(|session| Arc::<str>::from(session.browser.websocket_address().as_str())),
        });
    }

    fn drop_peer(&mut self, peer_id: &str) {
        if self.peers.remove(peer_id).is_some() {
            self.lifecycle
                .emit(BrowserLifecycleEvent::PeerDisconnected {
                    count: self.peers.len(),
                });
        }
    }

    async fn resize_viewport(
        &self,
        width: u32,
        height: u32,
        device_scale_factor: f64,
    ) -> Result<(), BrowserError> {
        let session = self.session.as_ref().ok_or(BrowserError::NotRunning)?;
        let page = session.page.clone();
        let dpr = if device_scale_factor.is_finite() && device_scale_factor > 0.0 {
            device_scale_factor
        } else {
            1.0
        };
        let params = SetDeviceMetricsOverrideParams::builder()
            .width(i64::from(width))
            .height(i64::from(height))
            .device_scale_factor(dpr)
            .mobile(false)
            .build()
            .map_err(|error| {
                BrowserError::browser_operation(
                    "build device metrics override params",
                    std::io::Error::other(error),
                )
            })?;
        page.execute(params).await.map_err(|error| {
            BrowserError::browser_operation("set device metrics override", error)
        })?;
        // Force an IDR after a resize: the codec sees a new resolution
        // and the next sample needs to be a keyframe anyway.
        self.keyframe_request.store(true, Ordering::Release);
        Ok(())
    }
}

// ── BrowserManager ─────────────────────────────────────────────────────────────

/// Manages a browser instance with WebRTC live streaming and input forwarding.
///
/// Video pipeline: CDP screencast → JPEG → H.264 (Screen Content) → WebRTC
/// Input pipeline: WebRTC `DataChannel` → JSON → CDP Input.dispatch*
#[derive(Debug)]
pub struct BrowserManager {
    command_tx: mpsc::UnboundedSender<BrowserCommand>,
    snapshot_rx: watch::Receiver<BrowserRuntimeSnapshot>,
}

impl BrowserManager {
    /// Create a new browser manager. Sets up the WebRTC API and shared video track.
    ///
    /// # Errors
    /// Returns [`BrowserError`] when the config fails validation or codec/interceptor registration fails.
    pub fn new(
        config: MayBrowserConfig,
        may_dir: PathBuf,
        lifecycle: BrowserLifecyclePublisher,
    ) -> Result<Self, BrowserError> {
        config
            .validate()
            .map_err(|error| BrowserError::browser_operation("validate browser config", error))?;
        // Register default codecs: H.264, VP8, VP9, AV1 — SDP negotiation picks the best
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .map_err(|error| BrowserError::web_rtc_operation("register default codecs", error))?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine).map_err(|error| {
            BrowserError::web_rtc_operation("register default interceptors", error)
        })?;

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        // Shared video track — all peer connections receive from this single track
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90_000,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                        .to_string(),
                ..Default::default()
            },
            "video".to_owned(),
            "browser-screencast".to_owned(),
        ));
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let actor_tx = command_tx.clone();
        let (snapshot_tx, snapshot_rx) = watch::channel(BrowserRuntimeSnapshot::default());

        tokio::spawn(async move {
            let mut runtime = BrowserRuntime::new(video_track, api, lifecycle);
            runtime.publish_snapshot(&snapshot_tx);

            while let Some(command) = command_rx.recv().await {
                match command {
                    BrowserCommand::Start { reply } => {
                        let result = runtime.start(&config, may_dir.as_path()).await;
                        runtime.publish_snapshot(&snapshot_tx);
                        let _ = reply.send(result);
                    }
                    BrowserCommand::Stop { reply } => {
                        let result = runtime.stop().await;
                        runtime.publish_snapshot(&snapshot_tx);
                        let _ = reply.send(result);
                    }
                    BrowserCommand::HandleOffer {
                        session_id,
                        offer_sdp,
                        reply,
                    } => {
                        let _ = reply.send(
                            runtime
                                .handle_offer(session_id, offer_sdp, actor_tx.clone())
                                .await,
                        );
                    }
                    BrowserCommand::ReadPage {
                        url,
                        options,
                        reply,
                    } => {
                        let _ = reply.send(runtime.read_page(url.as_str(), options).await);
                    }
                    BrowserCommand::DropPeer { peer_id } => {
                        runtime.drop_peer(peer_id.as_str());
                    }
                    BrowserCommand::SetTakeover { session_id } => {
                        runtime.set_takeover(session_id);
                    }
                    BrowserCommand::ResizeViewport {
                        width,
                        height,
                        device_scale_factor,
                        reply,
                    } => {
                        let _ = reply.send(
                            runtime
                                .resize_viewport(width, height, device_scale_factor)
                                .await,
                        );
                    }
                }
            }

            if let Err(error) = runtime.stop().await {
                tracing::warn!(error = %error, "failed to stop browser runtime after actor shutdown");
            }
        });

        Ok(Self {
            command_tx,
            snapshot_rx,
        })
    }

    /// Whether the browser is currently running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.snapshot_rx.borrow().running
    }

    /// Number of active WebRTC peers currently attached to the managed runtime.
    #[must_use]
    pub fn active_peer_count(&self) -> usize {
        self.snapshot_rx.borrow().peer_count
    }

    /// CDP websocket endpoint URL for agent connections.
    #[must_use]
    pub fn cdp_endpoint(&self) -> Option<String> {
        self.snapshot_rx
            .borrow()
            .cdp_endpoint
            .as_ref()
            .map(ToString::to_string)
    }

    /// Launch the browser and start the encoding pipeline.
    ///
    /// # Errors
    /// Returns [`BrowserError::ActorClosed`] if the browser actor is not reachable, or the actor's startup error.
    pub async fn start(&self) -> Result<(), BrowserError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(BrowserCommand::Start { reply: reply_tx })
            .map_err(|_| BrowserError::ActorClosed {
                operation: "start browser",
            })?;
        reply_rx.await.map_err(|_| BrowserError::ActorClosed {
            operation: "await start reply",
        })?
    }

    /// Stop the browser and clean up all resources.
    ///
    /// # Errors
    /// Returns [`BrowserError::ActorClosed`] if the browser actor is not reachable, or the actor's shutdown error.
    pub async fn stop(&self) -> Result<(), BrowserError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(BrowserCommand::Stop { reply: reply_tx })
            .map_err(|_| BrowserError::ActorClosed {
                operation: "stop browser",
            })?;
        reply_rx.await.map_err(|_| BrowserError::ActorClosed {
            operation: "await stop reply",
        })?
    }

    /// Handle a WebRTC SDP offer from a client. Returns the SDP answer.
    ///
    /// Uses WHEP-style single HTTP round-trip signaling:
    /// 1. Client creates offer (recvonly video + `DataChannel`)
    /// 2. Server creates answer with video track + ICE candidates
    /// 3. Connection established, video flows, `DataChannel` ready for input
    ///
    /// `session_id` identifies which May session originated the offer; it
    /// is recorded on the peer so the takeover gate can match input
    /// messages against the current `takeover_owner`.
    ///
    /// # Errors
    /// Returns [`BrowserError::ActorClosed`] if the browser actor is not reachable, or the actor's offer-handling error.
    pub async fn handle_offer(
        &self,
        session_id: String,
        offer_sdp: String,
    ) -> Result<String, BrowserError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(BrowserCommand::HandleOffer {
                session_id,
                offer_sdp,
                reply: reply_tx,
            })
            .map_err(|_| BrowserError::ActorClosed {
                operation: "handle webrtc offer",
            })?;
        reply_rx.await.map_err(|_| BrowserError::ActorClosed {
            operation: "await webrtc offer reply",
        })?
    }

    /// Mirror takeover ownership into the actor. Pass `None` to clear.
    pub fn set_takeover(&self, session_id: Option<String>) {
        let _ = self
            .command_tx
            .send(BrowserCommand::SetTakeover { session_id });
    }

    /// Resize the rendered viewport to match the live-view panel size.
    ///
    /// # Errors
    /// Returns [`BrowserError::ActorClosed`] if the browser actor is not reachable, or the actor's resize error.
    pub async fn resize_viewport(
        &self,
        width: u32,
        height: u32,
        device_scale_factor: f64,
    ) -> Result<(), BrowserError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(BrowserCommand::ResizeViewport {
                width,
                height,
                device_scale_factor,
                reply: reply_tx,
            })
            .map_err(|_| BrowserError::ActorClosed {
                operation: "resize browser viewport",
            })?;
        reply_rx.await.map_err(|_| BrowserError::ActorClosed {
            operation: "await resize viewport reply",
        })?
    }

    /// Read the current page content and capture a snapshot.
    ///
    /// # Errors
    /// Returns [`BrowserError::ActorClosed`] if the browser actor is not reachable, or the actor's page-read error.
    pub async fn read_page(
        &self,
        url: String,
        options: BrowserReadOptions,
    ) -> Result<BrowserPageCapture, BrowserError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(BrowserCommand::ReadPage {
                url,
                options,
                reply: reply_tx,
            })
            .map_err(|_| BrowserError::ActorClosed {
                operation: "read browser page",
            })?;
        reply_rx.await.map_err(|_| BrowserError::ActorClosed {
            operation: "await page read reply",
        })?
    }
}

/// Download the managed Chrome for Testing binary into the May cache directory and return its executable path.
///
/// # Errors
/// Returns [`BrowserError`] when the cache directory cannot be created or the fetcher fails to download the browser.
pub async fn provision_managed_browser(may_dir: &Path) -> Result<PathBuf, BrowserError> {
    let cache_dir = crate::session::MayHomePaths::new(may_dir.to_path_buf()).browser_cache_dir();
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .map_err(|error| BrowserError::provisioning_operation("create browser cache dir", error))?;

    let options = BrowserFetcherOptions::builder()
        .with_path(cache_dir)
        .with_kind(ManagedBrowserKind::Chrome)
        .with_version(ManagedBrowserVersion::Channel(
            ManagedBrowserChannel::Stable,
        ))
        .build()
        .map_err(|error| {
            BrowserError::provisioning_operation("build browser fetcher options", error)
        })?;

    let installation = BrowserFetcher::new(options)
        .fetch()
        .await
        .map_err(|error| BrowserError::provisioning_operation("fetch managed browser", error))?;

    Ok(installation.executable_path)
}

/// Provision the managed browser runtime when the config requires it and seed the profile on first run.
///
/// # Errors
/// Returns [`BrowserError`] when config validation, browser download, or profile seeding fails.
pub async fn prepare_browser_runtime(
    config: &MayBrowserConfig,
    may_dir: &Path,
) -> Result<(), BrowserError> {
    config.validate().map_err(|error| {
        BrowserError::browser_operation("validate browser runtime config", error)
    })?;
    if config.requires_managed_runtime() {
        let _ = provision_managed_browser(may_dir).await?;
        if let Some(seed) = config.system_profile.as_ref() {
            seed_system_profile_if_empty(may_dir, seed).await?;
        }
    }
    Ok(())
}

/// First-run seed for the managed Chrome profile from a real system
/// Chrome user-data-dir. Idempotent: if the managed profile already has
/// any content the seed is skipped, so re-running with `system_profile`
/// set never overwrites accumulated session state.
async fn seed_system_profile_if_empty(
    may_dir: &Path,
    seed: &crate::config::SystemProfileConfig,
) -> Result<(), BrowserError> {
    let target = crate::session::MayHomePaths::new(may_dir.to_path_buf()).browser_profile_dir();

    let already_seeded = match tokio::fs::read_dir(&target).await {
        Ok(mut iter) => iter.next_entry().await.ok().flatten().is_some(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(BrowserError::browser_operation(
                "inspect managed browser profile dir",
                error,
            ));
        }
    };
    if already_seeded {
        return Ok(());
    }

    let source = seed.path.clone();
    if tokio::fs::metadata(&source).await.is_err() {
        return Err(BrowserError::browser_operation(
            "stat system browser profile source",
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("system profile path missing: {}", source.display()),
            ),
        ));
    }

    if seed.copy_on_first_use {
        tokio::fs::create_dir_all(&target).await.map_err(|error| {
            BrowserError::browser_operation("prepare managed browser profile dir", error)
        })?;
        let target_clone = target.clone();
        // fs_extra is a sync API and copying a Chrome profile can move
        // hundreds of MB; do it on the blocking pool so the actor isn't
        // pinned during seeding.
        tokio::task::spawn_blocking(move || copy_dir_recursive(&source, &target_clone))
            .await
            .map_err(|error| {
                BrowserError::browser_operation(
                    "join system profile copy task",
                    std::io::Error::other(error.to_string()),
                )
            })??;
        tracing::info!(
            target = %target.display(),
            "Seeded managed browser profile from system Chrome"
        );
    } else {
        // In-place mode: symlink the source into the managed location
        // so May reads/writes the user's real profile directly.
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                BrowserError::browser_operation("prepare managed browser profile parent", error)
            })?;
        }
        let source_for_symlink = source.clone();
        let target_for_symlink = target.clone();
        tokio::task::spawn_blocking(move || {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&source_for_symlink, &target_for_symlink)
            }
            #[cfg(not(unix))]
            {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "system profile in-place mode requires unix symlinks",
                ))
            }
        })
        .await
        .map_err(|error| {
            BrowserError::browser_operation(
                "join system profile symlink task",
                std::io::Error::other(error.to_string()),
            )
        })?
        .map_err(|error| {
            BrowserError::browser_operation("symlink system profile in place", error)
        })?;
    }

    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), BrowserError> {
    for entry in WalkDir::new(src) {
        let entry =
            entry.map_err(|error| BrowserError::browser_operation("walk system profile", error))?;
        let from = entry.path();
        let relative = from.strip_prefix(src).map_err(|error| {
            BrowserError::browser_operation("strip prefix during profile copy", error)
        })?;
        let to = dst.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&to).map_err(|error| {
                BrowserError::browser_operation("create dir during profile copy", error)
            })?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    BrowserError::browser_operation("create dir during profile copy", error)
                })?;
            }
            std::fs::copy(from, &to).map_err(|error| {
                BrowserError::browser_operation("copy file during profile copy", error)
            })?;
        }
    }
    Ok(())
}

/// Path of the already-provisioned managed browser executable, if present in the cache.
#[must_use]
pub fn provisioned_browser_executable(may_dir: &Path) -> Option<PathBuf> {
    managed_browser_executable(may_dir)
}

/// Kill a stale managed Chrome process on `port` and remove profile lock files left by an unclean shutdown.
pub async fn stop_managed_browser(port: u16, may_dir: &Path) {
    let paths = ManagedBrowserPaths::discover(may_dir);
    cleanup_stale_chrome(port, paths.as_ref()).await;
    cleanup_profile_lock_files(
        crate::session::MayHomePaths::new(may_dir.to_path_buf())
            .browser_profile_dir()
            .as_path(),
    )
    .await;
}

impl BrowserRuntime {
    async fn start(
        &mut self,
        config: &MayBrowserConfig,
        may_dir: &Path,
    ) -> Result<(), BrowserError> {
        if self.session.is_some() {
            return Ok(());
        }

        self.lifecycle.emit(BrowserLifecycleEvent::Starting);

        let result = self.start_inner(config, may_dir).await;
        match &result {
            Ok(()) => self.lifecycle.emit(BrowserLifecycleEvent::Ready),
            Err(error) => self.lifecycle.emit(BrowserLifecycleEvent::Error {
                message: error.to_string(),
            }),
        }
        result
    }

    async fn start_inner(
        &mut self,
        config: &MayBrowserConfig,
        may_dir: &Path,
    ) -> Result<(), BrowserError> {
        let (mut browser, mut handler, backing) = match config.backend {
            BrowserBackendKind::Managed => {
                let paths =
                    ManagedBrowserPaths::discover(may_dir).ok_or(BrowserError::NotProvisioned)?;
                match inspect_managed_browser_listener_state(config.port, &paths).await? {
                    ManagedBrowserListenerState::Managed => {
                        tracing::info!(
                            port = config.port,
                            "Connecting to existing managed browser"
                        );
                        let endpoint = wait_for_local_browser_websocket_endpoint(
                            config.port,
                            managed_browser_launch_timeout(),
                        )
                        .await?;
                        let (browser, handler) =
                            Browser::connect(endpoint).await.map_err(|error| {
                                BrowserError::browser_operation(
                                    "connect to managed browser endpoint",
                                    error,
                                )
                            })?;
                        (
                            browser,
                            handler,
                            BrowserSessionBacking::Managed {
                                owns_process: false,
                            },
                        )
                    }
                    ManagedBrowserListenerState::ExternalProcess => {
                        return Err(BrowserError::browser_operation(
                            "validate managed browser listener ownership",
                            std::io::Error::other(format!(
                                "browser port {} is already in use by a non-May process",
                                config.port
                            )),
                        ));
                    }
                    ManagedBrowserListenerState::NotListening => {
                        cleanup_stale_chrome(config.port, Some(&paths)).await;
                        cleanup_profile_lock_files(paths.profile_dir.as_path()).await;
                        tokio::fs::create_dir_all(&paths.profile_dir)
                            .await
                            .map_err(|error| {
                                BrowserError::browser_operation("create browser profile dir", error)
                            })?;

                        tracing::info!(
                            port = config.port,
                            executable = %paths.executable.display(),
                            profile_dir = %paths.profile_dir.display(),
                            "Launching managed browser"
                        );
                        let browser_config = build_managed_browser_launch_config(
                            config.port,
                            &paths,
                            config.stealth_enabled,
                        )?;
                        let (browser, handler) =
                            Browser::launch(browser_config).await.map_err(|error| {
                                BrowserError::browser_operation("launch managed browser", error)
                            })?;
                        (
                            browser,
                            handler,
                            BrowserSessionBacking::Managed { owns_process: true },
                        )
                    }
                }
            }
            BrowserBackendKind::RemoteCdp => {
                let endpoint = config.remote_cdp_endpoint().map_err(|error| {
                    BrowserError::browser_operation("resolve remote cdp endpoint", error)
                })?;
                let (browser, handler) = Browser::connect(endpoint).await.map_err(|error| {
                    BrowserError::browser_operation("connect to remote cdp endpoint", error)
                })?;
                (browser, handler, BrowserSessionBacking::RemoteCdp)
            }
        };

        let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

        let page = Arc::new(match &backing {
            BrowserSessionBacking::Managed { owns_process } => {
                let timeout = managed_browser_start_operation_timeout();
                tokio::time::timeout(timeout, browser.new_page("about:blank"))
                    .await
                    .map_err(|_| {
                        managed_browser_start_timeout_error(
                            &mut browser,
                            *owns_process,
                            "create managed browser page",
                            timeout,
                        )
                    })?
                    .map_err(|error| {
                        BrowserError::browser_operation("create managed browser page", error)
                    })?
            }
            BrowserSessionBacking::RemoteCdp => acquire_browser_page(&mut browser).await?,
        });

        if config.stealth_enabled {
            // Page.addScriptToEvaluateOnNewDocument runs the shim before
            // any page script across every navigation, so each fresh
            // document boots with the patched fingerprints in place.
            let stealth_params = AddScriptToEvaluateOnNewDocumentParams::builder()
                .source(include_str!("stealth.js"))
                .build()
                .map_err(|error| {
                    BrowserError::browser_operation(
                        "build stealth shim params",
                        std::io::Error::other(error),
                    )
                })?;
            page.execute(stealth_params)
                .await
                .map_err(|error| BrowserError::browser_operation("inject stealth shim", error))?;
        }

        let max_width = i64::from(config.screencast_max_width);
        let max_height = i64::from(config.screencast_max_height);
        let every_nth_frame = i64::from(config.every_nth_frame.max(1));
        let start_screencast = page.execute(
            StartScreencastParams::builder()
                .format(StartScreencastFormat::Jpeg)
                .quality(i64::from(config.screencast_jpeg_quality))
                .max_width(max_width)
                .max_height(max_height)
                .every_nth_frame(every_nth_frame)
                .build(),
        );
        match &backing {
            BrowserSessionBacking::Managed { owns_process } => {
                let timeout = managed_browser_start_operation_timeout();
                tokio::time::timeout(timeout, start_screencast)
                    .await
                    .map_err(|_| {
                        managed_browser_start_timeout_error(
                            &mut browser,
                            *owns_process,
                            "start screencast",
                            timeout,
                        )
                    })?
                    .map_err(|error| BrowserError::browser_operation("start screencast", error))?;
            }
            BrowserSessionBacking::RemoteCdp => {
                start_screencast
                    .await
                    .map_err(|error| BrowserError::browser_operation("start screencast", error))?;
            }
        }

        let (cdp_reader, sample_writer) = start_encoding_pipeline(
            page.clone(),
            self.video_track.clone(),
            config.encoder_initial_bitrate_kbps,
            self.keyframe_request.clone(),
        )
        .await?;

        self.session = Some(BrowserSession {
            browser,
            page,
            backing,
            handler: handler_task,
            cdp_reader,
            sample_writer,
        });

        tracing::info!(backend = ?config.backend, "Browser started with WebRTC streaming");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), BrowserError> {
        let peers = std::mem::take(&mut self.peers);
        for (_, peer) in peers {
            let _ = peer.close().await;
        }

        let was_running = self.session.is_some();
        if let Some(mut session) = self.session.take() {
            session.cdp_reader.abort();
            session.sample_writer.abort();
            session.handler.abort();
            match session.backing {
                BrowserSessionBacking::Managed { owns_process } => {
                    let _ =
                        shutdown_managed_browser_session(&mut session.browser, owns_process).await;
                }
                BrowserSessionBacking::RemoteCdp => {
                    let _ = session.page.as_ref().clone().close().await;
                }
            }
            tracing::info!("Browser stopped");
        }

        if was_running {
            self.lifecycle.emit(BrowserLifecycleEvent::Stopped);
        }

        Ok(())
    }

    async fn handle_offer(
        &mut self,
        session_id: String,
        offer_sdp: String,
        peer_events: mpsc::UnboundedSender<BrowserCommand>,
    ) -> Result<String, BrowserError> {
        let page = self
            .session
            .as_ref()
            .ok_or(BrowserError::NotRunning)?
            .page
            .clone();

        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };

        let peer_connection = Arc::new(
            self.webrtc_api
                .new_peer_connection(config)
                .await
                .map_err(|error| {
                    BrowserError::web_rtc_operation("create peer connection", error)
                })?,
        );

        let video_rtp_sender = peer_connection
            .add_track(Arc::clone(&self.video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .map_err(|error| BrowserError::web_rtc_operation("add video track", error))?;

        // No audio track is offered: there is no audio source plumbed
        // through CDP, and previously we advertised an Opus track that
        // only carried silence. That forced every viewer to negotiate an
        // audio m-line for nothing and drained RTCP into /dev/null.
        let keyframe_request_for_rtcp = self.keyframe_request.clone();
        tokio::spawn(async move {
            // Drain RTCP from the peer and translate viewer feedback into
            // encoder hints. PLI / FIR mean the decoder has lost sync
            // (packet loss, codec reset) and needs an IDR; we flip the
            // shared atomic and the encoder thread emits a keyframe on
            // its next iteration. Other RTCP types (RR / TWCC / REMB)
            // are reserved for the ABR follow-up and currently ignored.
            while let Ok((packets, _)) = video_rtp_sender.read_rtcp().await {
                for packet in packets {
                    let any = packet.as_any();
                    if any
                        .downcast_ref::<webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication>()
                        .is_some()
                        || any
                            .downcast_ref::<webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest>()
                            .is_some()
                    {
                        tracing::debug!("RTCP PLI/FIR received; requesting IDR");
                        keyframe_request_for_rtcp
                            .store(true, Ordering::Release);
                    }
                }
            }
        });

        setup_data_channel_handler(
            &peer_connection,
            page,
            session_id,
            self.takeover_owner.clone(),
        );

        let peer_id = uuid::Uuid::new_v4().to_string();
        setup_peer_connection_lifecycle(&peer_connection, peer_id.clone(), peer_events);

        let offer = RTCSessionDescription::offer(offer_sdp)
            .map_err(|error| BrowserError::web_rtc_operation("parse offer", error))?;
        peer_connection
            .set_remote_description(offer)
            .await
            .map_err(|error| BrowserError::web_rtc_operation("set remote description", error))?;

        let answer = peer_connection
            .create_answer(None)
            .await
            .map_err(|error| BrowserError::web_rtc_operation("create answer", error))?;

        let mut gather_complete = peer_connection.gathering_complete_promise().await;

        // Flip the IDR request *before* the answer is sent so the encoder
        // emits a keyframe in the very next GOP, giving the new viewer a
        // decodable first frame as soon as RTP packets start flowing.
        self.keyframe_request.store(true, Ordering::Release);

        peer_connection
            .set_local_description(answer)
            .await
            .map_err(|error| BrowserError::web_rtc_operation("set local description", error))?;

        tokio::time::timeout(Duration::from_secs(10), gather_complete.recv())
            .await
            .map_err(|_| {
                BrowserError::web_rtc_operation(
                    "wait for ice gathering",
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "ICE gathering timed out"),
                )
            })?;

        let local_desc = peer_connection.local_description().await.ok_or_else(|| {
            BrowserError::web_rtc_operation(
                "read local description",
                std::io::Error::other("no local description after gathering"),
            )
        })?;

        self.peers.insert(peer_id, peer_connection);
        self.lifecycle.emit(BrowserLifecycleEvent::PeerConnected {
            count: self.peers.len(),
        });
        Ok(local_desc.sdp)
    }

    async fn read_page(
        &self,
        url: &str,
        options: BrowserReadOptions,
    ) -> Result<BrowserPageCapture, BrowserError> {
        let page = self
            .session
            .as_ref()
            .ok_or(BrowserError::NotRunning)?
            .page
            .clone();
        self.lifecycle.emit(BrowserLifecycleEvent::Navigating {
            url: url.to_string(),
        });
        let capture = read_page_capture(page, url, options).await?;
        self.lifecycle.emit(BrowserLifecycleEvent::PageLoaded {
            url: capture.page.url.clone(),
            title: Some(capture.page.title.clone()).filter(|t| !t.is_empty()),
        });
        Ok(capture)
    }
}

async fn inspect_managed_browser_listener_state(
    port: u16,
    paths: &ManagedBrowserPaths,
) -> Result<ManagedBrowserListenerState, BrowserError> {
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || {
        inspect_managed_browser_listener_state_blocking(port, &paths)
    })
    .await
    .map_err(|error| {
        BrowserError::browser_operation(
            "inspect managed browser listener state",
            std::io::Error::other(error.to_string()),
        )
    })?
}

fn inspect_managed_browser_listener_state_blocking(
    port: u16,
    paths: &ManagedBrowserPaths,
) -> Result<ManagedBrowserListenerState, BrowserError> {
    let output = std::process::Command::new("lsof")
        .args(["-tiTCP", &port.to_string(), "-sTCP:LISTEN"])
        .output()
        .map_err(|error| BrowserError::browser_operation("run lsof", error))?;
    if !output.status.success() && output.stdout.is_empty() {
        return Ok(ManagedBrowserListenerState::NotListening);
    }

    let pids = String::from_utf8_lossy(&output.stdout);
    if pids.trim().is_empty() {
        return Ok(ManagedBrowserListenerState::NotListening);
    }

    for pid in pids.split_whitespace() {
        let command = std::process::Command::new("ps")
            .args(["-p", pid, "-o", "args="])
            .output()
            .map_err(|error| BrowserError::browser_operation("run ps", error))?;
        if !command.status.success() {
            return Err(BrowserError::browser_operation(
                "inspect listening pid command line",
                std::io::Error::other(format!("ps failed for pid {pid}")),
            ));
        }
        if paths.owns_process_command(String::from_utf8_lossy(&command.stdout).as_ref()) {
            return Ok(ManagedBrowserListenerState::Managed);
        }
    }

    Ok(ManagedBrowserListenerState::ExternalProcess)
}

const fn managed_browser_launch_timeout() -> Duration {
    Duration::from_millis(chromiumoxide::browser::LAUNCH_TIMEOUT)
}

const fn managed_browser_start_operation_timeout() -> Duration {
    Duration::from_secs(5)
}

const fn browser_initial_page_timeout() -> Duration {
    Duration::from_secs(3)
}

fn managed_browser_start_timeout_error(
    browser: &mut Browser,
    owns_process: bool,
    operation: &'static str,
    timeout: Duration,
) -> BrowserError {
    if owns_process {
        match browser.try_wait() {
            Ok(Some(status)) => {
                return BrowserError::browser_operation(
                    operation,
                    std::io::Error::other(format!(
                        "managed browser exited with status {status} during startup"
                    )),
                );
            }
            Ok(None) => {}
            Err(error) => {
                return BrowserError::browser_operation(
                    "inspect managed browser process state",
                    error,
                );
            }
        }
    }

    BrowserError::browser_operation(
        operation,
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("browser startup operation exceeded {timeout:?}"),
        ),
    )
}

fn managed_browser_debugger_metadata_url(port: u16) -> reqwest::Url {
    let mut url =
        reqwest::Url::parse("http://127.0.0.1").expect("static managed browser URL must parse");
    url.set_port(Some(port))
        .expect("managed browser port must be valid");
    url.set_path("/json/version");
    url
}

fn build_managed_browser_launch_config(
    port: u16,
    paths: &ManagedBrowserPaths,
    stealth_enabled: bool,
) -> Result<BrowserConfig, BrowserError> {
    let mut builder = BrowserConfig::builder()
        .new_headless_mode()
        .port(port)
        .arg("--remote-allow-origins=*")
        .arg("--remote-debugging-address=0.0.0.0")
        .user_data_dir(paths.profile_dir.clone())
        .chrome_executable(paths.executable.clone());
    if stealth_enabled {
        // The single most-detected automation signal is the
        // AutomationControlled blink feature — fingerprinting libraries
        // read it before they reach for navigator.webdriver. Combined
        // with the Page.addScriptToEvaluateOnNewDocument shim below,
        // stock bot.sannysoft.com / creepjs reads pass.
        builder = builder
            .arg("--disable-blink-features=AutomationControlled")
            .arg("--no-default-browser-check")
            .arg("--no-first-run");
    }
    builder.build().map_err(|error| {
        BrowserError::browser_operation("build browser launch config", std::io::Error::other(error))
    })
}

async fn wait_for_local_browser_websocket_endpoint(
    port: u16,
    timeout: Duration,
) -> Result<String, BrowserError> {
    let metadata_url = managed_browser_debugger_metadata_url(port);
    let client = reqwest::Client::new();
    let start = Instant::now();

    loop {
        match client.get(metadata_url.clone()).send().await {
            Ok(response) if response.status().is_success() => {
                let metadata: BrowserVersionMetadata = response.json().await.map_err(|source| {
                    ManagedBrowserStartupError::DecodeDebuggerMetadata {
                        url: metadata_url.clone(),
                        source,
                    }
                })?;
                return metadata.web_socket_debugger_url.ok_or_else(|| {
                    ManagedBrowserStartupError::MissingWebSocketDebuggerUrl {
                        url: metadata_url.clone(),
                    }
                    .into()
                });
            }
            Ok(_) => {}
            Err(error) if error.is_connect() || error.is_timeout() => {}
            Err(error) => {
                return Err(BrowserError::browser_operation(
                    "fetch managed browser debugger metadata",
                    error,
                ));
            }
        }

        if start.elapsed() >= timeout {
            return Err(ManagedBrowserStartupError::Timeout { port, timeout }.into());
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn shutdown_managed_browser_session(
    browser: &mut Browser,
    owns_process: bool,
) -> Result<(), BrowserError> {
    let _ = browser.close().await;

    if !owns_process {
        return Ok(());
    }

    if browser
        .try_wait()
        .map_err(|error| {
            BrowserError::browser_operation("inspect managed browser shutdown", error)
        })?
        .is_some()
    {
        return Ok(());
    }

    match tokio::time::timeout(Duration::from_secs(3), browser.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(BrowserError::browser_operation(
            "wait for managed browser shutdown",
            error,
        )),
        Err(_) => match browser.kill().await {
            Some(Ok(())) | None => Ok(()),
            Some(Err(error)) => Err(BrowserError::browser_operation(
                "kill managed browser after shutdown timeout",
                error,
            )),
        },
    }
}

async fn acquire_browser_page(browser: &mut Browser) -> Result<Page, BrowserError> {
    let timeout = browser_initial_page_timeout();
    let start = Instant::now();
    let mut page_target_id: Option<TargetId> = None;

    loop {
        let target_id = if let Some(target_id) = page_target_id.clone() {
            target_id
        } else {
            let targets = browser
                .fetch_targets()
                .await
                .map_err(|error| BrowserError::browser_operation("fetch browser targets", error))?;
            let discovered_target_id = first_page_target_id(targets.as_slice()).ok_or_else(|| {
                BrowserError::browser_operation(
                    "discover initial browser page target",
                    std::io::Error::other("no page target exposed by browser"),
                )
            });
            match discovered_target_id {
                Ok(target_id) => {
                    tracing::debug!(target_id = ?target_id, "discovered initial browser page target");
                    page_target_id = Some(target_id.clone());
                    target_id
                }
                Err(_) if start.elapsed() < timeout => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                Err(error) => return Err(error),
            }
        };

        match browser.get_page(target_id.clone()).await {
            Ok(page) => return Ok(page),
            Err(CdpError::NotFound) => {
                tracing::debug!(target_id = ?target_id, "browser page target not ready yet");
            }
            Err(error) => {
                return Err(BrowserError::browser_operation(
                    "acquire browser page from target",
                    error,
                ));
            }
        }

        if start.elapsed() >= timeout {
            return Err(BrowserPageReadyError::Timeout { timeout }.into());
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn first_page_target_id(targets: &[TargetInfo]) -> Option<TargetId> {
    targets
        .iter()
        .find(|target| target.r#type == "page")
        .map(|target| target.target_id.clone())
}

async fn read_page_capture(
    page: Arc<Page>,
    url: &str,
    options: BrowserReadOptions,
) -> Result<BrowserPageCapture, BrowserError> {
    let trimmed_url = url.trim();
    if trimmed_url.is_empty() {
        return Err(BrowserError::InvalidInput(
            BrowserInputError::EmptyNavigationUrl,
        ));
    }

    let limits = capture_limits(options.detail);

    page.goto(trimmed_url)
        .await
        .map_err(|error| BrowserError::browser_operation("navigate page for read", error))?;
    tokio::time::sleep(limits.settle_delay).await;

    let page_summary = read_page_summary(page.as_ref(), limits).await?;
    let accessibility = read_accessibility_snapshot(page.as_ref(), limits).await?;
    let screenshot = capture_page_screenshot(page.as_ref(), options.screenshot).await?;

    Ok(BrowserPageCapture {
        page: BrowserPageSummary {
            url: page_summary.url,
            title: page_summary.title,
            main_heading: page_summary.main_heading,
            headings: page_summary.headings,
            text_excerpt: page_summary.text_excerpt,
            interactive_elements: page_summary.interactive_elements,
            accessibility,
        },
        screenshot,
    })
}

const fn capture_limits(detail: BrowserReadDetail) -> BrowserCaptureLimits {
    match detail {
        BrowserReadDetail::Brief => BrowserCaptureLimits {
            text_excerpt_chars: 1_200,
            interactive_elements: 8,
            accessibility_nodes: 16,
            accessibility_depth: 4,
            settle_delay: Duration::from_millis(500),
        },
        BrowserReadDetail::Standard => BrowserCaptureLimits {
            text_excerpt_chars: 3_000,
            interactive_elements: 16,
            accessibility_nodes: 32,
            accessibility_depth: 6,
            settle_delay: Duration::from_millis(900),
        },
        BrowserReadDetail::Full => BrowserCaptureLimits {
            text_excerpt_chars: 6_000,
            interactive_elements: 32,
            accessibility_nodes: 64,
            accessibility_depth: 8,
            settle_delay: Duration::from_millis(1_500),
        },
    }
}

async fn read_page_summary(
    page: &Page,
    limits: BrowserCaptureLimits,
) -> Result<BrowserDomSummary, BrowserError> {
    let summary_script = format!(
        "() => {{ const normalize = (value) => typeof value === 'string' ? value.replace(/\\s+/g, ' ').trim() : ''; const excerptChars = {}; const maxInteractiveElements = {}; const headings = Array.from(document.querySelectorAll('h1, h2, h3')).map((node) => normalize(node.innerText)).filter(Boolean).slice(0, 12); const interactiveElements = Array.from(document.querySelectorAll('a, button, input, select, textarea, summary, [role=\"button\"], [role=\"link\"], [role=\"menuitem\"], [tabindex]')).map((node) => {{ const role = normalize(node.getAttribute('role')) || normalize(node.tagName).toLowerCase(); const text = normalize(node.innerText || node.textContent || ''); const accessibleName = normalize(node.getAttribute('aria-label')) || normalize(node.getAttribute('title')) || text || null; const href = node instanceof HTMLAnchorElement ? (node.href || null) : null; const inputType = node instanceof HTMLInputElement ? (node.type || null) : null; return {{ role, tagName: normalize(node.tagName).toLowerCase(), text: text || null, accessibleName, href, inputType, disabled: Boolean(node.disabled) }}; }}).filter((item) => item.accessibleName || item.href || item.tagName === 'input').slice(0, maxInteractiveElements); const bodyText = normalize(document.body ? document.body.innerText : '').slice(0, excerptChars); return {{ url: location.href, title: normalize(document.title), mainHeading: headings[0] ?? null, headings, textExcerpt: bodyText, interactiveElements }}; }}",
        limits.text_excerpt_chars, limits.interactive_elements
    );

    page.evaluate(summary_script.as_str())
        .await
        .map_err(|error| BrowserError::browser_operation("evaluate page summary script", error))?
        .into_value()
        .map_err(|error| BrowserError::browser_operation("decode page summary", error))
}

async fn read_accessibility_snapshot(
    page: &Page,
    limits: BrowserCaptureLimits,
) -> Result<BrowserAccessibilitySnapshot, BrowserError> {
    page.execute(AccessibilityEnableParams::default())
        .await
        .map_err(|error| BrowserError::browser_operation("enable accessibility domain", error))?;
    let response = page
        .execute(
            GetFullAccessibilityTreeParams::builder()
                .depth(limits.accessibility_depth)
                .build(),
        )
        .await
        .map_err(|error| BrowserError::browser_operation("read accessibility tree", error))?;

    Ok(summarize_accessibility_tree(
        &response.result.nodes,
        limits.accessibility_nodes,
    ))
}

fn summarize_accessibility_tree(
    nodes: &[AxNode],
    max_nodes: usize,
) -> BrowserAccessibilitySnapshot {
    let total_nodes = nodes.len();
    let interactive_nodes = nodes
        .iter()
        .filter(|node| {
            accessibility_role(node)
                .as_deref()
                .is_some_and(is_interactive_role)
        })
        .count();
    let heading_nodes = nodes
        .iter()
        .filter(|node| {
            accessibility_role(node)
                .as_deref()
                .is_some_and(is_heading_role)
        })
        .count();
    let landmark_nodes = nodes
        .iter()
        .filter(|node| {
            accessibility_role(node)
                .as_deref()
                .is_some_and(is_landmark_role)
        })
        .count();

    let node_by_id = nodes
        .iter()
        .map(|node| (node.node_id.as_ref().to_string(), node))
        .collect::<HashMap<_, _>>();
    let mut roots = nodes
        .iter()
        .filter(|node| node.parent_id.is_none())
        .map(|node| node.node_id.as_ref().to_string())
        .collect::<Vec<_>>();
    roots.sort_unstable();

    let mut rendered_nodes = Vec::new();
    for root in roots {
        if rendered_nodes.len() >= max_nodes {
            break;
        }
        append_accessibility_nodes(
            &node_by_id,
            root.as_str(),
            0,
            max_nodes,
            &mut rendered_nodes,
        );
    }

    let interesting_nodes = rendered_nodes.len();
    BrowserAccessibilitySnapshot {
        total_nodes,
        interesting_nodes,
        interactive_nodes,
        heading_nodes,
        landmark_nodes,
        nodes: rendered_nodes,
    }
}

fn append_accessibility_nodes(
    node_by_id: &HashMap<String, &AxNode>,
    node_id: &str,
    depth: usize,
    max_nodes: usize,
    rendered_nodes: &mut Vec<BrowserAccessibilityNode>,
) {
    if rendered_nodes.len() >= max_nodes {
        return;
    }

    let Some(node) = node_by_id.get(node_id).copied() else {
        return;
    };

    if is_interesting_accessibility_node(node) {
        rendered_nodes.push(BrowserAccessibilityNode {
            role: accessibility_role(node).unwrap_or_else(|| "unknown".to_string()),
            name: accessibility_value(node.name.as_ref()),
            description: accessibility_value(node.description.as_ref()),
            value: accessibility_value(node.value.as_ref()),
            depth,
            child_count: node.child_ids.as_ref().map_or(0, Vec::len),
            ignored: node.ignored,
            properties: accessibility_properties(node.properties.as_deref()),
        });
    }

    if rendered_nodes.len() >= max_nodes {
        return;
    }

    if let Some(child_ids) = node.child_ids.as_ref() {
        for child_id in child_ids {
            let child_key = child_id.as_ref().to_string();
            append_accessibility_nodes(
                node_by_id,
                child_key.as_str(),
                depth + 1,
                max_nodes,
                rendered_nodes,
            );
            if rendered_nodes.len() >= max_nodes {
                return;
            }
        }
    }
}

fn is_interesting_accessibility_node(node: &AxNode) -> bool {
    if node.ignored {
        return false;
    }

    let role = accessibility_role(node);
    role.as_ref().is_some_and(|value| {
        is_interactive_role(value) || is_heading_role(value) || is_landmark_role(value)
    }) || accessibility_value(node.name.as_ref()).is_some()
}

fn accessibility_role(node: &AxNode) -> Option<String> {
    accessibility_value(node.role.as_ref()).map(|value| value.to_lowercase())
}

fn accessibility_value(value: Option<&AxValue>) -> Option<String> {
    value
        .and_then(|value| value.value.as_ref())
        .and_then(accessibility_json_value)
}

fn accessibility_json_value(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(value) => Some(value.to_string()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::String(value) => {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        serde_json::Value::Array(values) => {
            let joined = values
                .iter()
                .filter_map(accessibility_json_value)
                .collect::<Vec<_>>()
                .join(", ");
            (!joined.is_empty()).then_some(joined)
        }
        serde_json::Value::Object(_) => Some(value.to_string()),
    }
}

fn accessibility_properties(
    properties: Option<&[AxProperty]>,
) -> Vec<BrowserAccessibilityProperty> {
    properties
        .unwrap_or_default()
        .iter()
        .filter_map(|property| {
            accessibility_value(Some(&property.value)).map(|value| BrowserAccessibilityProperty {
                name: property.name.as_ref().to_string(),
                value,
            })
        })
        .take(8)
        .collect()
}

fn is_interactive_role(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "checkbox"
            | "combobox"
            | "link"
            | "listbox"
            | "menuitem"
            | "option"
            | "radio"
            | "searchbox"
            | "slider"
            | "spinbutton"
            | "switch"
            | "tab"
            | "textbox"
    )
}

fn is_heading_role(role: &str) -> bool {
    role == "heading"
}

fn is_landmark_role(role: &str) -> bool {
    matches!(
        role,
        "banner"
            | "complementary"
            | "contentinfo"
            | "form"
            | "main"
            | "navigation"
            | "region"
            | "search"
    )
}

async fn capture_page_screenshot(
    page: &Page,
    mode: BrowserScreenshotMode,
) -> Result<Option<BrowserCapturedScreenshot>, BrowserError> {
    if matches!(mode, BrowserScreenshotMode::Off) {
        return Ok(None);
    }

    let screenshot_bytes = page
        .screenshot(
            ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Png)
                .full_page(matches!(mode, BrowserScreenshotMode::FullPage))
                .build(),
        )
        .await
        .map_err(|error| BrowserError::browser_operation("capture browser screenshot", error))?;
    let dimensions = image::load_from_memory(screenshot_bytes.as_slice())
        .map_err(|error| BrowserError::browser_operation("decode browser screenshot", error))?;

    Ok(Some(BrowserCapturedScreenshot {
        mime_type: "image/png".to_string(),
        width: dimensions.width(),
        height: dimensions.height(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(screenshot_bytes),
    }))
}

async fn start_encoding_pipeline(
    page: Arc<Page>,
    video_track: Arc<TrackLocalStaticSample>,
    initial_bitrate_kbps: u32,
    keyframe_request: Arc<AtomicBool>,
) -> Result<(tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>), BrowserError> {
    let mut event_stream = page
        .event_listener::<EventScreencastFrame>()
        .await
        .map_err(|error| {
            BrowserError::browser_operation("subscribe to screencast frames", error)
        })?;

    // Bounded queue. On full we drop the *new* frame rather than block:
    // for screen content there are no decode dependencies between frames
    // so a fresher frame loses no information when the next one arrives,
    // and not blocking keeps the CDP reader from stalling the actor.
    let (jpeg_tx, jpeg_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(JPEG_QUEUE_CAPACITY);
    let (sample_tx, mut sample_rx) = tokio::sync::mpsc::channel::<EncodedSample>(16);

    let page_ack = page.clone();
    let cdp_reader = tokio::spawn(async move {
        while let Some(event) = event_stream.next().await {
            if let Err(e) = page_ack
                .execute(ScreencastFrameAckParams::new(event.session_id))
                .await
            {
                tracing::warn!(error = %e, "Failed to ack screencast frame");
                continue;
            }

            let b64_str: &str = event.data.as_ref();
            let jpeg_bytes = match base64::engine::general_purpose::STANDARD.decode(b64_str) {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!(error = %e, "Invalid base64 in screencast frame");
                    continue;
                }
            };

            match jpeg_tx.try_send(jpeg_bytes) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    tracing::debug!("encoder queue saturated, dropping screencast frame");
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
            }
        }
        tracing::info!("CDP screencast reader stopped");
    });

    // Encoder is CPU-bound so it goes on the blocking pool, not the
    // tokio worker threads. spawn_blocking returns a JoinHandle the same
    // shape as tokio::spawn, so the rest of the pipeline lifecycle
    // (abort on session stop) is uniform.
    tokio::task::spawn_blocking(move || {
        encode_loop(jpeg_rx, &sample_tx, initial_bitrate_kbps, &keyframe_request);
    });

    let sample_writer = tokio::spawn(async move {
        while let Some(encoded) = sample_rx.recv().await {
            if let Err(e) = video_track
                .write_sample(&Sample {
                    data: bytes::Bytes::from(encoded.data),
                    duration: encoded.duration,
                    ..Default::default()
                })
                .await
            {
                tracing::trace!(error = %e, "WebRTC sample write skipped");
            }
        }
        tracing::info!("WebRTC sample writer stopped");
    });

    Ok((cdp_reader, sample_writer))
}

/// Capacity of the JPEG → encoder queue. Tuned so a transient encoder
/// stall (~250 ms at 30 fps) absorbs without dropping frames, while still
/// catching unbounded growth on a stuck consumer.
const JPEG_QUEUE_CAPACITY: usize = 8;

// ── H.264 encoding loop (dedicated blocking task) ──────────────────────────────

fn encode_loop(
    mut frame_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    sample_tx: &tokio::sync::mpsc::Sender<EncodedSample>,
    initial_bitrate_kbps: u32,
    keyframe_request: &AtomicBool,
) {
    use openh264::OpenH264API;
    use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, UsageType};
    use openh264::formats::{RgbSliceU8, YUVBuffer};

    // Screen Content Real-Time mode: optimized for text/UI edges, solid backgrounds
    let config = EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .bitrate(BitRate::from_bps(initial_bitrate_kbps.saturating_mul(1000)))
        .max_frame_rate(FrameRate::from_hz(30.0));

    let api = OpenH264API::from_source();
    let mut encoder = match Encoder::with_api_config(api, config) {
        Ok(enc) => enc,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create H.264 encoder");
            return;
        }
    };

    let mut last_frame_time: Option<Instant> = None;
    let mut last_keyframe = Instant::now();
    let keyframe_interval = Duration::from_secs(2);

    // `blocking_recv` is the documented bridge between tokio mpsc and
    // a `spawn_blocking` task. It blocks the current OS thread (which
    // is the blocking-pool thread we own here) until a frame arrives.
    while let Some(jpeg_bytes) = frame_rx.blocking_recv() {
        // Decode JPEG → RGB
        let img = match image::load_from_memory_with_format(&jpeg_bytes, image::ImageFormat::Jpeg) {
            Ok(img) => img,
            Err(e) => {
                tracing::warn!(error = %e, "JPEG decode failed");
                continue;
            }
        };

        let rgb = normalize_rgb_frame_for_h264(img.to_rgb8());
        let (w, h) = (rgb.width() as usize, rgb.height() as usize);

        // Compute inter-frame duration
        let now = Instant::now();
        let duration = last_frame_time.map_or(Duration::from_millis(33), |t| now.duration_since(t));
        last_frame_time = Some(now);

        // Force IDR on viewer-join (drains the request flag) or on the
        // periodic interval. Without this a fresh peer can wait up to
        // `keyframe_interval` for the next decodable frame.
        let join_idr = keyframe_request.swap(false, Ordering::AcqRel);
        if join_idr || now.duration_since(last_keyframe) > keyframe_interval {
            encoder.force_intra_frame();
            last_keyframe = now;
        }

        // RGB → YUV420 → H.264
        let rgb_source = RgbSliceU8::new(rgb.as_raw(), (w, h));
        let yuv = YUVBuffer::from_rgb_source(rgb_source);
        let bitstream = match encoder.encode(&yuv) {
            Ok(bs) => bs,
            Err(e) => {
                tracing::warn!(error = %e, "H.264 encode failed");
                continue;
            }
        };

        let data = bitstream.to_vec();
        if data.is_empty() {
            continue;
        }

        if sample_tx
            .blocking_send(EncodedSample { data, duration })
            .is_err()
        {
            break; // WebRTC writer stopped
        }
    }

    tracing::info!("Browser encoder thread stopped");
}

fn normalize_rgb_frame_for_h264(rgb: image::RgbImage) -> image::RgbImage {
    let width = rgb.width();
    let height = rgb.height();
    let normalized_width = if width.is_multiple_of(2) {
        width
    } else {
        width + 1
    };
    let normalized_height = if height.is_multiple_of(2) {
        height
    } else {
        height + 1
    };

    if normalized_width == width && normalized_height == height {
        return rgb;
    }

    let mut normalized = image::RgbImage::new(normalized_width, normalized_height);

    for y in 0..height {
        for x in 0..width {
            normalized.put_pixel(x, y, *rgb.get_pixel(x, y));
        }
    }

    if normalized_width != width {
        let edge_x = width - 1;
        for y in 0..height {
            normalized.put_pixel(normalized_width - 1, y, *rgb.get_pixel(edge_x, y));
        }
    }

    if normalized_height != height {
        let edge_y = height - 1;
        for x in 0..normalized_width {
            let source_x = x.min(width - 1);
            normalized.put_pixel(
                x,
                normalized_height - 1,
                *normalized.get_pixel(source_x, edge_y),
            );
        }
    }

    normalized
}

// ── DataChannel input handler ──────────────────────────────────────────────────

fn setup_peer_connection_lifecycle(
    pc: &Arc<RTCPeerConnection>,
    peer_id: String,
    peer_events: mpsc::UnboundedSender<BrowserCommand>,
) {
    pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
        let peer_id = peer_id.clone();
        let peer_events = peer_events.clone();
        Box::pin(async move {
            if matches!(
                state,
                RTCPeerConnectionState::Disconnected
                    | RTCPeerConnectionState::Failed
                    | RTCPeerConnectionState::Closed
            ) {
                let _ = peer_events.send(BrowserCommand::DropPeer { peer_id });
            }
        })
    }));
}

fn setup_data_channel_handler(
    pc: &RTCPeerConnection,
    page: Arc<Page>,
    peer_session_id: String,
    takeover_owner: Arc<ArcSwapOption<String>>,
) {
    pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        let page = page.clone();
        let peer_session_id = peer_session_id.clone();
        let takeover_owner = takeover_owner.clone();
        Box::pin(async move {
            let label = dc.label().to_owned();
            tracing::debug!(label, "DataChannel opened for browser input");

            dc.on_message(Box::new(move |msg: DataChannelMessage| {
                let page = page.clone();
                let peer_session_id = peer_session_id.clone();
                let takeover_owner = takeover_owner.clone();
                Box::pin(async move {
                    let Ok(text) = String::from_utf8(msg.data.to_vec()) else {
                        return;
                    };
                    if let Err(e) =
                        handle_input_message(&page, &text, &peer_session_id, &takeover_owner).await
                    {
                        tracing::warn!(error = %e, "Browser input handling failed");
                    }
                })
            }));
        })
    }));
}

/// Returns true if the `peer_session_id` currently owns input over the
/// browser. Pure function over the snapshot — exercised by the takeover
/// gate unit test.
fn takeover_allows_input(peer_session_id: &str, takeover_owner: &ArcSwapOption<String>) -> bool {
    let owner = takeover_owner.load();
    owner
        .as_ref()
        .is_some_and(|owner_id| owner_id.as_str() == peer_session_id)
}

async fn handle_input_message(
    page: &Page,
    text: &str,
    peer_session_id: &str,
    takeover_owner: &ArcSwapOption<String>,
) -> Result<(), BrowserError> {
    let msg: InputMessage = serde_json::from_str(text)
        .map_err(|source| BrowserError::InvalidInputPayload { source })?;

    if !takeover_allows_input(peer_session_id, takeover_owner) {
        // Silently drop input when this peer has not been granted
        // takeover. The agent owns the page in that state and arbitrary
        // user clicks would race with agent-driven CDP commands.
        tracing::debug!(
            peer_session_id,
            "browser input dropped — peer is not the takeover owner"
        );
        return Ok(());
    }

    match msg {
        InputMessage::Mouse(input) => dispatch_mouse(page, input).await,
        InputMessage::Keyboard(input) => dispatch_keyboard(page, input).await,
        InputMessage::Navigate { url } => {
            let trimmed_url = url.trim();
            if trimmed_url.is_empty() {
                return Err(BrowserInputError::EmptyNavigationUrl.into());
            }
            page.goto(trimmed_url).await.map_err(|error| {
                BrowserError::browser_operation("navigate from data channel", error)
            })?;
            Ok(())
        }
    }
}

async fn dispatch_mouse(page: &Page, input: MouseInput) -> Result<(), BrowserError> {
    let event_type = input.event_type;
    let button = match input.button {
        Some(button) => MouseButton::from(button),
        None if event_type.requires_button() => {
            return Err(BrowserInputError::MissingMouseButton { event_type }.into());
        }
        None => MouseButton::None,
    };

    let mut builder = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::from(event_type))
        .x(input.x)
        .y(input.y)
        .button(button);

    if let Some(count) = input.click_count {
        builder = builder.click_count(i64::from(count));
    }
    if let Some(dx) = input.delta_x {
        builder = builder.delta_x(dx);
    }
    if let Some(dy) = input.delta_y {
        builder = builder.delta_y(dy);
    }
    if let Some(m) = input.modifiers {
        builder = builder.modifiers(i64::from(m));
    }

    let params = builder.build().map_err(|error| {
        BrowserError::browser_operation("build mouse dispatch params", std::io::Error::other(error))
    })?;
    page.execute(params)
        .await
        .map_err(|error| BrowserError::browser_operation("dispatch mouse event", error))?;
    Ok(())
}

async fn dispatch_keyboard(page: &Page, input: KeyboardInput) -> Result<(), BrowserError> {
    let mut builder = DispatchKeyEventParams::builder().r#type(input.event_type);

    if let Some(ref key) = input.key {
        builder = builder.key(key);
    }
    if let Some(ref code) = input.code {
        builder = builder.code(code);
    }
    if let Some(ref text) = input.text {
        builder = builder.text(text);
    }
    if let Some(m) = input.modifiers {
        builder = builder.modifiers(i64::from(m));
    }

    let params = builder.build().map_err(|error| {
        BrowserError::browser_operation(
            "build keyboard dispatch params",
            std::io::Error::other(error),
        )
    })?;
    page.execute(params)
        .await
        .map_err(|error| BrowserError::browser_operation("dispatch keyboard event", error))?;
    Ok(())
}

// ── Browser binary detection ───────────────────────────────────────────────────

/// Kill stale Chrome processes from previous runs and clean up lock files.
///
/// When the server exits without cleanly shutting down `BrowserManager`,
/// Chrome processes survive and hold the CDP port. This function detects
/// and kills any chromiumoxide-launched Chrome on the target port.
async fn cleanup_stale_chrome(port: u16, paths: Option<&ManagedBrowserPaths>) {
    let Some(paths) = paths.cloned() else {
        return;
    };
    let _ = tokio::task::spawn_blocking(move || {
        let Ok(output) = std::process::Command::new("lsof")
            .args(["-ti", &format!(":{port}")])
            .output()
        else {
            return;
        };
        let pids_str = String::from_utf8_lossy(&output.stdout);
        for pid_str in pids_str.split_whitespace() {
            let Ok(pid) = pid_str.parse::<u32>() else {
                continue;
            };
            let Ok(cmdline) = std::process::Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "args="])
                .output()
            else {
                continue;
            };
            let args = String::from_utf8_lossy(&cmdline.stdout);
            if paths.owns_process_command(args.as_ref()) {
                tracing::info!(pid, "Killing stale Chrome process on port {port}");
                let _ = std::process::Command::new("kill")
                    .arg(pid.to_string())
                    .status();
            }
        }
    })
    .await;
}

async fn cleanup_profile_lock_files(profile_dir: &Path) {
    let profile_dir = profile_dir.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
            let path = profile_dir.join(name);
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }
        }
    })
    .await;
}

/// Resolve the provisioned browser binary path. Runs in `spawn_blocking` since it does filesystem I/O.
fn managed_browser_executable(may_dir: &Path) -> Option<PathBuf> {
    let cache_dir = crate::session::MayHomePaths::new(may_dir.to_path_buf()).browser_cache_dir();
    if !cache_dir.exists() {
        return None;
    }

    let binary_name = if cfg!(target_os = "macos") {
        "Google Chrome for Testing"
    } else if cfg!(target_os = "windows") {
        "chrome.exe"
    } else {
        "chrome"
    };

    WalkDir::new(cache_dir)
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| entry.file_type().is_file() && entry.file_name() == binary_name)
        .map(walkdir::DirEntry::into_path)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwapOption;
    use image::{Rgb, RgbImage};

    use super::{normalize_rgb_frame_for_h264, takeover_allows_input};

    #[test]
    fn normalize_rgb_frame_for_h264_preserves_even_dimensions() {
        let mut rgb = RgbImage::new(4, 6);
        rgb.put_pixel(3, 5, Rgb([12, 34, 56]));

        let normalized = normalize_rgb_frame_for_h264(rgb.clone());

        assert_eq!(normalized.width(), 4);
        assert_eq!(normalized.height(), 6);
        assert_eq!(normalized, rgb);
    }

    #[test]
    fn normalize_rgb_frame_for_h264_pads_odd_dimensions_with_edge_pixels() {
        let mut rgb = RgbImage::new(3, 5);
        for y in 0..5 {
            for x in 0..3 {
                rgb.put_pixel(
                    x,
                    y,
                    Rgb([
                        u8::try_from(x * 10).unwrap(),
                        u8::try_from(y * 20).unwrap(),
                        99,
                    ]),
                );
            }
        }

        let normalized = normalize_rgb_frame_for_h264(rgb);

        assert_eq!(normalized.width(), 4);
        assert_eq!(normalized.height(), 6);
        assert_eq!(*normalized.get_pixel(0, 0), Rgb([0, 0, 99]));
        assert_eq!(*normalized.get_pixel(2, 4), Rgb([20, 80, 99]));
        assert_eq!(*normalized.get_pixel(3, 1), Rgb([20, 20, 99]));
        assert_eq!(*normalized.get_pixel(1, 5), Rgb([10, 80, 99]));
        assert_eq!(*normalized.get_pixel(3, 5), Rgb([20, 80, 99]));
    }

    #[test]
    fn takeover_gate_blocks_input_when_no_owner() {
        let owner: ArcSwapOption<String> = ArcSwapOption::empty();
        assert!(!takeover_allows_input("session-a", &owner));
    }

    #[test]
    fn takeover_gate_blocks_input_for_non_matching_session() {
        let owner = ArcSwapOption::new(Some(Arc::new("session-a".to_owned())));
        assert!(!takeover_allows_input("session-b", &owner));
    }

    #[test]
    fn takeover_gate_allows_input_for_owning_session() {
        let owner = ArcSwapOption::new(Some(Arc::new("session-a".to_owned())));
        assert!(takeover_allows_input("session-a", &owner));
    }

    #[test]
    fn takeover_gate_releases_after_clear() {
        let owner = ArcSwapOption::new(Some(Arc::new("session-a".to_owned())));
        assert!(takeover_allows_input("session-a", &owner));
        owner.store(None);
        assert!(!takeover_allows_input("session-a", &owner));
    }
}
