//! Display capture for a session's virtual desktop: waterkit's
//! `DisplayStream` (`ScreenCaptureKit`) delivers `IOSurface`-backed pixel
//! buffers holding the display's latest BGRA pixels.

use std::num::NonZeroU32;

use futures_lite::StreamExt;
use waterkit_screen::{DisplayStream, DisplayStreamConfig, PixelFormat};

use super::DisplayError;
use crate::video::PixelBuffer;

/// One captured frame: the display's `IOSurface`, held until the encoder is
/// done with it. No pixel is copied.
#[derive(Debug)]
pub struct CapturedFrame {
    pub surface: PixelBuffer,
}

/// Frames as the stream hands them off, newest last.
pub type FrameSender = tokio::sync::mpsc::Sender<CapturedFrame>;

/// The most frames per second a desktop stream captures.
const CAPTURE_FRAME_RATE: NonZeroU32 = NonZeroU32::new(60).expect("60 is non-zero");

/// A running capture on one virtual display. Dropping it stops the
/// stream and its forwarding task.
pub struct DisplayCapture {
    // Created paused: it runs only while the desktop stream has viewers.
    stream: DisplayStream,
    forwarder: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for DisplayCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisplayCapture").finish_non_exhaustive()
    }
}

impl DisplayCapture {
    /// Creates a paused capture of `display_id` at `size`; once
    /// [resumed](Self::resume) it hands each delivered frame to `frames`.
    ///
    /// # Errors
    /// Returns [`DisplayError`] when the stream cannot be created.
    pub async fn new(
        display_id: u32,
        size: (u32, u32),
        frames: FrameSender,
    ) -> Result<Self, DisplayError> {
        let config = DisplayStreamConfig::new(non_zero(size.0)?, non_zero(size.1)?)
            .frame_rate(CAPTURE_FRAME_RATE)
            .pixel_format(PixelFormat::Bgra8);
        let stream = DisplayStream::new(display_id, config)
            .await
            .map_err(|e| DisplayError(format!("create the display stream: {e}")))?;
        let mut delivered = Box::pin(stream.frames());
        let forwarder = tokio::spawn(async move {
            while let Some(buffer) = delivered.next().await {
                let frame = match PixelBuffer::from_io_surface(buffer.io_surface()) {
                    Ok(surface) => CapturedFrame { surface },
                    Err(error) => {
                        tracing::warn!(%error, "dropping a captured frame");
                        continue;
                    }
                };
                match frames.try_send(frame) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        tracing::debug!("computer capture queue saturated, dropping frame");
                    }
                }
            }
        });
        Ok(Self { stream, forwarder })
    }

    /// Starts delivering frames; the first frame follows at once.
    ///
    /// # Errors
    /// Returns [`DisplayError`] when the stream cannot be started.
    pub async fn resume(&mut self) -> Result<(), DisplayError> {
        self.stream
            .resume()
            .await
            .map_err(|e| DisplayError(format!("start the display stream: {e}")))
    }

    /// Stops delivering frames; the display is no longer captured until
    /// [`Self::resume`].
    ///
    /// # Errors
    /// Returns [`DisplayError`] when the stream cannot be stopped.
    pub async fn pause(&mut self) -> Result<(), DisplayError> {
        self.stream
            .pause()
            .await
            .map_err(|e| DisplayError(format!("stop the display stream: {e}")))
    }
}

impl Drop for DisplayCapture {
    fn drop(&mut self) {
        self.forwarder.abort();
    }
}

fn non_zero(extent: u32) -> Result<NonZeroU32, DisplayError> {
    NonZeroU32::new(extent).ok_or_else(|| DisplayError("the display has no area".to_owned()))
}
