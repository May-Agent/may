//! The real encoder pipeline on the host's hardware: synthetic frames in,
//! Annex B / OBU packets out. Nothing here opens a window or a browser.

use std::io::Cursor;
use std::time::{Duration, Instant};

use mediax_codec::{EncodedPacket, EncoderCodec};

use super::pipeline::FramePipeline;
use super::{BitrateTarget, FrameError, OfferCodecError, RawFrame, StreamFormat, VideoCodec};
use crate::config::StreamCodec;
use mediax_codec::{CodecError, EncoderConfig};

#[allow(clippy::unnecessary_wraps)]
fn has_hardware(_: EncoderConfig) -> Result<(), CodecError> {
    Ok(())
}

fn no_hardware(_: EncoderConfig) -> Result<(), CodecError> {
    Err(CodecError::Unsupported("no encoder".into()))
}

fn never_probed(_: EncoderConfig) -> Result<(), CodecError> {
    panic!("the AV1 preference must not probe hardware");
}

fn is_h264(codec: VideoCodec) -> bool {
    matches!(codec.codec, EncoderCodec::H264(_))
}

#[test]
fn auto_takes_hardware_h264_and_falls_back_to_av1() {
    let f = format(640, 360);
    let with = VideoCodec::select_with(f, StreamCodec::Auto, has_hardware).expect("select");
    assert!(is_h264(with));
    let without = VideoCodec::select_with(f, StreamCodec::Auto, no_hardware).expect("select");
    assert_eq!(without.name(), "AV1");
}

#[test]
fn h264_never_falls_back_to_av1() {
    let f = format(640, 360);
    let with = VideoCodec::select_with(f, StreamCodec::H264, has_hardware).expect("select");
    assert!(is_h264(with));
    let error = VideoCodec::select_with(f, StreamCodec::H264, no_hardware).expect_err("no AV1");
    assert!(matches!(error, FrameError::H264Unavailable(_)));
    assert!(error.to_string().contains("H.264"));
}

#[test]
fn av1_is_software_even_with_hardware_h264() {
    let codec =
        VideoCodec::select_with(format(640, 360), StreamCodec::Av1, never_probed).expect("select");
    assert_eq!(codec.name(), "AV1");
}

#[test]
fn other_encoder_failures_are_not_swallowed() {
    let failing = |_: EncoderConfig| Err(CodecError::InitializationFailed("bad".into()));
    for preference in [StreamCodec::Auto, StreamCodec::H264] {
        let error = VideoCodec::select_with(format(640, 360), preference, failing)
            .expect_err("a real failure");
        assert!(matches!(error, FrameError::Codec(_)));
    }
}

const SDP_HEAD: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n";

fn offer(media: &str) -> String {
    format!("{SDP_HEAD}{media}")
}

#[test]
fn an_offer_without_the_codec_is_refused_naming_it() {
    let f = format(640, 360);
    let chrome = offer(
        "m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\nc=IN IP4 0.0.0.0\r\na=rtpmap:96 VP8/90000\r\na=rtpmap:97 H264/90000\r\n",
    );
    let h264 = VideoCodec::select_with(f, StreamCodec::H264, has_hardware).expect("select");
    h264.check_offer(&chrome).expect("offered");
    let av1 = av1(f);
    let error = av1.check_offer(&chrome).expect_err("AV1 not offered");
    assert!(matches!(error, OfferCodecError::Missing { codec: "AV1" }));
    assert!(error.to_string().contains("AV1"));
    // A payload type mapped outside a video section does not count.
    let data_only =
        offer("m=application 9 UDP/DTLS/SCTP 45\r\nc=IN IP4 0.0.0.0\r\na=rtpmap:45 AV1/90000\r\n");
    assert!(matches!(
        av1.check_offer(&data_only),
        Err(OfferCodecError::Missing { .. })
    ));
    av1.check_offer(&offer(
        "m=video 9 UDP/TLS/RTP/SAVPF 45\r\nc=IN IP4 0.0.0.0\r\na=rtpmap:45 av1/90000\r\n",
    ))
    .expect("case-insensitive");
}

#[test]
fn an_offer_that_is_not_sdp_is_reported_as_invalid() {
    let av1 = av1(format(640, 360));
    assert!(matches!(
        av1.check_offer("not an offer"),
        Err(OfferCodecError::Invalid(_))
    ));
}

fn format(width: u32, height: u32) -> StreamFormat {
    StreamFormat {
        max_width: width,
        max_height: height,
        initial_bitrate_kbps: 2_000,
    }
}

fn av1(format: StreamFormat) -> VideoCodec {
    VideoCodec {
        codec: EncoderCodec::Av1,
        format,
    }
}

/// A moving gradient with a few hard edges, like a UI.
#[allow(clippy::cast_possible_truncation)]
fn jpeg(width: u32, height: u32, step: u32) -> RawFrame {
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        let bar = (x + step * 4) % 160 < 80;
        image::Rgb([
            ((x + step) % 256) as u8,
            (y % 256) as u8,
            if bar { 230 } else { 20 },
        ])
    });
    let mut bytes = Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::Jpeg)
        .expect("encode the synthetic JPEG");
    RawFrame::Jpeg(bytes.into_inner())
}

