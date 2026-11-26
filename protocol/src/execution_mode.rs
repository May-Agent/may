//! Session-scoped execution mode, orthogonal to [`crate::session_plan::SessionPlanState`].

use core::fmt;
use core::str::FromStr;

use crate::model_selector::ModelSelector;

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
    /// Frontier lead paired with a persistent cache-warm companion sidekick.
    Fusion {
        /// Model selector (`provider::model`) used by the companion sidekick.
        #[schema(value_type = String)]
        sidekick_model: ModelSelector,
    },
}

impl ExecutionMode {
    /// Returns the persisted mode tag stored in SQLite.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Orchestrator => "orchestrator",
            Self::Fusion { .. } => "fusion",
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

    /// Whether this session uses fusion mode.
    #[must_use]
    pub const fn is_fusion(&self) -> bool {
        matches!(self, Self::Fusion { .. })
    }

    /// Companion sidekick model selector when in [`Self::Fusion`].
    #[must_use]
    pub const fn sidekick_model(&self) -> Option<&ModelSelector> {
        match self {
            Self::Fusion { sidekick_model } => Some(sidekick_model),
            Self::Standard | Self::Orchestrator => None,
        }
    }
}

impl fmt::Display for ExecutionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Standard | Self::Orchestrator => f.write_str(self.as_str()),
            Self::Fusion { sidekick_model } => write!(f, "fusion:{sidekick_model}"),
        }
    }
}

/// Error returned when parsing a persisted or CLI execution mode fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid execution mode '{value}': expected 'standard', 'orchestrator', or 'fusion:<provider::model>'"
)]
pub struct ExecutionModeParseError {
    value: String,
}

impl FromStr for ExecutionMode {
    type Err = ExecutionModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.trim();
        let lower = trimmed.to_ascii_lowercase();
        match lower.as_str() {
            "standard" => Ok(Self::Standard),
            "orchestrator" => Ok(Self::Orchestrator),
            _ => {
                if let Some(rest) = trimmed
                    .strip_prefix("fusion:")
                    .or_else(|| trimmed.strip_prefix("fusion="))
                    .or_else(|| trimmed.strip_prefix("FUSION:"))
                    .or_else(|| trimmed.strip_prefix("FUSION="))
                {
                    let selector = rest.trim().parse::<ModelSelector>().map_err(|_| {
                        ExecutionModeParseError {
                            value: value.to_string(),
                        }
                    })?;
                    return Ok(Self::Fusion {
                        sidekick_model: selector,
                    });
                }
                Err(ExecutionModeParseError {
                    value: value.to_string(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ExecutionMode;
    use crate::model_selector::ModelSelector;

    #[test]
    fn round_trips_json_and_str() {
        let fusion = ExecutionMode::Fusion {
            sidekick_model: ModelSelector::new("anthropic", "claude-sonnet-4-6"),
        };
        for mode in [ExecutionMode::Standard, ExecutionMode::Orchestrator, fusion] {
            let json = serde_json::to_string(&mode).expect("serialize");
            let parsed: ExecutionMode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed, mode);
            assert_eq!(
                mode.to_string().parse::<ExecutionMode>().expect("parse"),
                mode
            );
        }
        let fusion_json = serde_json::to_string(&ExecutionMode::Fusion {
            sidekick_model: ModelSelector::new("openai", "gpt-5-mini"),
        })
        .expect("serialize fusion");
        assert!(fusion_json.contains("\"mode\":\"fusion\""), "{fusion_json}");
        assert!(
            fusion_json.contains("\"sidekickModel\":\"openai::gpt-5-mini\""),
            "{fusion_json}"
        );
    }
}
