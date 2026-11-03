//! A session desktop's video path: the virtual display's captured
//! surfaces through the shared encoder onto one WebRTC video track,
//! with a peer per watching client.
//!
//! The runtime mirrors the browser's (`container::browser`) — one
//! `TrackLocalStaticSample` feeds every peer, RTCP PLI/FIR and bandwidth
//! estimates reach the encoder, and a disconnecting peer is dropped — minus
//! everything a browser needs and a desktop does not (CDP, DOM reads,
//! input data channel).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::media::Sample;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::config::StreamCodec;

use super::capture::{CapturedFrame, DisplayCapture};
use super::input::{self, InputState};
use super::proto::InputMessage;
use super::{DisplayError, PlatformDisplay, SessionDisplay};
use crate::video::{
    EncodedSample, EncoderControl, FRAME_QUEUE_CAPACITY, GateChange, OfferCodecError, RawFrame,
    SourceGate, StreamFormat, VideoCodec, encode_loop, encoder_control,
};

/// Why a viewer's offer got no answer.
#[derive(Debug, thiserror::Error)]
pub enum OfferError {
    /// The viewer's offer is the problem: it does not parse, or it cannot
    /// receive the stream's codec.
    #[error(transparent)]
    Codec(#[from] OfferCodecError),
    #[error(transparent)]
    Display(#[from] DisplayError),
}

/// Encoder bitrate for a desktop stream. The display is 1920×1200@2x —
/// one step above the browser's viewport default.
const DESKTOP_BITRATE_KBPS: u32 = 8_000;

/// Frames queued for the encoder. Every queued frame keeps its capture
/// surface in use, so the queue stays well inside the capture stream's depth.
const DESKTOP_FRAME_QUEUE: usize = 4;

/// One session's desktop stream — the holder a route asks for.
///
/// `Some` while running: capture on, encoder live, peers attachable.
/// `None` is the surface's asleep state — the display itself is owned
/// by [`super::SessionDisplays`] and outlives a stopped stream so the
/// desktop keeps its windows.
pub struct SessionStream {
    runtime: Arc<Mutex<Option<StreamRuntime>>>,
    takeover_active: Arc<AtomicBool>,
}

impl std::fmt::Debug for SessionStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStream").finish_non_exhaustive()
    }
}

struct StreamRuntime {
    inputs: Arc<std::sync::Mutex<InputState>>,
    // Runs only while `peers` is non-empty (`sync_capture`). Its Drop stops
    // `CGDisplayStream` and frees the dispatch queue. The frame channel
    // closes with it, which ends the bridge, encoder and writer tasks.
    capture: DisplayCapture,
    capture_gate: SourceGate,
    // The display `DataChannel` input posts into.
    display: Arc<PlatformDisplay>,
    // Set while the session's user is driving the desktop; `DataChannel`
    // input is dropped otherwise, as in the browser's takeover gate.
    takeover_active: Arc<AtomicBool>,
    video_track: Arc<TrackLocalStaticSample>,
    webrtc_api: webrtc::api::API,
    codec: VideoCodec,
    peers: HashMap<String, Arc<RTCPeerConnection>>,
    control: EncoderControl,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for StreamRuntime {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            for (_, peer) in self.peers.drain() {
                handle.spawn(async move {
                    let _ = peer.close().await;
                });
            }
        } else {
            self.peers.clear();
        }
    }
}

impl StreamRuntime {
    /// Brings the display capture in line with the peers map: running while
    /// a viewer is attached, stopped otherwise so nothing is captured or
    /// encoded for nobody.
    fn sync_capture(&mut self) -> Result<(), DisplayError> {
        let viewers = self.peers.len();
        let Some(change) = self.capture_gate.required(viewers) else {
            return Ok(());
        };
        match change {
            GateChange::Start => self.capture.resume()?,
            GateChange::Stop => self.capture.pause(),
        }
        self.capture_gate.applied(change);
        tracing::debug!(?change, viewers, "computer capture gate");
        Ok(())
    }
}

