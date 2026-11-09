//! The daemon's start and stop, for telling an unexpected exit apart.

/// The latest daemon start; one row.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Always 1.
    pub id: i32,
    /// Id of the process session the daemon led.
    pub process_session: i64,
    /// Whether it stopped through its shutdown path.
    pub stopped: bool,
    /// When it started (ISO 8601).
    pub started_at: String,
}