/// NAL unit types of an Annex B access unit.
#[cfg(target_os = "macos")]
fn nal_types(data: &[u8]) -> Vec<u8> {
    data.windows(4)
        .enumerate()
        .filter(|(_, w)| w[..3] == [0, 0, 1])
        .map(|(_, w)| w[3] & 0x1f)
        .collect()
}

#[cfg(target_os = "macos")]
fn is_idr(packet: &EncodedPacket) -> bool {
    packet.keyframe && nal_types(&packet.data).contains(&5)
}

/// A pipeline fed one frame per 30 fps tick.
struct Feed {
    pipeline: FramePipeline,
    frames: u32,
}

impl Feed {
    const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 30);

    fn new(codec: VideoCodec) -> Self {
        Self {
            pipeline: FramePipeline::new(codec),
            frames: 0,
        }
    }

    /// Encodes the next frame and times the call.
    fn timed(&mut self, frame: RawFrame, keyframe: bool) -> (Vec<EncodedPacket>, Duration) {
        let presentation_time = Self::FRAME * self.frames;
        self.frames += 1;
        let started = Instant::now();
        let packets = self
            .pipeline
            .encode(frame, presentation_time, keyframe)
            .expect("encode");
        (packets, started.elapsed())
    }
}

fn report(label: &str, times: &[Duration]) {
    let total: Duration = times.iter().sum();
    let max = times.iter().max().expect("frames");
    eprintln!(
        "{label}: {} frames, mean {:.2} ms, max {:.2} ms",
        times.len(),
        total.as_secs_f64() * 1000.0 / times.len() as f64,
        max.as_secs_f64() * 1000.0
    );
}

#[test]
fn the_sdp_names_the_level_the_encoder_runs_at() {
    let codec = VideoCodec::select(format(640, 360), StreamCodec::Auto).expect("select");
    let capability = codec.capability();
    match codec.codec {
        EncoderCodec::H264(_) => {
            assert_eq!(capability.mime_type, "video/H264");
            assert_eq!(
                capability.sdp_fmtp_line,
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01e"
            );
        }
        EncoderCodec::Av1 => assert_eq!(capability.mime_type, "video/AV1"),
        EncoderCodec::H265(_) => panic!("H.265 is never selected"),
    }
    let (_api, _track) = codec.webrtc("video", "test").expect("webrtc stack");

    let av1 = av1(format(640, 360)).capability();
    assert_eq!(av1.mime_type, "video/AV1");
}

#[test]
fn bitrate_follows_the_lowest_peer_estimate() {
    use mediax_codec::Bitrate;
    use std::num::NonZeroU32;
    let start = Bitrate::new(NonZeroU32::new(2_000_000).unwrap());
    let mut target = BitrateTarget::new(start);
    // Within the step: not applied.
    assert!(target.report("a".into(), 2_100_000).is_none());
    let lowered = target.report("b".into(), 800_000).expect("moved");
    assert_eq!(lowered.bits_per_second().get(), 800_000);
    // The lower peer leaves: the other's estimate takes over.
    let raised = target.peer_gone("b").expect("moved");
    assert_eq!(raised.bits_per_second().get(), 2_100_000);
}

#[cfg(target_os = "macos")]
#[test]
fn jpegs_become_h264_idrs_on_request_and_size_change() {
    let codec = VideoCodec::select(format(640, 360), StreamCodec::Auto).expect("select");
    assert!(
        matches!(codec.codec, EncoderCodec::H264(_)),
        "this test needs a hardware H.264 encoder"
    );
    let mut feed = Feed::new(codec);
    let mut times = Vec::new();

    let (packets, elapsed) = feed.timed(jpeg(640, 360, 0), false);
    times.push(elapsed);
    assert_eq!(packets.len(), 1);
    assert!(packets[0].data.starts_with(&[0, 0, 0, 1]), "Annex B");
    assert!(is_idr(&packets[0]), "the first frame is an IDR");
    let first_types = nal_types(&packets[0].data);
    assert!(
        first_types.contains(&7) && first_types.contains(&8),
        "SPS and PPS lead the IDR"
    );

    for step in 1..30 {
        let (packets, elapsed) = feed.timed(jpeg(640, 360, step), false);
        times.push(elapsed);
        assert_eq!(packets.len(), 1);
        assert!(!packets[0].keyframe, "frame {step} is a delta frame");
    }

    let (packets, elapsed) = feed.timed(jpeg(640, 360, 30), true);
    times.push(elapsed);
    assert!(is_idr(&packets[0]), "a requested keyframe is an IDR");

    // A viewport resize: new even size, new encoder, IDR without a request.
    let (packets, elapsed) = feed.timed(jpeg(480, 270, 31), false);
    times.push(elapsed);
    assert!(is_idr(&packets[0]), "a size change starts with an IDR");
    let (packets, _) = feed.timed(jpeg(480, 270, 32), false);
    assert!(!packets[0].keyframe);

    // An odd size is cropped to even and encoded through NV12.
    let (packets, elapsed) = feed.timed(jpeg(521, 301, 33), false);
    times.push(elapsed);
    assert!(
        is_idr(&packets[0]),
        "the odd frame's even crop is a new size"
    );
    let (packets, _) = feed.timed(jpeg(521, 301, 34), false);
    assert!(!packets[0].keyframe);

    report("jpeg -> hardware H.264", &times);
}

