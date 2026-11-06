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

/// The helper bundle's identifier, the TCC and `replayd` subject.
pub const HELPER_BUNDLE_ID: &str = "ai.may.computer-agent";

/// Where `replayd` records each app's direct screen-capture consent,
/// relative to the home directory.
const CAPTURE_APPROVALS: &str =
    "Library/Group Containers/group.com.apple.replayd/ScreenCaptureApprovals.plist";

/// The first macOS release that asks for direct screen-capture consent.
const DIRECT_CAPTURE_CONSENT_SINCE: isize = 15;

/// Every grant as one process sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Grants {
    /// Screen Recording: capture and stills.
    pub screen_recording: bool,
    /// Accessibility: event injection.
    pub accessibility: bool,
    /// `replayd`'s one-time consent to capture without the system window
    /// picker; without it, capture fails even with Screen Recording.
    pub direct_capture: bool,
}

/// The probe process's side: optionally raise the prompts, then report.
///
/// A request also captures the main display once when Screen Recording is
/// granted and direct-capture consent is not: the first direct capture is
/// what makes `replayd` ask for that consent.
///
/// # Errors
/// Returns a message when the consent record cannot be read.
pub fn probe(request: bool) -> Result<Grants, String> {
    if request {
        request_permissions();
        if screen_recording_granted() && !direct_capture_consented()? {
            // The still itself is not needed; capturing raises the consent.
            let _ = super::screenshot::screenshot_png(unsafe { CGMainDisplayID() });
        }
    }
    Ok(Grants {
        screen_recording: screen_recording_granted(),
        accessibility: accessibility_granted(),
        direct_capture: direct_capture_consented()?,
    })
}

/// Whether `replayd` holds this app's direct screen-capture consent. It
/// keys an approval by the app bundle's file URL or by its bundle
/// identifier; systems before macOS 15 have no such consent.
///
/// # Errors
/// Returns a message when the approvals file exists but cannot be read.
pub fn direct_capture_consented() -> Result<bool, String> {
    let version = objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion();
    if version.majorVersion < DIRECT_CAPTURE_CONSENT_SINCE {
        return Ok(true);
    }
    let home = dirs::home_dir().ok_or("no home directory")?;
    let path = home.join(CAPTURE_APPROVALS);
    let approvals = match plist::Value::from_file(&path) {
        Ok(approvals) => approvals,
        Err(error)
            if error
                .as_io()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let approvals = approvals
        .as_dictionary()
        .ok_or_else(|| format!("{} is not a dictionary", path.display()))?;
    Ok(approvals.contains_key(HELPER_BUNDLE_ID)
        || own_bundle_url()?.is_some_and(|url| approvals.contains_key(&url)))
}

/// The file URL of the app bundle this executable runs from, in the form
/// `replayd` keys approvals by; `None` for a bare binary.
fn own_bundle_url() -> Result<Option<String>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let Some(bundle) = exe
        .ancestors()
        .find(|dir| dir.extension().is_some_and(|ext| ext == "app"))
    else {
        return Ok(None);
    };
    let path = objc2_foundation::NSString::from_str(&bundle.to_string_lossy());
    let url = objc2_foundation::NSURL::fileURLWithPath_isDirectory(&path, true);
    Ok(url.absoluteString().map(|url| url.to_string()))
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
