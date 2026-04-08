//! Persisted runtime state row model for sessions.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub session_id: String,
    pub state: String,
    pub browser_takeover_active: bool,
    pub browser_takeover_alert_pending: bool,
    pub terminal_task_id: Option<String>,
    pub terminal_output_file: Option<String>,
    pub terminal_output_preview: Option<String>,
    pub terminal_input_notice: Option<String>,
    pub updated_at: String,
}
