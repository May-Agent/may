//! macOS privacy grants the agent needs.
//!
//! Screen Recording (capture and stills) and Accessibility (event
//! injection) are TCC entries; the helper reads and requests them only
//! through fresh probe processes, so a grant applies without restarting
//! the daemon.

use std::ffi::c_void;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGDisplayPixelsHigh(display: u32) -> usize;
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

/// The subcommand a fresh probe process runs: `may computer-agent-probe`.
pub const PROBE_COMMAND: &str = "computer-agent-probe";
/// Probe flag that raises the prompts for missing grants before reporting.
pub const PROBE_REQUEST_FLAG: &str = "--request";

/// Both grants as one process sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Grants {
    /// Screen Recording: capture and stills.
    pub screen_recording: bool,
    /// Accessibility: event injection.
    pub accessibility: bool,
}

/// The probe process's side: optionally raise the prompts, then report.
///
/// With Screen Recording granted, a request also captures the main display
/// once: from macOS 15 the first capture that bypasses the system window
/// picker is what makes `replayd` ask for that one-time consent, and the
/// request instance is where the user expects system prompts.
///
/// # Panics
/// Panics when the main display reports an extent beyond `u32`.
pub async fn probe(request: bool) -> Grants {
    if request {
        request_permissions();
        if screen_recording_granted() {
            // SAFETY: plain CoreGraphics queries on the main display.
            let (display, size) = unsafe {
                let display = CGMainDisplayID();
                let extent =
                    |pixels: usize| u32::try_from(pixels).expect("display extent fits u32");
                (
                    display,
                    (
                        extent(CGDisplayPixelsWide(display)),
                        extent(CGDisplayPixelsHigh(display)),
                    ),
                )
            };
            // Only the consent prompt matters; the still is discarded.
            let _ = super::screenshot::screenshot_png(display, size).await;
        }
    }
    Grants {
        screen_recording: screen_recording_granted(),
        accessibility: accessibility_granted(),
    }
}

/// Read the grants in a fresh copy of this executable.
///
/// macOS caches a negative answer in the process that asked, so the
/// serving helper never asks itself. The probe is its child, so TCC
/// attributes it to the helper's bundle. Requests go through
/// `LaunchServices` instead (`ComputerAgent::request_permissions`): a
/// Screen Recording request from a plain child never reaches TCC.
///
/// # Errors
/// Returns a message when the probe cannot run, fails, or prints
/// something other than its report.
pub fn fresh_grants() -> Result<Grants, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let output = std::process::Command::new(exe)
        .arg(PROBE_COMMAND)
        .output()
        .map_err(|e| format!("run permission probe: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "permission probe exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("bad permission probe report: {e}"))
}
