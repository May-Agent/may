//! Word-sequence identifiers.
//!
//! Internal ids are random words joined by `-` (for example
//! `waspishly-acquainted-ray`) rather than UUIDs: they read better in logs,
//! and every path or tool result that quotes them costs fewer tokens.

use petname::{Generator, Petnames};

/// A three-word id, e.g. `waspishly-acquainted-ray`.
///
/// # Panics
/// Panics if the embedded petname dictionary cannot produce an id.
#[must_use]
pub fn new() -> String {
    Petnames::default()
        .generate_one(3, "-")
        .expect("failed to generate an id")
}

/// A four-word id for ids that double as capabilities (webhook tokens and
/// other secrets).
///
/// # Panics
/// Panics if the embedded petname dictionary cannot produce an id.
#[must_use]
pub fn new_long() -> String {
    Petnames::default()
        .generate_one(4, "-")
        .expect("failed to generate an id")
}
