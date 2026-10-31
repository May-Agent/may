//! A session desktop's video path: the virtual display's captured
//! frames through the shared H.264 encoder onto one WebRTC video track,
//! with a peer per watching client.
//!
//! The runtime mirrors the browser's (`container::browser`) — one
//! `TrackLocalStaticSample` feeds every peer, RTCP PLI/FIR flips a
//! shared keyframe request, and a disconnecting peer is dropped — minus
//! everything a browser needs and a desktop does not (CDP, DOM reads,
//! input data channel).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MIME_TYPE_H264, MediaEngine};
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

use super::capture::{CapturedFrame, DisplayCapture};
use super::{DisplayError, PlatformDisplay, SessionDisplay};
use crate::video::{BgraFrame, EncodedSample, FRAME_QUEUE_CAPACITY, RawFrame, encode_loop};

/// Encoder bitrate for a desktop stream. The display is 1920×1200@2x —
/// one step above the browser's viewport default.
const DESKTOP_BITRATE_KBPS: u32 = 8_000;

/// One session's desktop stream — the holder a route asks for.
///
/// `Some` while running: capture on, encoder live, peers attachable.
/// `None` is the surface's asleep state — the display itself is owned
/// by [`super::SessionDisplays`] and outlives a stopped stream so the
/// desktop keeps its windows.
pub struct SessionStream {
    runtime: Arc<Mutex<Option<StreamRuntime>>>,
}

impl std::fmt::Debug for SessionStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStream").finish_non_exhaustive()
    }
}

struct StreamRuntime {
    // Held, never touched: its Drop stops `CGDisplayStream` and frees
    // the dispatch queue. The frame channel closes with it, which ends
    // the bridge, encoder and writer tasks.
    _capture: DisplayCapture,
    video_track: Arc<TrackLocalStaticSample>,
    webrtc_api: webrtc::api::API,
    peers: HashMap<String, Arc<RTCPeerConnection>>,
    keyframe_request: Arc<AtomicBool>,
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
    pub async fn start(&self, display: Arc<PlatformDisplay>) -> Result<(), DisplayError> {
        let mut guard = self.runtime.lock().await;
        if guard.is_some() {
            return Ok(());
        }

        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .map_err(|e| DisplayError(format!("register default codecs: {e}")))?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .map_err(|e| DisplayError(format!("register default interceptors: {e}")))?;
        let webrtc_api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90_000,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                        .to_owned(),
                ..Default::default()
            },
            "video".to_owned(),
            "session-desktop".to_owned(),
        ));

        let keyframe_request = Arc::new(AtomicBool::new(false));
        let (frame_tx, frame_rx) = tokio::sync::mpsc::channel(FRAME_QUEUE_CAPACITY);
        let (sample_tx, mut sample_rx) = tokio::sync::mpsc::channel::<EncodedSample>(16);
        let (captured_tx, mut captured_rx) =
            tokio::sync::mpsc::channel::<CapturedFrame>(FRAME_QUEUE_CAPACITY);

        let capture = DisplayCapture::start(display.display_id(), display.size(), captured_tx)?;
        // The display backs 2x; the capture surfaces physical pixels.
        // `size()` reports points — recheck against the first frame if
        // the stream's output size ever differs from the surface's.

        // Captured surfaces → encoder frames. try_send keeps a stuck
        // encoder from growing the queue: a dropped desktop frame loses
        // nothing the next one does not carry.
        let bridge = tokio::spawn(async move {
            while let Some(frame) = captured_rx.recv().await {
                if frame.bgra.is_empty() {
                    continue;
                }
                let raw = RawFrame::Bgra(BgraFrame {
                    data: frame.bgra,
                    stride: frame.bytes_per_row,
                    width: frame.width,
                    height: frame.height,
                });
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

        let encode_keyframe = keyframe_request.clone();
        let encoder = tokio::task::spawn_blocking(move || {
            encode_loop(frame_rx, &sample_tx, DESKTOP_BITRATE_KBPS, &encode_keyframe);
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
            _capture: capture,
            video_track,
            webrtc_api,
            peers: HashMap::new(),
            keyframe_request,
            tasks: vec![bridge, encoder, writer],
        });
        Ok(())
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
    /// Returns [`DisplayError`] when the stream is asleep, the offer is
    /// malformed, or the peer cannot be created or negotiated.
    #[allow(clippy::significant_drop_tightening)]
    pub async fn handle_offer(&self, offer_sdp: String) -> Result<String, DisplayError> {
        let mut guard = self.runtime.lock().await;
        let runtime = guard
            .as_mut()
            .ok_or_else(|| DisplayError("the session desktop is asleep".to_owned()))?;

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

        // Viewer RTCP → encoder hints, as in the browser stream.
        let keyframe_for_rtcp = runtime.keyframe_request.clone();
        tokio::spawn(async move {
            while let Ok((packets, _)) = rtp_sender.read_rtcp().await {
                for packet in packets {
                    let any = packet.as_any();
                    if any
                        .downcast_ref::<webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication>()
                        .is_some()
                        || any
                            .downcast_ref::<webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest>()
                            .is_some()
                    {
                        keyframe_for_rtcp.store(true, Ordering::Release);
                    }
                }
            }
        });

        let peer_id = uuid::Uuid::new_v4().to_string();
        let state_for_peer = self.runtime.clone();
        let gone_id = peer_id.clone();
        peer.on_peer_connection_state_change(Box::new(move |state| {
            let state_for_peer = state_for_peer.clone();
            let gone_id = gone_id.clone();
            Box::pin(async move {
                if matches!(
                    state,
                    RTCPeerConnectionState::Disconnected
                        | RTCPeerConnectionState::Failed
                        | RTCPeerConnectionState::Closed
                ) {
                    #[allow(clippy::significant_drop_tightening)]
                    let mut guard = state_for_peer.lock().await;
                    if let Some(runtime) = guard.as_mut() {
                        runtime.peers.remove(&gone_id);
                    }
                    tracing::debug!(peer_id = %gone_id, "computer peer went away");
                }
            })
        }));

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
        runtime.keyframe_request.store(true, Ordering::Release);
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

        runtime.peers.insert(peer_id, peer);
        Ok(local.sdp)
    }
}