#[cfg(target_os = "macos")]
mod surfaces {
    use std::ptr::{self, NonNull};

    use objc2_core_foundation::{CFDictionary, CFRetained, CFString};
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
        CVPixelBufferGetBytesPerRow, CVPixelBufferGetIOSurface, CVPixelBufferLockBaseAddress,
        CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
        kCVPixelBufferIOSurfacePropertiesKey, kCVPixelFormatType_32BGRA, kCVReturnSuccess,
    };

    use super::*;
    use crate::video::PixelBuffer;

    /// An `IOSurface`-backed BGRA buffer, wrapped the way a captured display
    /// surface is.
    #[allow(clippy::cast_possible_truncation)]
    fn surface(width: usize, height: usize, step: usize) -> RawFrame {
        let no_properties = CFDictionary::<CFString, CFString>::empty();
        let attributes = CFDictionary::<CFString, CFDictionary<CFString, CFString>>::from_slices(
            &[unsafe { kCVPixelBufferIOSurfacePropertiesKey }],
            &[&*no_properties],
        );
        let mut raw: *mut CVPixelBuffer = ptr::null_mut();
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width,
                height,
                kCVPixelFormatType_32BGRA,
                Some(attributes.as_opaque()),
                NonNull::from(&mut raw),
            )
        };
        assert_eq!(status, kCVReturnSuccess);
        let buffer = unsafe { CFRetained::from_raw(NonNull::new(raw).expect("buffer")) };
        unsafe {
            CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
            let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRow(&buffer);
            for y in 0..height {
                let row = std::slice::from_raw_parts_mut(base.add(y * stride), width * 4);
                for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let bar = (x + step * 4) % 160 < 80;
                    *px = [
                        if bar { 230 } else { 20 },
                        (y % 256) as u8,
                        ((x + step) % 256) as u8,
                        255,
                    ];
                }
            }
            CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
        }
        let io_surface = CVPixelBufferGetIOSurface(Some(&buffer)).expect("IOSurface-backed");
        RawFrame::Surface(PixelBuffer::from_io_surface(&io_surface).expect("wrap the surface"))
    }

    #[test]
    fn surfaces_become_h264_idrs_without_a_cpu_copy() {
        let codec = VideoCodec::select(format(1280, 720), StreamCodec::Auto).expect("select");
        assert!(matches!(codec.codec, EncoderCodec::H264(_)));
        assert!(codec.takes_surfaces());
        let mut feed = Feed::new(codec);
        let mut times = Vec::new();

        let (packets, elapsed) = feed.timed(surface(1280, 720, 0), false);
        times.push(elapsed);
        assert!(is_idr(&packets[0]));
        assert!(packets[0].data.starts_with(&[0, 0, 0, 1]));

        for step in 1..30 {
            let (packets, elapsed) = feed.timed(surface(1280, 720, step), false);
            times.push(elapsed);
            assert!(!packets[0].keyframe);
        }
        let (packets, elapsed) = feed.timed(surface(1280, 720, 30), true);
        times.push(elapsed);
        assert!(is_idr(&packets[0]));

        let (packets, elapsed) = feed.timed(surface(1024, 576, 31), false);
        times.push(elapsed);
        assert!(is_idr(&packets[0]), "a size change starts with an IDR");

        // An odd-sized surface takes the crop path.
        let (packets, elapsed) = feed.timed(surface(1023, 575, 32), false);
        times.push(elapsed);
        assert!(is_idr(&packets[0]));

        report("IOSurface -> hardware H.264 (1280x720)", &times);
    }

    #[test]
    fn surfaces_reach_av1_through_nv12() {
        let mut feed = Feed::new(av1(format(640, 360)));
        let mut keyframes = 0;
        let mut packets_seen = 0;
        for step in 0..24 {
            let (packets, _) = feed.timed(surface(640, 360, step), step == 12);
            packets_seen += packets.len();
            keyframes += packets.iter().filter(|p| p.keyframe).count();
        }
        assert!(packets_seen > 0);
        assert!(
            keyframes >= 2,
            "the start and the request each yield a key frame"
        );
    }
}

#[test]
fn jpegs_reach_av1_through_nv12() {
    let mut feed = Feed::new(av1(format(640, 360)));
    let mut times = Vec::new();
    let mut keyframes = 0;
    let mut packets_seen = 0;
    for step in 0..24 {
        let (packets, elapsed) = feed.timed(jpeg(640, 360, step), step == 12);
        times.push(elapsed);
        packets_seen += packets.len();
        keyframes += packets.iter().filter(|p| p.keyframe).count();
    }
    assert!(packets_seen > 0);
    assert!(
        keyframes >= 2,
        "the start and the request each yield a key frame"
    );
    report("jpeg -> software AV1 (640x360)", &times);
}
