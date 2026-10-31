//! The macOS virtual-display backend: a private-API `CGVirtualDisplay`
//! per session. The display exists nowhere on the user's monitors —
//! it is a full desktop the agent owns — and dies when its `SkylightDisplay`
//! is dropped, because releasing the `CGVirtualDisplay` tears the
//! display down.

use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSSize, NSString};

use super::{DisplayError, SessionDisplay};

fn class(name: &std::ffi::CStr) -> Result<&'static AnyClass, DisplayError> {
    AnyClass::get(name).ok_or_else(|| {
        DisplayError(format!(
            "{} is unavailable on this macOS",
            name.to_string_lossy()
        ))
    })
}

/// One session's `SkyLight` virtual display.
pub struct SkylightDisplay {
    /// The `CGVirtualDisplay`, held only for ownership: its drop
    /// removes the display.
    _display: Retained<AnyObject>,
    display_id: u32,
    size: (u32, u32),
}

// The display object is immutable after `applySettings`; Core Graphics
// display objects are safe to read from any thread.
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for SkylightDisplay {}
unsafe impl Sync for SkylightDisplay {}

impl SkylightDisplay {
    /// Creates a virtual display `width`x`height` points at `refresh` Hz
    /// with 2x backing scale, and applies it. `name` shows in display
    /// lists.
    ///
    /// # Errors
    /// Returns [`DisplayError`] when the private classes are missing or
    /// `SkyLight` refuses the settings.
    pub fn create(name: &str, size: (u32, u32), refresh: f64) -> Result<Self, DisplayError> {
        let (width, height) = size;
        unsafe {
            let descriptor: Retained<AnyObject> =
                msg_send![class(c"CGVirtualDisplayDescriptor")?, new];
            let name = NSString::from_str(name);
            let _: () = msg_send![&descriptor, setName: &*name];
            let _: () = msg_send![&descriptor, setMaxPixelsWide: width * 2];
            let _: () = msg_send![&descriptor, setMaxPixelsHigh: height * 2];
            // ~110 dpi in millimetres, matching a typical desktop panel.
            let mm = NSSize::new(
                f64::from(width) / 110.0 * 25.4,
                f64::from(height) / 110.0 * 25.4,
            );
            let _: () = msg_send![&descriptor, setSizeInMillimeters: mm];
            let _: () = msg_send![&descriptor, setVendorID: 0x610u32];
            let _: () = msg_send![&descriptor, setProductID: 0xa08u32];
            let serial = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(1, |d| d.subsec_nanos())
                % 0xffff_ffff)
                | 1;
            let _: () = msg_send![&descriptor, setSerialNum: serial];

            // `initWithDescriptor:` consumes the allocated receiver under
            // the +1 convention, so the init result is the sole owner.
            let alloced: Allocated<AnyObject> = msg_send![class(c"CGVirtualDisplay")?, alloc];
            let display: Retained<AnyObject> = msg_send![alloced, initWithDescriptor: &*descriptor];

            let settings: Retained<AnyObject> = msg_send![class(c"CGVirtualDisplaySettings")?, new];
            let _: () = msg_send![&settings, setHiDPI: 2u32];
            let alloced: Allocated<AnyObject> = msg_send![class(c"CGVirtualDisplayMode")?, alloc];
            let mode: Retained<AnyObject> = msg_send![
                alloced,
                initWithWidth: width * 2,
                height: height * 2,
                refreshRate: refresh
            ];
            let modes: Retained<AnyObject> = msg_send![class(c"NSMutableArray")?, new];
            let _: () = msg_send![&modes, addObject: &*mode];
            let _: () = msg_send![&settings, setModes: &*modes];

            let applied: bool = msg_send![&display, applySettings: &*settings];
            if !applied {
                return Err(DisplayError("SkyLight refused the display settings".into()));
            }
            let display_id: u32 = msg_send![&display, displayID];
            if display_id == 0 {
                return Err(DisplayError("SkyLight assigned no display id".into()));
            }
            Ok(Self {
                _display: display,
                display_id,
                size,
            })
        }
    }
}

impl std::fmt::Debug for SkylightDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkylightDisplay")
            .field("display_id", &self.display_id)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl SessionDisplay for SkylightDisplay {
    fn display_id(&self) -> u32 {
        self.display_id
    }

    fn size(&self) -> (u32, u32) {
        self.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session's display exists while held and is gone after release.
    /// `SkyLight` reports it through the real `displayID`.
    #[test]
    fn a_display_lives_for_its_holder() {
        let display = SkylightDisplay::create("May test", (800, 600), 60.0)
            .expect("a virtual display should always be creatable");
        assert_ne!(display.display_id(), 0);
        assert_eq!(display.size(), (800, 600));
    }
}
