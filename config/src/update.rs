//! Where a build gets its updates from, and how it was built.
//!
//! The channels themselves — the GitHub release and the CI artifact
//! downloads — live in `may-agent`'s `update` module; the `[update] channel`
//! setting and the build kind it defaults from are here.

/// Where a build gets its updates from.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
    skyzen::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// The latest GitHub release.
    Stable,
    /// The latest successful CI build of `main`.
    Dev,
}

impl Channel {
    /// The channel of the running binary's kind.
    #[must_use]
    pub const fn of_build() -> Self {
        BuildKind::current().channel()
    }
}

/// How the running binary was built, recorded when it is compiled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildKind {
    /// A cargo-dist release build (`release-build`).
    Release,
    /// Compiled by GitHub Actions: the build the dev channel downloads.
    Ci,
    /// Compiled anywhere else, such as `cargo build` on a developer's machine.
    Local,
}

impl BuildKind {
    /// The kind of the running binary.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(feature = "release-build") {
            Self::Release
        } else if matches!(env!("MAY_BUILT_BY_CI").as_bytes(), b"true") {
            Self::Ci
        } else {
            Self::Local
        }
    }

    /// The channel a build of this kind follows when `[update] channel` is
    /// unset: a release build is `stable`, any other `dev`.
    #[must_use]
    pub const fn channel(self) -> Channel {
        match self {
            Self::Release => Channel::Stable,
            Self::Ci | Self::Local => Channel::Dev,
        }
    }

    /// Whether the daemon of a build of this kind updates itself in the
    /// background. A local build never does, so a `cargo build` binary is
    /// never overwritten behind its developer; `may update` still works on
    /// it.
    #[must_use]
    pub const fn auto_updates(self) -> bool {
        match self {
            Self::Release | Self::Ci => true,
            Self::Local => false,
        }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Stable => "stable",
            Self::Dev => "dev",
        })
    }
}
