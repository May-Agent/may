//! Display capture for a session's virtual desktop: `CGDisplayStream`
//! delivers `IOSurface`s holding the display's latest BGRA pixels.
//!
//! `CGDisplayStream` is deprecated in favour of `ScreenCaptureKit`, and
//! it is still the lighter mechanism: one C call plus a frame block,
//! against five `ObjC` classes, delegates and completion handlers. The
//! captured display is the daemon's own virtual display, so the
//! `ScreenRecording` permission path `SCKit` forces does not apply here.

use std::ffi::c_void;

use block2::RcBlock;

use super::DisplayError;

/// One captured frame: tightly packed BGRA8 pixels.
pub struct CapturedFrame {
    pub bgra: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub bytes_per_row: usize,
}

/// Frames as the stream hands them off, newest last.
pub type FrameSender = tokio::sync::mpsc::Sender<CapturedFrame>;

// CGDisplayStream + IOSurface, declared directly: both are plain C
// APIs and no maintained crate binds the stream surface without
// dragging in the full framework.
type CGDisplayStreamRef = *mut c_void;
type IOSurfaceRef = *mut c_void;
type DispatchQueue = *mut c_void;

const K_DISPLAY_STREAM_BLANK_SCREEN: i64 = 0;
const K_DISPLAY_STREAM_FRAME_STATUS_COMPLETE: i64 = 1;
const K_CG_BASE_BYTE_ORDER_32_LITTLE: u32 = 0x4247_5241; // 'BGRA'
const K_IO_SURFACE_READ_ONLY: u32 = 1;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayStreamCreateWithDispatchQueue(
        display_id: u32,
        queue: DispatchQueue,
        output_width: usize,
        output_height: usize,
        pixel_format: u32,
        properties: *const c_void,
        handler: *mut c_void,
    ) -> CGDisplayStreamRef;
    fn CGDisplayStreamStart(stream: CGDisplayStreamRef) -> i32;
    fn CGDisplayStreamStop(stream: CGDisplayStreamRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const i8, attr: *const c_void) -> DispatchQueue;
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceLock(surface: IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(surface: IOSurfaceRef) -> *mut u8;
    fn IOSurfaceGetBytesPerRow(surface: IOSurfaceRef) -> usize;
    fn IOSurfaceGetWidth(surface: IOSurfaceRef) -> usize;
    fn IOSurfaceGetHeight(surface: IOSurfaceRef) -> usize;
}

/// A running capture on one virtual display. Dropping it stops the
/// stream and frees the display-stream object.
pub struct DisplayCapture {
    stream: CGDisplayStreamRef,
    queue: DispatchQueue,
    // The block must outlive the stream: it fires on `queue` until
    // `CGDisplayStreamStop` returns.
    _handler: RcBlock<dyn Fn(i64, u64, IOSurfaceRef, *mut c_void)>,
}

impl std::fmt::Debug for DisplayCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisplayCapture").finish_non_exhaustive()
    }
}

// The stream delivers frames on its own dispatch queue; the handle is
// only used to start/stop and carries no thread affinity of its own.
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for DisplayCapture {}

impl DisplayCapture {
    /// Starts capturing `display_id` at `size`, handing each delivered
    /// frame to `frames`.
    ///
    /// # Errors
    /// Returns [`DisplayError`] when the stream cannot be created or
    /// started.
    pub fn start(
        display_id: u32,
        size: (u32, u32),
        frames: FrameSender,
    ) -> Result<Self, DisplayError> {
        let (width, height) = (size.0 as usize, size.1 as usize);
        let handler = RcBlock::new(
            move |status: i64, _display_time: u64, surface: IOSurfaceRef, _update: *mut c_void| {
                if status != K_DISPLAY_STREAM_FRAME_STATUS_COMPLETE || surface.is_null() {
                    return;
                }
                let frame = copy_surface(surface);
                match frames.try_send(frame) {
                    Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        tracing::debug!("computer capture queue saturated, dropping frame");
                    }
                }
            },
        );
        // SAFETY: the stream is created on a queue we own and stopped
        // before the handler block is released.
        unsafe {
            let label = c"may.computer.capture".as_ptr();
            let queue = dispatch_queue_create(label, std::ptr::null());
            let stream = CGDisplayStreamCreateWithDispatchQueue(
                display_id,
                queue,
                width,
                height,
                K_CG_BASE_BYTE_ORDER_32_LITTLE,
                std::ptr::null(),
                RcBlock::as_ptr(&handler).cast::<c_void>(),
            );
            if stream.is_null() {
                return Err(DisplayError("CGDisplayStream refused the display".into()));
            }
            if CGDisplayStreamStart(stream) != 0 {
                CFRelease(stream);
                return Err(DisplayError("CGDisplayStream failed to start".into()));
            }
            Ok(Self {
                stream,
                queue,
                _handler: handler,
            })
        }
    }
}

impl Drop for DisplayCapture {
    fn drop(&mut self) {
        unsafe {
            CGDisplayStreamStop(self.stream);
            CFRelease(self.stream);
            CFRelease(self.queue);
        }
    }
}

/// Copies a locked `IOSurface`'s BGRA pixels out while the frame is
/// held — the surface is recycled underneath us otherwise.
fn copy_surface(surface: IOSurfaceRef) -> CapturedFrame {
    unsafe {
        let mut seed = 0u32;
        if IOSurfaceLock(surface, K_IO_SURFACE_READ_ONLY, &raw mut seed) != 0 {
            return CapturedFrame {
                bgra: Vec::new(),
                width: 0,
                height: 0,
                bytes_per_row: 0,
            };
        }
        let base = IOSurfaceGetBaseAddress(surface);
        let row = IOSurfaceGetBytesPerRow(surface);
        let (width, height) = (IOSurfaceGetWidth(surface), IOSurfaceGetHeight(surface));
        let mut bgra = Vec::with_capacity(row.saturating_mul(height));
        if !base.is_null() {
            bgra.extend_from_slice(std::slice::from_raw_parts(base, row.saturating_mul(height)));
        }
        IOSurfaceUnlock(surface, K_IO_SURFACE_READ_ONLY, &raw mut seed);
        CapturedFrame {
            bgra,
            width,
            height,
            bytes_per_row: row,
        }
    }
}

// Referenced so the constant documents itself rather than the magic
// `1` at the call site being the only record.
#[allow(dead_code)]
const _: i64 = K_DISPLAY_STREAM_BLANK_SCREEN;
