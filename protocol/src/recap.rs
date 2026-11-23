//! The recap a turn writes for a later one to open with.

use serde::{Deserialize, Serialize};

/// The recap row's `content`, persisted as JSON.
///
/// Everything the card renders plus the turn count it covered, so idle
/// scheduling can tell whether the session has moved on since its last
/// recap. Clients replaying history parse the same shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecapRecord {
    /// The catch-up summary.
    pub summary: String,
    /// The suggested next step, when the recap named one.
    pub next_action: Option<String>,
    /// User messages the recap covered when it was written.
    pub turns: usize,
}
