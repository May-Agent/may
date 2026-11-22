//! The turn-progress snapshot the events stream carries.

use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, skyzen::ToSchema,
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// A turn's progress snapshot.
pub struct ProgressReport {
    /// How many items of the session's work are finished.
    pub done: u32,
    /// How many items the session's work has in all, finished or not. At
    /// least `done`.
    pub total: u32,
    /// Unfinished items whose work is complete and only waits to land.
    pub waiting_to_land: Vec<String>,
    /// Unfinished items blocked on something other than the user.
    pub waiting_on_other: Vec<String>,
    /// Unfinished items nobody has started.
    pub not_started: Vec<String>,
    /// Unfinished items blocked on an answer or action from the user.
    pub waiting_on_user: Vec<String>,
}
