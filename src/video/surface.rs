//! Frames that stay on the GPU/media-engine side: a `CVPixelBuffer` the
//! encoder takes as it is, wrapping either a captured display `IOSurface`
//! or the platform JPEG decoder's output.

use std::ptr::{self, NonNull};

use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreateWithIOSurface, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_io_surface::IOSurfaceRef;

use super::FrameError;
use super::nv12::{self, Nv12, Packed};

/// A BGRA pixel buffer ready for the encoder.
pub struct PixelBuffer {
    buffer: CFRetained<CVPixelBuffer>,
    /// The display surface this buffer keeps in use, so the capture stream
    /// does not draw into it while it waits for or sits in the encoder.
    held_surface: Option<CFRetained<IOSurfaceRef>>,
}

impl std::fmt::Debug for PixelBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PixelBuffer")
            .field("size", &self.size())
            .finish_non_exhaustive()
    }
}

// SAFETY: a `CVPixelBuffer` is a reference-counted CoreVideo object whose
// retain, release and read-only locking are thread-safe; this wrapper never
// mutates the pixels.
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for PixelBuffer {}

impl PixelBuffer {
    /// Wraps a captured display surface without copying its pixels, and marks
    /// the surface in use until this value drops.
    ///
    /// # Errors
    /// Returns [`FrameError::Surface`] when `CoreVideo` refuses the surface.
    pub fn from_io_surface(surface: &IOSurfaceRef) -> Result<Self, FrameError> {
        let mut raw: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: `raw` is a valid out pointer; CoreVideo retains the surface.
        let status = unsafe {
            CVPixelBufferCreateWithIOSurface(None, surface, None, NonNull::from(&mut raw))
        };
        let buffer = NonNull::new(raw)
            .filter(|_| status == kCVReturnSuccess)
            .ok_or(FrameError::Surface(status))?;
        // SAFETY: a `Create` function returns an owned (+1) reference.
        let buffer = unsafe { CFRetained::from_raw(buffer) };
        surface.increment_use_count();
        Ok(Self {
            buffer,
            // SAFETY: `surface` is a live reference.
            held_surface: Some(unsafe { CFRetained::retain(NonNull::from(surface)) }),
        })
    }

    /// Wraps a buffer the platform JPEG decoder produced.
    pub const fn from_decoded(buffer: CFRetained<CVPixelBuffer>) -> Self {
        Self {
            buffer,
            held_surface: None,
        }
    }

    /// The buffer's `(width, height)` in pixels.
    pub fn size(&self) -> (u32, u32) {
        let width = CVPixelBufferGetWidth(&self.buffer);
        let height = CVPixelBufferGetHeight(&self.buffer);
        (
            u32::try_from(width).expect("a pixel buffer width fits u32"),
            u32::try_from(height).expect("a pixel buffer height fits u32"),
        )
    }

    /// The buffer itself, for the encoder.
    pub fn buffer(&self) -> &CVPixelBuffer {
        &self.buffer
    }

    /// Reads the pixels out as NV12, cropping an odd edge. This is the only
    /// place a surface is touched by the CPU.
    ///
    /// # Errors
    /// Returns [`FrameError::Surface`] when the buffer cannot be locked, and
    /// [`FrameError::PixelFormat`] when it is not BGRA.
    pub fn to_nv12(&self) -> Result<Nv12, FrameError> {
        let format = CVPixelBufferGetPixelFormatType(&self.buffer);
        if format != kCVPixelFormatType_32BGRA {
            return Err(FrameError::PixelFormat(format));
        }
        let (width, height) = self.size();
        // SAFETY: the buffer is live; the lock is released below.
        let status =
            unsafe { CVPixelBufferLockBaseAddress(&self.buffer, CVPixelBufferLockFlags::ReadOnly) };
        if status != kCVReturnSuccess {
            return Err(FrameError::Surface(status));
        }
        let stride = CVPixelBufferGetBytesPerRow(&self.buffer);
        let base = CVPixelBufferGetBaseAddress(&self.buffer).cast::<u8>();
        // SAFETY: while locked, `base` addresses `stride * height` bytes.
        let pixels = unsafe { std::slice::from_raw_parts(base, stride * height as usize) };
        let converted = nv12::convert(Packed::Bgra, pixels, stride, width, height);
        // SAFETY: balances the lock above.
        unsafe { CVPixelBufferUnlockBaseAddress(&self.buffer, CVPixelBufferLockFlags::ReadOnly) };
        Ok(converted?)
    }
}

impl Drop for PixelBuffer {
    fn drop(&mut self) {
        if let Some(surface) = &self.held_surface {
            surface.decrement_use_count();
        }
    }
}
