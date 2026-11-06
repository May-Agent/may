//! One-shot stills of a session desktop for the agent: the first frame of a
//! short-lived `DisplayStream`, which composites the display's current
//! framebuffer on resume.

use std::num::NonZeroU32;
use std::time::Duration;

use futures_lite::StreamExt;
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVReturnSuccess,
};
use waterkit_screen::{DisplayStream, DisplayStreamConfig, PixelBuffer, PixelFormat};

use super::DisplayError;

/// How long the display may take to deliver its first frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// A PNG of the display's current contents at `size` pixels.
///
/// # Errors
/// Returns [`DisplayError`] when the display cannot be captured or the
/// pixels cannot be encoded.
pub async fn screenshot_png(display_id: u32, size: (u32, u32)) -> Result<Vec<u8>, DisplayError> {
    let extent = |value: u32| {
        NonZeroU32::new(value).ok_or_else(|| DisplayError("the display has no area".to_owned()))
    };
    let config =
        DisplayStreamConfig::new(extent(size.0)?, extent(size.1)?).pixel_format(PixelFormat::Bgra8);
    let mut stream = DisplayStream::new(display_id, config)
        .await
        .map_err(|e| DisplayError(format!("create the display stream: {e}")))?;
    let mut frames = std::pin::pin!(stream.frames());
    stream
        .resume()
        .await
        .map_err(|e| DisplayError(format!("start the display stream: {e}")))?;
    let frame = tokio::time::timeout(FIRST_FRAME_TIMEOUT, frames.next())
        .await
        .map_err(|_| DisplayError("the display produced no frame".to_owned()))?
        .ok_or_else(|| DisplayError("the display stream ended before a frame".to_owned()))?;
    // Capture ends with the stream; the frame is already ours.
    drop(stream);
    tokio::task::spawn_blocking(move || encode_png(&frame))
        .await
        .map_err(|e| DisplayError(format!("encode the screenshot: {e}")))?
}

/// Encodes a BGRA frame as PNG, reading its pixels under a read lock.
fn encode_png(frame: &PixelBuffer) -> Result<Vec<u8>, DisplayError> {
    let buffer = frame.cv_pixel_buffer();
    let (width, height) = (frame.width() as usize, frame.height() as usize);
    // SAFETY: the buffer is live; the lock is released below.
    let status = unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) };
    if status != kCVReturnSuccess {
        return Err(DisplayError(format!("lock the frame: CVReturn {status}")));
    }
    let stride = CVPixelBufferGetBytesPerRow(buffer);
    let base = CVPixelBufferGetBaseAddress(buffer).cast::<u8>();
    // SAFETY: while locked, `base` addresses `stride * height` bytes.
    let raw = unsafe { std::slice::from_raw_parts(base, stride * height) };

    // The frame is BGRA8; reorder to RGB for PNG.
    let mut rgb = image::RgbImage::new(frame.width(), frame.height());
    for (y, row) in raw.chunks_exact(stride).take(height).enumerate() {
        for (x, px) in row.as_chunks::<4>().0.iter().take(width).enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            rgb.put_pixel(x as u32, y as u32, image::Rgb([px[2], px[1], px[0]]));
        }
    }
    // SAFETY: balances the lock above.
    unsafe { CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) };

    let mut png = std::io::Cursor::new(Vec::new());
    rgb.write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| DisplayError(format!("encode the screenshot: {e}")))?;
    Ok(png.into_inner())
}
