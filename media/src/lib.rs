//! May's media layer: the encoded video pipeline, the per-session virtual
//! desktop (`computer`), and on-device OCR for the images tool calls return.
//!
//! Everything heavy lives here — `mediax-codec`, `webrtc`, `rav1e`/`yuv` and
//! the platform screen, permission and automation frameworks — so the rest
//! of the workspace builds without them.

/// One virtual desktop per session — computer use.
pub mod computer;
/// The encoded video pipeline a stream carries.
pub mod video;
/// On-device OCR and conditional downscaling for text-dense images.
pub mod vision_ocr;

/// The foundational wire types the media layer reads, under the paths the
/// modules were written against.
pub use may_config::config;
pub use may_protocol::call_images;
pub use may_protocol::id;

/// The automation API `computer_tool`'s element references come from.
#[cfg(target_os = "macos")]
pub use waterkit_automation;
/// The permission helper the `may` binary re-enters for the computer agent.
#[cfg(target_os = "macos")]
pub use waterkit_permission;
/// The WebRTC stack `browser` streams through, re-exported so `may-agent`
/// needs no direct dependency on it.
pub use webrtc;

/// Fixture images shared by the workspace's `#[cfg(test)]` code.
#[cfg(any(test, feature = "testing"))]
pub mod test_support;
