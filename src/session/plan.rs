//! Session-scoped planning mode state.

/// Persisted plan mode for a session.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    Default,
    skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SessionPlanState {
    /// Planning workflow is off; the session behaves normally.
    #[default]
    Disabled,
    /// A plan is being drafted; the session is protected from auto-archive.
    Draft,
    /// The plan was approved; execution may proceed.
    Approved,
}

impl SessionPlanState {
    /// Return the persisted name of the plan state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Draft => "draft",
            Self::Approved => "approved",
        }
    }

    /// Whether any planning workflow is active (not `Disabled`).
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Whether the state blocks automatic archiving of the session.
    #[must_use]
    pub const fn blocks_auto_archive(self) -> bool {
        matches!(self, Self::Draft)
    }
}

impl core::str::FromStr for SessionPlanState {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "disabled" => Ok(Self::Disabled),
            "draft" => Ok(Self::Draft),
            "approved" => Ok(Self::Approved),
            other => Err(["unknown session plan state: ", other].concat()),
        }
    }
}
