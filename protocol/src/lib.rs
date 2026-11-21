//! May's foundational types: the ids, wire shapes and leaf utilities every
//! crate above it shares, with no dependency on `may-agent` itself.
//!
//! What lives here is what more than one layer reads: the model-facing ids
//! and word pools, the XML block and command-reading shapes clients render,
//! the persisted ask-user requests a refresh recovers, and the small
//! standalone utilities (panic boundary, log ring, quota decoding, file
//! tracking) that need nothing but the platform.

/// The `ask_user` wire types and their persisted shapes.
pub mod ask_user;
/// Provider prompt-cache lifetime.
pub mod cache_lifetime;
/// Streamed command output for the model.
pub mod command_output;
/// How a terminal call's script is read before it runs.
pub mod command_reading;
/// Live decode-speed measurement for streamed model output.
pub mod decode_meter;
/// Per-session file-change tracking backed by detached git trees.
pub mod file_tracker;
/// Model-facing ids: session names and each session's pool of words.
pub mod id;
/// An assistant response as it streams: the live row and the settled one.
pub mod live_response;
/// Location lookup for location-aware profiles.
pub mod location;
/// A ring buffer of the tracing events the daemon writes, dumped on demand.
pub mod log_ring;
/// Model-facing ids: session names and each session's pool of words.
pub mod model_ids;
/// Model selection and filtering helpers.
pub mod model_selector;
/// Catches a panic at the boundary of the operation that raised it.
pub mod panic_boundary;
/// Provider quota and rate-limit decoding.
pub mod quota;
/// The secret types the wire shares; the store is `may-config`'s.
pub mod secrets;
/// XML block serialization for prompt/context payloads.
pub mod xml;

/// Test support shared by the workspace's `#[cfg(test)]` code.
#[cfg(feature = "testing")]
pub mod test_support;
