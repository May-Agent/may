//! Issue backends, re-exported from `may-client` plus the owner-approval
//! helpers that stay in the agent crate.

pub use may_client::issue::*;

/// Owner-approval helpers for issue writes.
pub mod owner;
