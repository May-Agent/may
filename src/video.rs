//! The shared screen-content video path every streamed surface uses.
//!
//! Raw frames in, encoded samples out for a WebRTC `TrackLocalStaticSample`.
//! The encoder is `mediax-codec`: hardware H.264 (Constrained Baseline,
//! Annex B) where the host has an encoder, software AV1 otherwise. The codec
//! is chosen once per stream by [`VideoCodec::select`] and the WebRTC stack
//! built by [`VideoCodec::webrtc`] advertises exactly that codec.
//!
//! Two sources feed it: the managed browser's CDP screencast JPEGs and a
//! session desktop's `DisplayStream` surfaces, so the frame type is
//! [`RawFrame`]. Where the host allows, neither touches the CPU: the surface
//! goes to the encoder as a `CVPixelBuffer` and a JPEG is decoded by the
//! platform into one.

mod gate;
mod level;
mod nv12;
mod pipeline;
mod rate;
mod send_time;
#[cfg(target_os = "macos")]
mod surface;
#[cfg(test)]
mod tests;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use crate::config::StreamCodec;
use mediax_codec::{
    Bitrate, BitstreamFormat, CodecError, Encoder, EncoderCodec, EncoderConfig, EncoderProfile,
    EvenDimension, FrameRate, H264Profile, H264Settings,
};
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::{configure_nack, configure_rtcp_reports};
use webrtc::api::media_engine::{MIME_TYPE_AV1, MIME_TYPE_H264, MediaEngine};
use webrtc::interceptor::registry::Registry;
use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
use webrtc::rtcp::receiver_report::ReceiverReport;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTCRtpHeaderExtensionCapability, RTPCodecType,
};
use webrtc::rtp_transceiver::{RTCPFeedback, rtp_sender::RTCRtpSender};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

pub use gate::{GateChange, SourceGate};
use pipeline::FramePipeline;
use rate::{BitrateTarget, Reception};
#[cfg(target_os = "macos")]
pub use surface::PixelBuffer;

/// Capacity of the frame → encoder queue. Tuned so a transient encoder
/// stall (~250 ms at 30 fps) absorbs without dropping frames, while still
/// catching unbounded growth on a stuck consumer.
pub const FRAME_QUEUE_CAPACITY: usize = 8;

/// The frame rate the encoder and the H.264 level are sized for.
pub const STREAM_FRAME_RATE: u32 = 30;

/// The longest span between keyframes; viewer joins and PLI/FIR ask for
/// earlier ones.
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);

/// RTP payload type of the one codec a stream offers (dynamic range).
const PAYLOAD_TYPE: u8 = 96;

/// An encoded video frame ready for WebRTC.
pub struct EncodedSample {
    pub data: Vec<u8>,
    pub duration: Duration,
}

/// A frame on its way to the encoder. Each source supplies what it already
/// has: the browser hands JPEG bytes off the wire, a display hands the
/// captured surface.
#[derive(Debug)]
pub enum RawFrame {
    /// An encoded JPEG frame (the browser's CDP screencast).
    Jpeg(Vec<u8>),
    /// A captured display surface, handed over without a copy.
    #[cfg(target_os = "macos")]
    Surface(PixelBuffer),
}

