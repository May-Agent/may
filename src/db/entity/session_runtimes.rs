//! Persisted runtime state row model for sessions.

/// Persisted session runtime row.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Owning session id.
    pub session_id: String,
    /// Persisted runtime state string.
    pub state: String,
    /// Run id of the run holding the session, `None` while it is free.
    pub run_id: Option<String>,
    /// Whether a user peer currently owns browser input.
    pub browser_takeover_active: bool,
    /// Whether a takeover-ended alert still needs to surface.
    pub browser_takeover_alert_pending: bool,
    /// Id of the live terminal task, when one is running.
    pub terminal_task_id: Option<String>,
    /// Path of the captured terminal output file.
    pub terminal_output_file: Option<String>,
    /// Tail preview of the terminal output.
    pub terminal_output_preview: Option<String>,
    /// Notice shown to the user about pending terminal input.
    pub terminal_input_notice: Option<String>,
    /// Why the command was promoted to the background.
    pub terminal_background_reason: Option<String>,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
}
