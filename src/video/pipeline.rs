//! From a [`RawFrame`] to encoded packets: decode or wrap the frame, keep an
//! encoder of the frame's size, and force the IDR a new encoder needs.

use std::time::Duration;

use mediax_codec::{Bitrate, EncodedPacket, Encoder, EvenDimension, FrameOptions};

use super::nv12::{self, Nv12, Packed};
use super::{FrameError, RawFrame, VideoCodec};

/// What the encoder takes for one frame.
enum Prepared {
    #[cfg(target_os = "macos")]
    Surface(super::PixelBuffer),
    Nv12(Nv12),
}

impl Prepared {
    #[cfg_attr(
        not(target_os = "macos"),
        expect(
            clippy::missing_const_for_fn,
            reason = "only the macOS surface arm calls a non-const accessor"
        )
    )]
    fn size(&self) -> (u32, u32) {
        match self {
            #[cfg(target_os = "macos")]
            Self::Surface(buffer) => buffer.size(),
            Self::Nv12(frame) => (frame.width, frame.height),
        }
    }
}

struct Live {
    encoder: Encoder,
    width: u32,
    height: u32,
}

/// One stream's encoder, recreated whenever the frame size changes.
pub struct FramePipeline {
    codec: VideoCodec,
    bitrate: Bitrate,
    live: Option<Live>,
}

impl std::fmt::Debug for FramePipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FramePipeline")
            .field("codec", &self.codec)
            .finish_non_exhaustive()
    }
}

impl FramePipeline {
    pub fn new(codec: VideoCodec) -> Self {
        Self {
            bitrate: codec.initial_bitrate(),
            codec,
            live: None,
        }
    }

    /// Retargets the running encoder, and the one a size change creates next.
    ///
    /// # Errors
    /// Returns the encoder's error.
    pub fn set_bitrate(&mut self, bitrate: Bitrate) -> Result<(), FrameError> {
        self.bitrate = bitrate;
        if let Some(live) = &mut self.live {
            live.encoder.set_bitrate(bitrate)?;
        }
        Ok(())
    }

    /// Encodes one frame shown at `presentation_time` on the stream's
    /// timeline. `keyframe` makes it an IDR; so does the first frame at a new
    /// size, which is encoded by a new encoder.
    ///
    /// An encoder may hold a frame back (software AV1 releases a frame's
    /// packet a few submissions later), so the result can be empty.
    ///
    /// # Errors
    /// Returns [`FrameError`] when the frame cannot be decoded or encoded, or
    /// exceeds the size the stream was sized for.
    pub fn encode(
        &mut self,
        frame: RawFrame,
        presentation_time: Duration,
        keyframe: bool,
    ) -> Result<Vec<EncodedPacket>, FrameError> {
        let prepared = self.prepare(frame)?;
        let (width, height) = prepared.size();
        let resized = self
            .live
            .as_ref()
            .is_none_or(|live| (live.width, live.height) != (width, height));
        if resized {
            self.live = Some(self.open(width, height)?);
        }
        let keyframe = keyframe || resized;
        let live = self.live.as_mut().expect("opened above");
        let options = FrameOptions { keyframe };
        let stream = match &prepared {
            #[cfg(target_os = "macos")]
            Prepared::Surface(buffer) => {
                live.encoder
                    .encode_pixel_buffer(buffer.buffer(), presentation_time, options)
            }
            Prepared::Nv12(frame) => {
                live.encoder
                    .encode_nv12(&frame.data, presentation_time, options)
            }
        };
        Ok(stream.collect::<Result<Vec<_>, _>>()?)
    }

    fn open(&self, width: u32, height: u32) -> Result<Live, FrameError> {
        let (max_width, max_height) = self.codec.ceiling()?;
        if width > max_width.get() || height > max_height.get() {
            return Err(FrameError::FrameTooLarge {
                width,
                height,
                max_width: max_width.get(),
                max_height: max_height.get(),
            });
        }
        let even = |pixels| EvenDimension::new(pixels).map_err(|_| FrameError::Dimension(pixels));
        let config = self
            .codec
            .encoder_config(even(width)?, even(height)?, self.bitrate);
        tracing::info!(width, height, "starting the encoder at the frame size");
        Ok(Live {
            encoder: Encoder::new(config)?,
            width,
            height,
        })
    }

    fn prepare(&self, frame: RawFrame) -> Result<Prepared, FrameError> {
        match frame {
            #[cfg(target_os = "macos")]
            RawFrame::Surface(buffer) => self.prepare_surface(buffer),
            RawFrame::Jpeg(bytes) => self.prepare_jpeg(&bytes),
        }
    }

    /// A surface goes to the encoder as it is when the encoder takes surfaces
    /// and the size is even; otherwise it is read out and cropped to even.
    #[cfg(target_os = "macos")]
    fn prepare_surface(&self, buffer: super::PixelBuffer) -> Result<Prepared, FrameError> {
        let (width, height) = buffer.size();
        if self.codec.takes_surfaces() && width % 2 == 0 && height % 2 == 0 {
            Ok(Prepared::Surface(buffer))
        } else {
            Ok(Prepared::Nv12(buffer.to_nv12()?))
        }
    }

    #[cfg(target_os = "macos")]
    fn prepare_jpeg(&self, bytes: &[u8]) -> Result<Prepared, FrameError> {
        use mediax_codec::CodecError;
        match mediax_codec::image::decode_jpeg_pixel_buffer(bytes) {
            Ok(buffer) => self.prepare_surface(super::PixelBuffer::from_decoded(buffer)),
            // A host without the platform JPEG decoder decodes on the CPU.
            Err(CodecError::Unsupported(reason)) => {
                tracing::debug!(%reason, "no platform JPEG decoder; decoding on the CPU");
                Self::decode_jpeg_on_cpu(bytes)
            }
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[allow(clippy::unused_self)]
    fn prepare_jpeg(&self, bytes: &[u8]) -> Result<Prepared, FrameError> {
        Self::decode_jpeg_on_cpu(bytes)
    }

    fn decode_jpeg_on_cpu(bytes: &[u8]) -> Result<Prepared, FrameError> {
        let rgb = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)?.to_rgb8();
        let (width, height) = rgb.dimensions();
        Ok(Prepared::Nv12(nv12::convert(
            Packed::Rgb,
            rgb.as_raw(),
            width as usize * 3,
            width,
            height,
        )?))
    }
}
