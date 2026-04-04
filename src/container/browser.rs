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
use std::time::{Duration, Instant};

use base64::Engine;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    MouseButton,
};
use chromiumoxide::cdp::browser_protocol::page::{
    EventScreencastFrame, ScreencastFrameAckParams, StartScreencastFormat, StartScreencastParams,
};
use chromiumoxide::page::Page;
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
use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine};
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

use crate::browser_service::BrowserPageSnapshot;
use crate::config::{BrowserBackendKind, BrowserConfig as MayBrowserConfig};

// ── Error ──────────────────────────────────────────────────────────────────────

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum BrowserInputError {
    #[error("navigate input requires a non-empty url")]
    EmptyNavigationUrl,
}

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("Browser not running")]
    NotRunning,
    #[error("Managed browser runtime is not provisioned")]
    NotProvisioned,
    #[error("browser actor unexpectedly closed while handling {operation}")]
    ActorClosed { operation: &'static str },
    #[error("browser operation failed during {operation}")]
    BrowserOperation {
        operation: &'static str,
        #[source]
        source: BoxError,
    },
    #[error("webrtc operation failed during {operation}")]
    WebRtcOperation {
        operation: &'static str,
        #[source]
        source: BoxError,
    },
    #[error("encoder operation failed during {operation}")]
    EncoderOperation {
        operation: &'static str,
        #[source]
        source: BoxError,
    },
    #[error("provisioning operation failed during {operation}")]
    ProvisioningOperation {
        operation: &'static str,
        #[source]
        source: BoxError,
    },
    #[error("browser input payload is invalid")]
    InvalidInputPayload {
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    InvalidInput(#[from] BrowserInputError),
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

    fn encoder_operation(
        operation: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::EncoderOperation {
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
enum MouseEventType {
    MousePressed,
    MouseReleased,
    MouseMoved,
    MouseWheel,
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
    owns_browser_process: bool,
    _handler: tokio::task::JoinHandle<()>,
    _cdp_reader: tokio::task::JoinHandle<()>,
    _sample_writer: tokio::task::JoinHandle<()>,
    _audio_writer: tokio::task::JoinHandle<()>,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserVersionMetadata {
    web_socket_debugger_url: Option<String>,
}

enum BrowserCommand {
    Start {
        reply: oneshot::Sender<Result<(), BrowserError>>,
    },
    Stop {
        reply: oneshot::Sender<Result<(), BrowserError>>,
    },
    HandleOffer {
        offer_sdp: String,
        reply: oneshot::Sender<Result<String, BrowserError>>,
    },
    ReadPage {
        url: String,
        reply: oneshot::Sender<Result<BrowserPageSnapshot, BrowserError>>,
    },
    DropPeer {
        peer_id: String,
    },
}

struct BrowserRuntime {
    session: Option<BrowserSession>,
    video_track: Arc<TrackLocalStaticSample>,
    audio_track: Arc<TrackLocalStaticSample>,
    webrtc_api: webrtc::api::API,
    peers: HashMap<String, Arc<RTCPeerConnection>>,
}

impl BrowserRuntime {
    fn new(
        video_track: Arc<TrackLocalStaticSample>,
        audio_track: Arc<TrackLocalStaticSample>,
        webrtc_api: webrtc::api::API,
    ) -> Self {
        Self {
            session: None,
            video_track,
            audio_track,
            webrtc_api,
            peers: HashMap::new(),
        }
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
        self.peers.remove(peer_id);
    }
}

// ── BrowserManager ─────────────────────────────────────────────────────────────

/// Manages a browser instance with WebRTC live streaming and input forwarding.
///
/// Video pipeline: CDP screencast → JPEG → H.264 (Screen Content) → WebRTC
/// Input pipeline: WebRTC `DataChannel` → JSON → CDP Input.dispatch*
pub struct BrowserManager {
    command_tx: mpsc::UnboundedSender<BrowserCommand>,
    snapshot_rx: watch::Receiver<BrowserRuntimeSnapshot>,
}

impl BrowserManager {
    /// Create a new browser manager. Sets up the WebRTC API and shared video track.
    pub fn new(config: MayBrowserConfig, may_dir: PathBuf) -> Result<Self, BrowserError> {
        config
            .validate()
            .map_err(|error| BrowserError::browser_operation("validate browser config", error))?;
        // Register default codecs: H.264, VP8, VP9, AV1 — SDP negotiation picks the best
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .map_err(|error| BrowserError::web_rtc_operation("register default codecs", error))?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)
            .map_err(|error| {
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
        let audio_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                ..Default::default()
            },
            "audio".to_owned(),
            "browser-audio".to_owned(),
        ));

        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let actor_tx = command_tx.clone();
        let (snapshot_tx, snapshot_rx) = watch::channel(BrowserRuntimeSnapshot::default());
        let config_for_actor = config.clone();
        let may_dir_for_actor = may_dir.clone();

        tokio::spawn(async move {
            let mut runtime = BrowserRuntime::new(video_track, audio_track, api);
            runtime.publish_snapshot(&snapshot_tx);

            while let Some(command) = command_rx.recv().await {
                match command {
                    BrowserCommand::Start { reply } => {
                        let result = runtime
                            .start(&config_for_actor, may_dir_for_actor.as_path())
                            .await;
                        runtime.publish_snapshot(&snapshot_tx);
                        let _ = reply.send(result);
                    }
                    BrowserCommand::Stop { reply } => {
                        let result = runtime.stop().await;
                        runtime.publish_snapshot(&snapshot_tx);
                        let _ = reply.send(result);
                    }
                    BrowserCommand::HandleOffer { offer_sdp, reply } => {
                        let _ = reply.send(runtime.handle_offer(offer_sdp, actor_tx.clone()).await);
                    }
                    BrowserCommand::ReadPage { url, reply } => {
                        let _ = reply.send(runtime.read_page(url.as_str()).await);
                    }
                    BrowserCommand::DropPeer { peer_id } => {
                        runtime.drop_peer(peer_id.as_str());
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
    pub async fn is_running(&self) -> bool {
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
    pub async fn start(&self) -> Result<(), BrowserError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(BrowserCommand::Start { reply: reply_tx })
            .map_err(|_| BrowserError::ActorClosed { operation: "start browser" })?;
        reply_rx
            .await
            .map_err(|_| BrowserError::ActorClosed {
                operation: "await start reply",
            })?
    }

    /// Stop the browser and clean up all resources.
    pub async fn stop(&self) -> Result<(), BrowserError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(BrowserCommand::Stop { reply: reply_tx })
            .map_err(|_| BrowserError::ActorClosed { operation: "stop browser" })?;
        reply_rx
            .await
            .map_err(|_| BrowserError::ActorClosed {
                operation: "await stop reply",
            })?
    }

    /// Handle a WebRTC SDP offer from a client. Returns the SDP answer.
    ///
    /// Uses WHEP-style single HTTP round-trip signaling:
    /// 1. Client creates offer (recvonly video + `DataChannel`)
    /// 2. Server creates answer with video track + ICE candidates
    /// 3. Connection established, video flows, `DataChannel` ready for input
    pub async fn handle_offer(&self, offer_sdp: String) -> Result<String, BrowserError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(BrowserCommand::HandleOffer {
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

    pub async fn read_page(&self, url: String) -> Result<BrowserPageSnapshot, BrowserError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(BrowserCommand::ReadPage {
                url,
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

pub async fn prepare_browser_runtime(
    config: &MayBrowserConfig,
    may_dir: &Path,
) -> Result<(), BrowserError> {
    config
        .validate()
        .map_err(|error| BrowserError::browser_operation("validate browser runtime config", error))?;
    if config.requires_managed_runtime() {
        let _ = provision_managed_browser(may_dir).await?;
    }
    Ok(())
}

#[must_use]
pub fn provisioned_browser_executable(may_dir: &Path) -> Option<PathBuf> {
    managed_browser_executable(may_dir).map(PathBuf::from)
}

pub async fn stop_managed_browser(port: u16, may_dir: &Path) {
    cleanup_stale_chrome(port).await;
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

        let (browser, mut handler, owns_browser_process) = match config.backend {
            BrowserBackendKind::Managed => {
                match inspect_managed_browser_listener_state(config.port).await? {
                    ManagedBrowserListenerState::Managed => {
                        let endpoint =
                            resolve_local_browser_websocket_endpoint(config.port).await?;
                        let (browser, handler) = Browser::connect(endpoint)
                            .await
                            .map_err(|error| {
                                BrowserError::browser_operation(
                                    "connect to managed browser endpoint",
                                    error,
                                )
                            })?;
                        (browser, handler, false)
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
                        cleanup_stale_chrome(config.port).await;

                        let may_dir = may_dir.to_path_buf();
                        let browser_paths = tokio::task::spawn_blocking(move || {
                            let home = crate::session::MayHomePaths::new(may_dir);
                            (
                                managed_browser_executable(home.root()),
                                home.browser_profile_dir(),
                            )
                        })
                        .await
                        .map_err(|error| {
                            BrowserError::browser_operation(
                                "resolve managed browser paths",
                                std::io::Error::other(error.to_string()),
                            )
                        })?;
                        let (binary_path, profile_dir) = browser_paths;
                        cleanup_profile_lock_files(profile_dir.as_path()).await;
                        tokio::fs::create_dir_all(&profile_dir)
                            .await
                            .map_err(|error| {
                                BrowserError::browser_operation(
                                    "create browser profile dir",
                                    error,
                                )
                            })?;

                        let mut builder = BrowserConfig::builder()
                            .new_headless_mode()
                            .port(config.port)
                            .arg("--remote-allow-origins=*")
                            .arg("--remote-debugging-address=0.0.0.0")
                            .user_data_dir(profile_dir);

                        let path = binary_path.ok_or(BrowserError::NotProvisioned)?;
                        builder = builder.chrome_executable(path);

                        let browser_config = builder.build().map_err(|error| {
                            BrowserError::browser_operation(
                                "build browser launch config",
                                std::io::Error::other(error),
                            )
                        })?;
                        let (browser, handler) = Browser::launch(browser_config)
                            .await
                            .map_err(|error| {
                                BrowserError::browser_operation(
                                    "launch managed browser",
                                    error,
                                )
                            })?;
                        (browser, handler, true)
                    }
                }
            }
            BrowserBackendKind::RemoteCdp => {
                let endpoint = config
                    .remote_cdp_endpoint()
                    .map_err(|error| {
                        BrowserError::browser_operation("resolve remote cdp endpoint", error)
                    })?;
                let (browser, handler) = Browser::connect(endpoint)
                    .await
                    .map_err(|error| {
                        BrowserError::browser_operation("connect to remote cdp endpoint", error)
                    })?;
                (browser, handler, false)
            }
        };

        let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

        let page = Arc::new(
            browser
                .new_page("about:blank")
                .await
                .map_err(|error| BrowserError::browser_operation("open blank page", error))?,
        );

        let max_width = i64::from(config.screencast_max_width);
        let max_height = max_width * 5 / 8;
        page.execute(
            StartScreencastParams::builder()
                .format(StartScreencastFormat::Jpeg)
                .quality(i64::from(config.screencast_quality))
                .max_width(max_width)
                .max_height(max_height)
                .every_nth_frame(1)
                .build(),
        )
        .await
        .map_err(|error| BrowserError::browser_operation("start screencast", error))?;

        let (cdp_reader, sample_writer) = start_encoding_pipeline(
            page.clone(),
            self.video_track.clone(),
            config.screencast_quality,
        )
        .await?;
        let audio_writer = start_audio_pipeline(self.audio_track.clone());

        self.session = Some(BrowserSession {
            browser,
            page,
            owns_browser_process,
            _handler: handler_task,
            _cdp_reader: cdp_reader,
            _sample_writer: sample_writer,
            _audio_writer: audio_writer,
        });

        tracing::info!(backend = ?config.backend, "Browser started with WebRTC streaming");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), BrowserError> {
        let peers = std::mem::take(&mut self.peers);
        for (_, peer) in peers {
            let _ = peer.close().await;
        }

        if let Some(mut session) = self.session.take() {
            session._cdp_reader.abort();
            session._sample_writer.abort();
            session._audio_writer.abort();
            session._handler.abort();
            if session.owns_browser_process {
                let _ = session.browser.close().await;
            } else {
                let _ = session.page.as_ref().clone().close().await;
            }
            tracing::info!("Browser stopped");
        }

        Ok(())
    }

    async fn handle_offer(
        &mut self,
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
        let audio_rtp_sender = peer_connection
            .add_track(Arc::clone(&self.audio_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .map_err(|error| BrowserError::web_rtc_operation("add audio track", error))?;

        tokio::spawn(async move {
            let mut buf = vec![0u8; 1500];
            while video_rtp_sender.read(&mut buf).await.is_ok() {}
        });
        tokio::spawn(async move {
            let mut buf = vec![0u8; 1500];
            while audio_rtp_sender.read(&mut buf).await.is_ok() {}
        });

        setup_data_channel_handler(&peer_connection, page);

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
        Ok(local_desc.sdp)
    }

    async fn read_page(&self, url: &str) -> Result<BrowserPageSnapshot, BrowserError> {
        let page = self
            .session
            .as_ref()
            .ok_or(BrowserError::NotRunning)?
            .page
            .clone();
        read_page_snapshot(page, url).await
    }
}

async fn inspect_managed_browser_listener_state(
    port: u16,
) -> Result<ManagedBrowserListenerState, BrowserError> {
    tokio::task::spawn_blocking(move || inspect_managed_browser_listener_state_blocking(port))
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
        if String::from_utf8_lossy(&command.stdout).contains("chromiumoxide-runner") {
            return Ok(ManagedBrowserListenerState::Managed);
        }
    }

    Ok(ManagedBrowserListenerState::ExternalProcess)
}

async fn resolve_local_browser_websocket_endpoint(port: u16) -> Result<String, BrowserError> {
    let mut endpoint = String::from("http://127.0.0.1:");
    endpoint.push_str(&port.to_string());
    endpoint.push_str("/json/version");
    let response = reqwest::get(endpoint.as_str())
        .await
        .map_err(|error| BrowserError::browser_operation("fetch browser version metadata", error))?;
    if !response.status().is_success() {
        return Err(BrowserError::browser_operation(
            "validate browser version metadata response",
            std::io::Error::other(format!(
                "browser debugger metadata request to {} returned HTTP {}",
                endpoint,
                response.status()
            )),
        ));
    }
    let metadata: BrowserVersionMetadata = response
        .json()
        .await
        .map_err(|error| BrowserError::browser_operation("decode browser version metadata", error))?;
    metadata.web_socket_debugger_url.ok_or_else(|| {
        BrowserError::browser_operation(
            "read browser websocket endpoint",
            std::io::Error::other(format!(
                "browser debugger metadata at {} did not include webSocketDebuggerUrl",
                endpoint
            )),
        )
    })
}

async fn read_page_snapshot(
    page: Arc<Page>,
    url: &str,
) -> Result<BrowserPageSnapshot, BrowserError> {
    let trimmed_url = url.trim();
    if trimmed_url.is_empty() {
        return Err(BrowserError::InvalidInput(BrowserInputError::EmptyNavigationUrl));
    }

    page.goto(trimmed_url)
        .await
        .map_err(|error| BrowserError::browser_operation("navigate page for read", error))?;
    tokio::time::sleep(Duration::from_millis(3000)).await;

    let snapshot: BrowserPageSnapshot = page
        .evaluate("() => { const norm = (value) => typeof value === 'string' ? value.replace(/\\s+/g, ' ').trim() : ''; const headings = Array.from(document.querySelectorAll('h1, h2, h3')).map((node) => norm(node.innerText)).filter(Boolean).slice(0, 8); const bodyText = norm(document.body ? document.body.innerText : '').slice(0, 4000); return { url: location.href, title: norm(document.title), mainHeading: headings.length > 0 ? headings[0] : null, headings, textExcerpt: bodyText }; }")
        .await
        .map_err(|error| BrowserError::browser_operation("evaluate page snapshot script", error))?
        .into_value()
        .map_err(|error| BrowserError::browser_operation("decode page snapshot", error))?;

    Ok(snapshot)
}

async fn start_encoding_pipeline(
    page: Arc<Page>,
    video_track: Arc<TrackLocalStaticSample>,
    screencast_quality: u32,
) -> Result<(tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>), BrowserError> {
    let mut event_stream = page
        .event_listener::<EventScreencastFrame>()
        .await
        .map_err(|error| BrowserError::browser_operation("subscribe to screencast frames", error))?;

    let (jpeg_tx, jpeg_rx) = std::sync::mpsc::channel::<Vec<u8>>();
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

            if jpeg_tx.send(jpeg_bytes).is_err() {
                break;
            }
        }
        tracing::info!("CDP screencast reader stopped");
    });

    std::thread::Builder::new()
        .name("browser-encoder".into())
        .spawn(move || {
            encode_loop(jpeg_rx, sample_tx, screencast_quality);
        })
        .map_err(|error| BrowserError::encoder_operation("spawn browser encoder thread", error))?;

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

fn start_audio_pipeline(audio_track: Arc<TrackLocalStaticSample>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(20));
        let silence_frame = bytes::Bytes::from_static(&[0xF8, 0xFF, 0xFE]);
        loop {
            ticker.tick().await;
            if let Err(e) = audio_track
                .write_sample(&Sample {
                    data: silence_frame.clone(),
                    duration: Duration::from_millis(20),
                    ..Default::default()
                })
                .await
            {
                tracing::trace!(error = %e, "WebRTC audio sample write skipped");
            }
        }
    })
}

// ── H.264 encoding loop (dedicated thread) ─────────────────────────────────────

fn encode_loop(
    frame_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    sample_tx: tokio::sync::mpsc::Sender<EncodedSample>,
    quality: u32,
) {
    use openh264::OpenH264API;
    use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, UsageType};
    use openh264::formats::{RgbSliceU8, YUVBuffer};

    // Screen Content Real-Time mode: optimized for text/UI edges, solid backgrounds
    let config = EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .bitrate(BitRate::from_bps(quality * 40_000))
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

    while let Ok(jpeg_bytes) = frame_rx.recv() {
        // Decode JPEG → RGB
        let img = match image::load_from_memory_with_format(&jpeg_bytes, image::ImageFormat::Jpeg) {
            Ok(img) => img,
            Err(e) => {
                tracing::warn!(error = %e, "JPEG decode failed");
                continue;
            }
        };

        let rgb = img.to_rgb8();
        let (w, h) = (rgb.width() as usize, rgb.height() as usize);

        // Compute inter-frame duration
        let now = Instant::now();
        let duration = last_frame_time.map_or(Duration::from_millis(33), |t| now.duration_since(t));
        last_frame_time = Some(now);

        // Periodic keyframes for new viewers / packet loss recovery
        if now.duration_since(last_keyframe) > keyframe_interval {
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

fn setup_data_channel_handler(pc: &RTCPeerConnection, page: Arc<Page>) {
    pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        let page = page.clone();
        Box::pin(async move {
            let label = dc.label().to_owned();
            tracing::debug!(label, "DataChannel opened for browser input");

            dc.on_message(Box::new(move |msg: DataChannelMessage| {
                let page = page.clone();
                Box::pin(async move {
                    let text = match String::from_utf8(msg.data.to_vec()) {
                        Ok(s) => s,
                        Err(_) => return,
                    };
                    if let Err(e) = handle_input_message(&page, &text).await {
                        tracing::warn!(error = %e, "Browser input handling failed");
                    }
                })
            }));
        })
    }));
}

async fn handle_input_message(page: &Page, text: &str) -> Result<(), BrowserError> {
    let msg: InputMessage = serde_json::from_str(text)
        .map_err(|source| BrowserError::InvalidInputPayload { source })?;

    match msg {
        InputMessage::Mouse(input) => dispatch_mouse(page, input).await,
        InputMessage::Keyboard(input) => dispatch_keyboard(page, input).await,
        InputMessage::Navigate { url } => {
            let trimmed_url = url.trim();
            if trimmed_url.is_empty() {
                return Err(BrowserInputError::EmptyNavigationUrl.into());
            }
            page.goto(trimmed_url)
                .await
                .map_err(|error| BrowserError::browser_operation("navigate from data channel", error))?;
            Ok(())
        }
    }
}

async fn dispatch_mouse(page: &Page, input: MouseInput) -> Result<(), BrowserError> {
    let event_type = DispatchMouseEventType::from(input.event_type);
    let button = input.button.map_or(MouseButton::None, MouseButton::from);

    let mut builder = DispatchMouseEventParams::builder()
        .r#type(event_type)
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
        BrowserError::browser_operation(
            "build mouse dispatch params",
            std::io::Error::other(error),
        )
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
async fn cleanup_stale_chrome(port: u16) {
    let _ = tokio::task::spawn_blocking(move || {
        // Clean up singleton lock files
        let dir = std::env::temp_dir().join("chromiumoxide-runner");
        for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
            let path = dir.join(name);
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }
        }

        // Kill any Chrome processes using our port that have chromiumoxide-runner user-data-dir.
        // This avoids killing the user's regular Chrome.
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
            if args.contains("chromiumoxide-runner") {
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
fn managed_browser_executable(may_dir: &Path) -> Option<String> {
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
        .map(|entry| entry.path().to_string_lossy().to_string())
}
