//! macOS privacy grants the agent needs: Screen Recording (capture and
//! stills) and Accessibility (event injection). Both are TCC entries
//! evaluated per process at launch — which is exactly why this whole
//! stack lives in a respawnable helper: the daemon never has to exit
//! for a grant to take effect.

use std::ffi::c_void;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithBytes(
        alloc: *const c_void,
        bytes: *const u8,
        len: isize,
        encoding: u32,
        ext: bool,
    ) -> *const c_void;
    fn CFNumberCreate(alloc: *const c_void, ty: i32, value: *const c_void) -> *const c_void;
    fn CFDictionaryCreate(
        alloc: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        n: isize,
        cb: *const c_void,
        cb2: *const c_void,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_CF_NUMBER_BOOL_TYPE: i32 = 9;

fn cfstr(s: &str) -> *const c_void {
    unsafe {
        CFStringCreateWithBytes(
            std::ptr::null(),
            s.as_ptr(),
            isize::try_from(s.len()).unwrap_or(isize::MAX),
            K_CF_STRING_ENCODING_UTF8,
            false,
        )
    }
}

/// Whether the process may read on-screen pixels other than its own.
#[must_use]
pub fn screen_recording_granted() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Whether the process may drive other apps' UI (event injection).
#[must_use]
pub fn accessibility_granted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Ask macOS to prompt the user for both grants.
///
/// `AXIsProcessTrustedWithOptions({prompt:true})` opens the System
/// Settings pane; `CGRequestScreenCaptureAccess` raises the Screen
/// Recording alert. Neither grant applies to this process until it
/// relaunches — the daemon respawns the agent once the user retries.
pub fn request_permissions() {
    unsafe {
        let key = cfstr("AXTrustedCheckOptionPrompt");
        let mut yes = true_u8();
        let value = CFNumberCreate(
            std::ptr::null(),
            K_CF_NUMBER_BOOL_TYPE,
            &raw mut yes as *const c_void,
        );
        let keys = [key];
        let values = [value];
        let options = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            std::ptr::null(),
            std::ptr::null(),
        );
        AXIsProcessTrustedWithOptions(options);
        CFRelease(options);
        CFRelease(value);
        CFRelease(key);
        CGRequestScreenCaptureAccess();
    }
}

const fn true_u8() -> u8 {
    1
}