impl Default for SessionStream {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStream {
    /// A stream holder in the asleep state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runtime: Arc::new(Mutex::new(None)),
            takeover_active: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether the stream is live (a viewer may attach).
    pub async fn is_running(&self) -> bool {
        self.runtime.lock().await.is_some()
    }

    /// Starts capturing `display` and encoding. Idempotent while a
    /// stream is already up — a second start is a no-op, matching the
    /// browser's wake-once semantics.
    ///
    /// # Errors
    /// Returns [`DisplayError`] when codec/interceptor registration or
    /// the display's capture fails.
    #[allow(clippy::significant_drop_tightening)]
    pub async fn start(
        &self,
        display: Arc<PlatformDisplay>,
        inputs: Arc<std::sync::Mutex<super::input::InputState>>,
        preference: StreamCodec,
    ) -> Result<(), DisplayError> {
        let mut guard = self.runtime.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        let codec = select_codec(&display, preference)?;
        self.start_locked(&mut guard, display, inputs, codec)
    }

    /// Moves a running stream to the codec `preference` selects: its peers
    /// close (viewers reconnect) and the capture and encoder restart. A
    /// stream that is asleep stays asleep. The codec is selected before
    /// anything stops, so an unavailable codec leaves the stream running.
    ///
    /// # Errors
    /// Returns [`DisplayError`] when the codec cannot be selected or the
    /// restarted stream fails to start.
    #[allow(clippy::significant_drop_tightening)]
    pub async fn restart(&self, preference: StreamCodec) -> Result<(), DisplayError> {
        let mut guard = self.runtime.lock().await;
        let Some(running) = guard.as_ref() else {
            return Ok(());
        };
        let (display, inputs) = (running.display.clone(), running.inputs.clone());
        let codec = select_codec(&display, preference)?;
        *guard = None;
        self.start_locked(&mut guard, display, inputs, codec)
    }

