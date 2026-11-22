//! The lifecycle states of a scheduled task's run.

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
/// The lifecycle state of one scheduled task's run.
pub enum TaskRunStatus {
    /// Run is in progress.
    Running,
    /// Run completed successfully.
    Success,
    /// Run failed.
    Failed,
    /// Run was interrupted (restart/shutdown).
    Interrupted,
}

impl TaskRunStatus {
    /// Persisted `snake_case` string form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    /// Parse a persisted status string.
    ///
    /// # Errors
    /// Returns the unrecognized value when it is not a known status.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "running" => Ok(Self::Running),
            "success" => Ok(Self::Success),
            "failed" => Ok(Self::Failed),
            "interrupted" => Ok(Self::Interrupted),
            other => Err(format!("unknown task run status: {other}")),
        }
    }
}
