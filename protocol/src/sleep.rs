//! The sleep/shutdown status the daemon reports.

use serde::{Deserialize, Serialize};

/// What clients see of the state.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SleepStatus {
    /// Whether the user is asleep.
    pub asleep: bool,
    /// The session `/sleep` was run in, while the user is asleep.
    #[serde(default)]
    pub origin: Option<String>,
    /// How many health reminders this daemon has raised; a client shows the
    /// reminder when the count grows.
    #[serde(default)]
    pub reminders: u64,
}