    fn start_locked(
        &self,
        guard: &mut Option<StreamRuntime>,
        display: Arc<PlatformDisplay>,
        inputs: Arc<std::sync::Mutex<super::input::InputState>>,
        codec: VideoCodec,
    ) -> Result<(), DisplayError> {
        let (webrtc_api, video_track) = codec
            .webrtc("video", "session-desktop")
            .map_err(|e| DisplayError(format!("build webrtc stack: {e}")))?;

        let (control, requests) = encoder_control();
        let (frame_tx, frame_rx) = tokio::sync::mpsc::channel(DESKTOP_FRAME_QUEUE);
        let (sample_tx, mut sample_rx) = tokio::sync::mpsc::channel::<EncodedSample>(16);
        let (captured_tx, mut captured_rx) =
            tokio::sync::mpsc::channel::<CapturedFrame>(FRAME_QUEUE_CAPACITY);

        let capture = DisplayCapture::new(display.display_id(), display.size(), captured_tx)?;
        // The display backs 2x; the capture surfaces physical pixels.
        // `size()` reports points — recheck against the first frame if
        // the stream's output size ever differs from the surface's.

        // Captured surfaces → encoder frames. try_send keeps a stuck
        // encoder from growing the queue: a dropped desktop frame loses
        // nothing the next one does not carry.
        let bridge = tokio::spawn(async move {
            while let Some(frame) = captured_rx.recv().await {
                let raw = RawFrame::Surface(frame.surface);
                match frame_tx.try_send(raw) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        tracing::debug!("computer encoder queue saturated, dropping frame");
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
            tracing::info!("computer capture bridge stopped");
        });

        let encoder = tokio::task::spawn_blocking(move || {
            encode_loop(frame_rx, &sample_tx, codec, &requests);
        });

        let writer_track = video_track.clone();
        let writer = tokio::spawn(async move {
            while let Some(encoded) = sample_rx.recv().await {
                if let Err(e) = writer_track
                    .write_sample(&Sample {
                        data: bytes::Bytes::from(encoded.data),
                        duration: encoded.duration,
                        ..Default::default()
                    })
                    .await
                {
                    tracing::trace!(error = %e, "computer WebRTC sample write skipped");
                }
            }
            tracing::info!("computer WebRTC sample writer stopped");
        });

        *guard = Some(StreamRuntime {
            inputs,
            capture,
            capture_gate: SourceGate::stopped(),
            display,
            takeover_active: self.takeover_active.clone(),
            video_track,
            webrtc_api,
            codec,
            peers: HashMap::new(),
            control,
            tasks: vec![bridge, encoder, writer],
        });
        Ok(())
    }

    /// Whether the session's user currently owns the desktop.
    #[must_use]
    pub fn takeover_active(&self) -> bool {
        self.takeover_active.load(Ordering::Acquire)
    }

    /// Starts (`true`) or ends (`false`) user takeover: while set, the
    /// stream's `DataChannel` input reaches the display.
    pub fn set_takeover(&self, active: bool) {
        self.takeover_active.store(active, Ordering::Release);
    }

    /// Stops capture, encoding and every peer. The display stays alive
    /// for the session — only its stream sleeps.
    pub async fn stop(&self) {
        let runtime = self.runtime.lock().await.take();
        drop(runtime);
    }

    /// Answers a viewer's WebRTC offer with this stream's video track.
    ///
    /// # Errors
    /// Returns [`OfferError::Codec`] when the offer is malformed or lacks the
    /// stream's codec, and [`OfferError::Display`] when the stream is asleep
    /// or the peer cannot be created or negotiated.
    #[allow(clippy::significant_drop_tightening)]
    pub async fn handle_offer(&self, offer_sdp: String) -> Result<String, OfferError> {
        let mut guard = self.runtime.lock().await;
        let runtime = guard
            .as_mut()
            .ok_or_else(|| DisplayError("the session desktop is asleep".to_owned()))?;
        runtime.codec.check_offer(&offer_sdp)?;

        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let peer = Arc::new(
            runtime
                .webrtc_api
                .new_peer_connection(config)
                .await
                .map_err(|e| DisplayError(format!("create peer connection: {e}")))?,
        );
        let rtp_sender = peer
            .add_track(runtime.video_track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .map_err(|e| DisplayError(format!("add video track: {e}")))?;

        // Viewer RTCP → encoder: keyframe requests and bandwidth estimates.
        let peer_id = crate::id::new();
        tokio::spawn(
            runtime
                .control
                .clone()
                .follow_peer(rtp_sender, peer_id.clone()),
        );

        let state_for_peer = self.runtime.clone();
        let gone_id = peer_id.clone();
        peer.on_peer_connection_state_change(Box::new(move |state| {
            let state_for_peer = state_for_peer.clone();
            let gone_id = gone_id.clone();
            Box::pin(async move {
                // `Disconnected` is transient: ICE recovers or moves on to `Failed`.
                if matches!(
                    state,
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                ) {
                    forget_peer(&state_for_peer, &gone_id).await;
                }
            })
        }));

        setup_input_channel(
            &peer,
            runtime.display.clone(),
            runtime.takeover_active.clone(),
            runtime.inputs.clone(),
            self.runtime.clone(),
            peer_id.clone(),
        );

        let offer = RTCSessionDescription::offer(offer_sdp)
            .map_err(|e| DisplayError(format!("parse offer: {e}")))?;
        peer.set_remote_description(offer)
            .await
            .map_err(|e| DisplayError(format!("set remote description: {e}")))?;
        let answer = peer
            .create_answer(None)
            .await
            .map_err(|e| DisplayError(format!("create answer: {e}")))?;
        let mut gather = peer.gathering_complete_promise().await;
        runtime.control.request_keyframe();
        peer.set_local_description(answer)
            .await
            .map_err(|e| DisplayError(format!("set local description: {e}")))?;
        tokio::time::timeout(Duration::from_secs(10), gather.recv())
            .await
            .map_err(|_| DisplayError("ICE gathering timed out".to_owned()))?;
        let local = peer
            .local_description()
            .await
            .ok_or_else(|| DisplayError("no local description after gathering".to_owned()))?;

        // The keyframe requested above is read at the first frame of the
        // capture this starts, so the viewer's first frame is an IDR.
        runtime.peers.insert(peer_id.clone(), peer);
        if let Err(error) = runtime.sync_capture() {
            if let Some(peer) = runtime.peers.remove(&peer_id) {
                tokio::spawn(async move {
                    let _ = peer.close().await;
                });
            }
            return Err(error.into());
        }
        Ok(local.sdp)
    }
}

fn select_codec(
    display: &PlatformDisplay,
    preference: StreamCodec,
) -> Result<VideoCodec, DisplayError> {
    let (width, height) = display.size();
    VideoCodec::select(
        StreamFormat {
            max_width: width,
            max_height: height,
            initial_bitrate_kbps: DESKTOP_BITRATE_KBPS,
        },
        preference,
    )
    .map_err(|e| DisplayError(format!("select video codec: {e}")))
}

/// Forgets a viewer that left and closes its connection. Closing stops the
/// RTP sender, which ends the viewer's RTCP follower and takes its estimate
/// out of the encoder's minimum. It runs apart from the caller, which may be
/// a webrtc handler that `close` raises again.
async fn forget_peer(runtime: &Mutex<Option<StreamRuntime>>, peer_id: &str) {
    let gone = runtime.lock().await.as_mut().and_then(|runtime| {
        let gone = runtime.peers.remove(peer_id);
        if gone.is_some()
            && let Err(error) = runtime.sync_capture()
        {
            tracing::warn!(%error, "pausing the display capture failed");
        }
        gone
    });
    if let Some(peer) = gone {
        tracing::debug!(peer_id, "computer peer went away");
        tokio::spawn(async move {
            if let Err(error) = peer.close().await {
                tracing::debug!(%error, "closing a computer stream peer failed");
            }
        });
    }
}

/// Routes a peer's `DataChannel` input into the display while takeover
/// is held. Each channel carries its own held-button state so drags
/// survive across messages.
fn setup_input_channel(
    peer: &Arc<RTCPeerConnection>,
    display: Arc<PlatformDisplay>,
    takeover_active: Arc<AtomicBool>,
    inputs: Arc<std::sync::Mutex<super::input::InputState>>,
    runtime: Arc<Mutex<Option<StreamRuntime>>>,
    peer_id: String,
) {
    peer.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        let display = display.clone();
        let takeover_active = takeover_active.clone();
        let inputs = inputs.clone();
        let runtime = runtime.clone();
        let peer_id = peer_id.clone();
        Box::pin(async move {
            // The channel closes with the viewer's connection, long before ICE
            // gives up on a viewer that closed: webrtc does not move the
            // connection to `Closed` on the viewer's DTLS close alert.
            dc.on_close(Box::new(move || {
                let runtime = runtime.clone();
                let peer_id = peer_id.clone();
                Box::pin(async move { forget_peer(&runtime, &peer_id).await })
            }));
            dc.on_message(Box::new(move |message: DataChannelMessage| {
                let display = display.clone();
                let takeover_active = takeover_active.clone();
                let inputs = inputs.clone();
                Box::pin(async move {
                    if !takeover_active.load(Ordering::Acquire) {
                        return;
                    }
                    let Ok(input) = serde_json::from_slice::<InputMessage>(&message.data) else {
                        return;
                    };
                    let mut state = inputs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Err(error) = input::post_input(display.display_id(), input, &mut state) {
                        tracing::debug!(%error, "computer input dropped");
                    }
                })
            }));
        })
    }));
}