/// Why a frame or the stream could not be encoded.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("the {width}x{height} stream needs more than H.264 level 5.2")]
    StreamTooLarge { width: u32, height: u32 },
    #[error(
        "the stream codec is set to H.264 only, but this host has no hardware H.264 encoder: {0}"
    )]
    H264Unavailable(String),
    #[error("stream dimension {0} is not usable")]
    Dimension(u32),
    #[error("frame is {width}x{height}, above the stream's {max_width}x{max_height}")]
    FrameTooLarge {
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
    },
    #[error("the encoder failed: {0}")]
    Codec(#[from] CodecError),
    #[error("JPEG decode failed: {0}")]
    Jpeg(#[from] image::ImageError),
    #[error("NV12 conversion failed: {0}")]
    Convert(#[from] yuv::YuvError),
    #[cfg(target_os = "macos")]
    #[error("CoreVideo refused the surface: status {0}")]
    Surface(i32),
    #[cfg(target_os = "macos")]
    #[error("pixel buffer format {0:#010x} is not BGRA")]
    PixelFormat(u32),
}

/// A viewer's offer that cannot use the stream's codec.
#[derive(Debug, thiserror::Error)]
pub enum OfferCodecError {
    #[error("the viewer's offer is not valid SDP: {0}")]
    Invalid(#[from] webrtc::sdp::Error),
    #[error(
        "this stream sends {codec} only (the stream codec setting), and the viewer's offer does not include {codec}"
    )]
    Missing { codec: &'static str },
}

/// The size and rate a stream is sized for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    /// The largest frame the source will deliver.
    pub max_width: u32,
    pub max_height: u32,
    pub initial_bitrate_kbps: u32,
}

/// The codec a stream uses, fixed for its lifetime.
#[derive(Debug, Clone, Copy)]
pub struct VideoCodec {
    codec: EncoderCodec,
    format: StreamFormat,
}

impl VideoCodec {
    /// Picks the codec the `preference` allows: hardware H.264 at the lowest
    /// level admitting the stream, or software AV1.
    ///
    /// `Auto` takes software AV1 on `CodecError::Unsupported` (the project's
    /// decided policy for hosts without an encoder); `H264` never does.
    ///
    /// # Errors
    /// Returns [`FrameError`] when H.264 is wanted and the stream exceeds
    /// level 5.2, the host has no hardware encoder under `H264`, or the
    /// encoder fails for another reason.
    pub fn select(format: StreamFormat, preference: StreamCodec) -> Result<Self, FrameError> {
        Self::select_with(format, preference, |config| Encoder::new(config).map(drop))
    }

    /// [`select`](Self::select) with the hardware H.264 probe supplied by the
    /// caller: it opens (and drops) an encoder for the given config.
    ///
    /// `Auto` falls back to AV1 on `CodecError::Unsupported`; `H264` fails
    /// with [`FrameError::H264Unavailable`] instead; `Av1` never probes.
    ///
    /// # Errors
    /// See [`select`](Self::select).
    pub fn select_with(
        format: StreamFormat,
        preference: StreamCodec,
        probe: impl FnOnce(EncoderConfig) -> Result<(), CodecError>,
    ) -> Result<Self, FrameError> {
        let av1 = Self {
            codec: EncoderCodec::Av1,
            format,
        };
        if preference == StreamCodec::Av1 {
            return Ok(av1);
        }
        let level = level::h264_level(format.max_width, format.max_height, STREAM_FRAME_RATE)
            .ok_or(FrameError::StreamTooLarge {
                width: format.max_width,
                height: format.max_height,
            })?;
        let h264 = Self {
            codec: EncoderCodec::H264(H264Settings {
                profile: H264Profile::ConstrainedBaseline,
                level,
                bitstream: BitstreamFormat::AnnexB,
            }),
            format,
        };
        let (width, height) = h264.ceiling()?;
        match probe(h264.encoder_config(width, height, h264.initial_bitrate())) {
            Ok(()) => Ok(h264),
            Err(CodecError::Unsupported(reason)) if preference == StreamCodec::Auto => {
                tracing::info!(%reason, "no hardware H.264 encoder; streaming AV1");
                Ok(av1)
            }
            Err(CodecError::Unsupported(reason)) => Err(FrameError::H264Unavailable(reason)),
            Err(error) => Err(error.into()),
        }
    }

    /// The codec's name as a viewer-facing message spells it.
    pub const fn name(self) -> &'static str {
        match self.codec {
            EncoderCodec::H264(_) => "H.264",
            EncoderCodec::Av1 => "AV1",
            EncoderCodec::H265(_) => "H.265",
        }
    }

    /// Checks that a viewer's SDP offer lists this codec in a video section.
    ///
    /// # Errors
    /// Returns [`OfferCodecError::Invalid`] when the offer does not parse,
    /// and [`OfferCodecError::Missing`] naming the codec when no video
    /// section of the offer lists it.
    pub fn check_offer(self, offer_sdp: &str) -> Result<(), OfferCodecError> {
        let rtp_name = match self.codec {
            EncoderCodec::H264(_) => "H264",
            EncoderCodec::Av1 => "AV1",
            EncoderCodec::H265(_) => "H265",
        };
        let offer = webrtc::sdp::SessionDescription::unmarshal(&mut std::io::Cursor::new(
            offer_sdp.as_bytes(),
        ))?;
        let offered = offer
            .media_descriptions
            .iter()
            .filter(|media| media.media_name.media == "video")
            .flat_map(|media| &media.media_name.formats)
            .filter_map(|format| format.parse::<u8>().ok())
            .filter_map(|payload_type| offer.get_codec_for_payload_type(payload_type).ok())
            .any(|codec| codec.name.eq_ignore_ascii_case(rtp_name));
        if offered {
            Ok(())
        } else {
            Err(OfferCodecError::Missing { codec: self.name() })
        }
    }

    /// Whether frames can reach the encoder as platform surfaces.
    #[cfg(target_os = "macos")]
    pub(crate) const fn takes_surfaces(self) -> bool {
        matches!(self.codec, EncoderCodec::H264(_))
    }

    pub(crate) fn ceiling(self) -> Result<(EvenDimension, EvenDimension), FrameError> {
        let even = |pixels: u32| {
            EvenDimension::new(pixels & !1).map_err(|_| FrameError::Dimension(pixels))
        };
        Ok((even(self.format.max_width)?, even(self.format.max_height)?))
    }

    pub(crate) fn initial_bitrate(self) -> Bitrate {
        let bits = self.format.initial_bitrate_kbps.saturating_mul(1000).max(1);
        Bitrate::new(NonZeroU32::new(bits).expect("clamped to at least one"))
    }

    // `VideoColorInfo` lives in `mediax-core`, which this crate does not name.
    #[allow(clippy::default_trait_access)]
    pub(crate) fn encoder_config(
        self,
        width: EvenDimension,
        height: EvenDimension,
        bitrate: Bitrate,
    ) -> EncoderConfig {
        EncoderConfig {
            codec: self.codec,
            width,
            height,
            // BT.709, limited range: what `nv12` converts to.
            color: Default::default(),
            profile: EncoderProfile::Realtime,
            bitrate,
            frame_rate: FrameRate::new(
                NonZeroU32::new(STREAM_FRAME_RATE).expect("non-zero"),
                NonZeroU32::MIN,
            ),
            keyframe_interval: Some(KEYFRAME_INTERVAL),
        }
    }

    /// The codec as the SDP offers it. H.264 carries `packetization-mode=1`
    /// and the encoder's own `profile-level-id`.
    pub fn capability(self) -> RTCRtpCodecCapability {
        let (mime_type, sdp_fmtp_line) = match self.codec {
            EncoderCodec::H264(settings) => {
                let [profile, constraints, level] = settings.profile_level_id();
                (
                    MIME_TYPE_H264,
                    format!(
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile:02x}{constraints:02x}{level:02x}"
                    ),
                )
            }
            EncoderCodec::Av1 => (MIME_TYPE_AV1, "profile-id=0".to_owned()),
            EncoderCodec::H265(_) => unreachable!("H.265 is never selected"),
        };
        RTCRtpCodecCapability {
            mime_type: mime_type.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line,
            // NACK and PLI come from the interceptor registry; these two are
            // the bitrate estimate and the keyframe request.
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: "goog-remb".to_owned(),
                    parameter: String::new(),
                },
                RTCPFeedback {
                    typ: "ccm".to_owned(),
                    parameter: "fir".to_owned(),
                },
            ],
        }
    }

    /// A WebRTC API whose media engine knows exactly this codec, and the
    /// track every peer of the stream receives.
    ///
    /// Transport-cc is deliberately not negotiated: webrtc-rs 0.17 has no
    /// congestion controller to read it, and with only `goog-remb` offered
    /// the viewer reports its estimate as REMB.
    ///
    /// # Errors
    /// Returns the WebRTC error when registration fails.
    pub fn webrtc(
        self,
        track_id: &str,
        stream_id: &str,
    ) -> Result<(webrtc::api::API, Arc<TrackLocalStaticSample>), webrtc::Error> {
        let mut media_engine = MediaEngine::default();
        media_engine.register_codec(
            RTCRtpCodecParameters {
                capability: self.capability(),
                payload_type: PAYLOAD_TYPE,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
        media_engine.register_header_extension(
            RTCRtpHeaderExtensionCapability {
                uri: send_time::ABS_SEND_TIME_URI.to_owned(),
            },
            RTPCodecType::Video,
            None,
        )?;
        // Interceptors wrap the transport in registration order, so the
        // first is the one next to the socket: the send time is stamped
        // there, on NACK retransmissions as well.
        let mut registry = Registry::new();
        registry.add(Box::new(send_time::SendTimeBuilder));
        let registry = configure_nack(registry, &mut media_engine);
        let registry = configure_rtcp_reports(registry);
        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();
        let track = Arc::new(TrackLocalStaticSample::new(
            self.capability(),
            track_id.to_owned(),
            stream_id.to_owned(),
        ));
        Ok((api, track))
    }
}

// ── Encoder requests ───────────────────────────────────────────────────────────

/// What the transport asks of the encoder.
#[derive(Debug)]
enum EncoderRequest {
    Keyframe,
    Bitrate {
        peer: String,
        bits_per_second: u32,
    },
    /// The peer's RTCP reception counters.
    Reception {
        peer: String,
        reception: Reception,
        at: Instant,
    },
    PeerGone {
        peer: String,
    },
}

/// The transport's handle on a stream's encoder: keyframe requests and
/// bitrate estimates, delivered to the encoder thread between frames.
#[derive(Debug, Clone)]
pub struct EncoderControl {
    tx: Sender<EncoderRequest>,
}

/// The encoder thread's end of an [`EncoderControl`].
#[derive(Debug)]
pub struct EncoderRequests {
    rx: Receiver<EncoderRequest>,
}

/// A control handle and the receiving end for the encoder thread.
pub fn encoder_control() -> (EncoderControl, EncoderRequests) {
    let (tx, rx) = std::sync::mpsc::channel();
    (EncoderControl { tx }, EncoderRequests { rx })
}

impl EncoderControl {
    /// The next frame is an IDR (viewer join, PLI, FIR).
    pub fn request_keyframe(&self) {
        // The encoder thread is gone when the stream stops; nothing to ask.
        let _ = self.tx.send(EncoderRequest::Keyframe);
    }

    /// Follows `peer`'s feedback on `sender` until the peer goes away: PLI and
    /// FIR request a keyframe, REMB reports the peer's bandwidth estimate and
    /// receiver reports its loss.
    pub async fn follow_peer(self, sender: Arc<RTCRtpSender>, peer: String) {
        while let Ok((packets, _)) = sender.read_rtcp().await {
            for packet in packets {
                let any = packet.as_any();
                if any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>() {
                    tracing::debug!("RTCP PLI/FIR received; requesting IDR");
                    self.request_keyframe();
                } else if let Some(remb) = any.downcast_ref::<ReceiverEstimatedMaximumBitrate>() {
                    // `bitrate` is a non-negative estimate in bits per second.
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let bits_per_second = remb.bitrate as u32;
                    tracing::trace!(%peer, bits_per_second, ssrcs = ?remb.ssrcs, "REMB");
                    let _ = self.tx.send(EncoderRequest::Bitrate {
                        peer: peer.clone(),
                        bits_per_second,
                    });
                } else if let Some(report) = any
                    .downcast_ref::<ReceiverReport>()
                    .and_then(|report| report.reports.first())
                {
                    // The peer receives this one video stream, so its one
                    // reception report is the stream's.
                    let reception = Reception {
                        highest_sequence: report.last_sequence_number,
                        total_lost: report.total_lost,
                    };
                    tracing::trace!(%peer, ?reception, "receiver report");
                    let _ = self.tx.send(EncoderRequest::Reception {
                        peer: peer.clone(),
                        reception,
                        at: Instant::now(),
                    });
                }
            }
        }
        let _ = self.tx.send(EncoderRequest::PeerGone { peer });
    }
}

impl EncoderRequests {
    fn try_next(&self) -> Option<EncoderRequest> {
        // Empty, or the transport side is gone: either way nothing to apply.
        self.rx.try_recv().ok()
    }
}

// ── Encoding loop (dedicated blocking task) ────────────────────────────────────

/// Runs on a blocking-pool thread: receives frames, encodes them and emits
/// [`EncodedSample`]s until the frame channel closes or the sample channel
/// dies.
pub fn encode_loop(
    mut frame_rx: tokio::sync::mpsc::Receiver<RawFrame>,
    sample_tx: &tokio::sync::mpsc::Sender<EncodedSample>,
    codec: VideoCodec,
    requests: &EncoderRequests,
) {
    let mut pipeline = FramePipeline::new(codec);
    let mut target = BitrateTarget::new(codec.initial_bitrate());
    let mut keyframe = false;
    // Frames are timed by their arrival on the stream's timeline; a sample
    // lasts until the next packet's presentation time, which software AV1
    // releases frames after the frame it encodes.
    let start = Instant::now();
    let mut last_presentation = Duration::ZERO;

    // `blocking_recv` is the documented bridge between tokio mpsc and
    // a `spawn_blocking` task. It blocks the current OS thread (which
    // is the blocking-pool thread we own here) until a frame arrives.
    while let Some(frame) = frame_rx.blocking_recv() {
        while let Some(request) = requests.try_next() {
            let new_bitrate = match request {
                EncoderRequest::Keyframe => {
                    keyframe = true;
                    None
                }
                EncoderRequest::Bitrate {
                    peer,
                    bits_per_second,
                } => target.estimate(peer, bits_per_second),
                EncoderRequest::Reception {
                    peer,
                    reception,
                    at,
                } => target.reception(&peer, reception, at),
                EncoderRequest::PeerGone { peer } => target.peer_gone(&peer),
            };
            if let Some(bitrate) = new_bitrate {
                tracing::debug!(bits_per_second = bitrate.bits_per_second().get(), "bitrate");
                if let Err(error) = pipeline.set_bitrate(bitrate) {
                    tracing::warn!(%error, "setting the encoder bitrate failed");
                }
            }
        }

        let wanted_keyframe = std::mem::take(&mut keyframe);
        let packets = match pipeline.encode(frame, start.elapsed(), wanted_keyframe) {
            Ok(packets) => packets,
            Err(error @ (FrameError::StreamTooLarge { .. } | FrameError::Dimension(_))) => {
                tracing::error!(%error, "video stream cannot be encoded");
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "dropping frame");
                keyframe = wanted_keyframe;
                continue;
            }
        };

        for packet in packets {
            let duration = packet.presentation_time.saturating_sub(last_presentation);
            last_presentation = packet.presentation_time;
            if sample_tx
                .blocking_send(EncodedSample {
                    data: packet.data,
                    duration,
                })
                .is_err()
            {
                return; // WebRTC writer stopped
            }
        }
    }

    tracing::info!("encoder thread stopped");
}
