//! Managed lifecycle for the heel sandbox binary.
//!
//! - [`binary::configure_heel_binary`] installs or resolves the heel
//!   binary for a given `MayConfig` and returns the on-disk path.
//! - [`release::ensure_managed_heel_binary`] downloads the correct
//!   release asset for the host triple and caches it under the may
//!   profile directory.

pub mod binary;
pub mod release;

pub use binary::configure_heel_binary;
