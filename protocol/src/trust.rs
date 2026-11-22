//! The persisted trust level of one repository.

/// The persisted trust level for one repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    /// Forge PRs and issues.
    Full,
    /// Forge PRs, internal may issues, outbound screen on published text.
    PrsOnly,
    /// May PR + may issue on a session clone; the checkout stays read-only.
    LocalBranch,
    /// Write the launch checkout directly (not recommended).
    Local,
}

impl TrustLevel {
    /// Parse a `repo trust set` argument.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "full" => Some(Self::Full),
            "prs-only" | "prs_only" => Some(Self::PrsOnly),
            "local-branch" | "local_branch" => Some(Self::LocalBranch),
            "local" => Some(Self::Local),
            _ => None,
        }
    }

    /// The stored representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::PrsOnly => "prs-only",
            Self::LocalBranch => "local-branch",
            Self::Local => "local",
        }
    }

    /// The session clone is made from the forge remote and its PRs open on
    /// it; every other level keeps its clone local.
    #[must_use]
    pub const fn publishes_to_forge(self) -> bool {
        matches!(self, Self::Full | Self::PrsOnly)
    }

    /// Work lands through a session clone and `pr`, not the launch checkout.
    #[must_use]
    pub const fn clones_work(self) -> bool {
        matches!(self, Self::Full | Self::PrsOnly | Self::LocalBranch)
    }
}
