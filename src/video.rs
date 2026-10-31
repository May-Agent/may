//! The shared screen-content video path every streamed surface uses.
//!
//! Raw frames in, H.264 (Screen Content) samples out for a WebRTC
//! `TrackLocalStaticSample`.
//!
//! Today two sources feed it — the managed browser's CDP screencast
//! JPEGs and a session desktop's `CGDisplayStream` surfaces — so the
//! frame type is [`RawFrame`]: the encoder loop takes whatever decode
//! each source needs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// An encoded video frame ready for WebRTC.
pub struct EncodedSample {
    pub data: Vec<u8>,
    pub duration: Duration,
}

/// A frame on its way to the encoder. Each source supplies the decode
/// it already has: the browser hands JPEG bytes off the wire, a
/// display hands locked `IOSurface` pixels.
pub enum RawFrame {
    /// An encoded JPEG frame (the browser's CDP screencast).
    Jpeg(Vec<u8>),
    /// BGRA pixels copied out of a display surface, with the row stride
    /// the surface carried.
    #[cfg(target_os = "macos")]
    Bgra(BgraFrame),
}

/// A captured display frame in BGRA order, as the display stream hands it over.
#[cfg(target_os = "macos")]
pub struct BgraFrame {
    pub data: Vec<u8>,
    /// Distance in bytes between two consecutive rows, which may exceed
    /// `width * 4` (`IOSurface` aligns rows).
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

#[cfg(target_os = "macos")]
impl BgraFrame {
    /// Drops the alpha byte and reorders to RGB, honouring the surface
    /// stride — the only conversion the display-capture source needs.
    fn to_rgb(&self) -> image::RgbImage {
        let mut rgb = image::RgbImage::new(
            u32::try_from(self.width).unwrap_or(u32::MAX),
            u32::try_from(self.height).unwrap_or(u32::MAX),
        );
        for (y, row) in self
            .data
            .chunks_exact(self.stride)
            .take(self.height)
            .enumerate()
        {
            for (x, px) in row.as_chunks::<4>().0.iter().take(self.width).enumerate() {
                #[allow(clippy::cast_possible_truncation)]
                rgb.put_pixel(x as u32, y as u32, image::Rgb([px[2], px[1], px[0]]));
            }
        }
        rgb
    }
}

/// Capacity of the frame → encoder queue. Tuned so a transient encoder
/// stall (~250 ms at 30 fps) absorbs without dropping frames, while still
/// catching unbounded growth on a stuck consumer.
pub const FRAME_QUEUE_CAPACITY: usize = 8;

// ── H.264 encoding loop (dedicated blocking task) ──────────────────────────────

/// Runs on a blocking-pool thread: receives frames, encodes Screen
/// Content H.264, emits [`EncodedSample`]s until the frame channel
/// closes or the sample channel dies.
pub fn encode_loop(
    mut frame_rx: tokio::sync::mpsc::Receiver<RawFrame>,
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
    while let Some(frame) = frame_rx.blocking_recv() {
        // Decode to RGB by whatever the source handed us.
        let img = match frame {
            RawFrame::Jpeg(jpeg_bytes) => {
                match image::load_from_memory_with_format(&jpeg_bytes, image::ImageFormat::Jpeg) {
                    Ok(img) => img.to_rgb8(),
                    Err(e) => {
                        tracing::warn!(error = %e, "JPEG decode failed");
                        continue;
                    }
                }
            }
            #[cfg(target_os = "macos")]
            RawFrame::Bgra(frame) => frame.to_rgb(),
        };

        let rgb = normalize_rgb_frame_for_h264(img);
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

    tracing::info!("encoder thread stopped");
}

/// H.264 requires even dimensions; pad odd frames by duplicating the
/// edge row/column rather than cropping a visible pixel.
pub fn normalize_rgb_frame_for_h264(rgb: image::RgbImage) -> image::RgbImage {
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
