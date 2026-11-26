//! Session-scoped execution mode, orthogonal to [`crate::session_plan::SessionPlanState`].

use core::fmt;
use core::str::FromStr;

/// Persisted execution mode for a session.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(
    tag = "mode",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ExecutionMode {
    /// Default direct execution.
    #[default]
    Standard,
    /// Prompt-driven subagent delegation to keep the main context lean.
    Orchestrator,
}

impl ExecutionMode {
    /// Returns the persisted mode tag stored in SQLite.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Orchestrator => "orchestrator",
        }
    }

    /// Whether this session uses the default direct execution mode.
    #[must_use]
    pub const fn is_standard(&self) -> bool {
        matches!(self, Self::Standard)
    }

    /// Whether this session uses orchestrator mode.
    #[must_use]
    pub const fn is_orchestrator(&self) -> bool {
        matches!(self, Self::Orchestrator)
    }
}

impl fmt::Display for ExecutionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing a persisted or CLI execution mode fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown execution mode: {value}")]
pub struct ExecutionModeParseError {
    value: String,
}

impl FromStr for ExecutionMode {
    type Err = ExecutionModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "standard" => Ok(Self::Standard),
            "orchestrator" => Ok(Self::Orchestrator),
            _ => Err(ExecutionModeParseError {
                value: value.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ExecutionMode;

    #[test]
    fn round_trips_json_and_str() {
        for mode in [ExecutionMode::Standard, ExecutionMode::Orchestrator] {
            let json = serde_json::to_string(&mode).expect("serialize");
            let parsed: ExecutionMode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed, mode);
            assert_eq!(mode.as_str().parse::<ExecutionMode>().expect("parse"), mode);
        }
    }
}
