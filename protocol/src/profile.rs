//! The profile wire types: preferences, environments and pinned items.

use crate::session_summary::SessionSummary;

/// Slug of the built-in Daily profile: everyday chat on the local machine.
/// The web UI lands on it by default.
pub const DAILY_PROFILE_SLUG: &str = "daily";

/// Slug of the built-in Build profile: coding work on the local machine.
/// The TUI and one-shot runs land on it by default, and a session created
/// without a profile falls back to it.
pub const BUILD_PROFILE_SLUG: &str = "build";

/// Slug of the built-in Work profile: work sessions on the local machine.
pub const WORK_PROFILE_SLUG: &str = "work";

/// A profile's user-tunable preferences.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePreferences {
    /// Container-environment preferences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerProfilePreferences>,
}

/// Container-environment preferences of a profile.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct ContainerProfilePreferences {
    /// Extra host paths mounted into the container.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<ContainerMountPreference>,
}

/// A host path mounted into the container.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContainerMountPreference {
    /// Path on the host to mount.
    pub host_path: String,
    /// Absolute mount point inside the container.
    #[schema(value_type = String)]
    pub container_path: aither_sandbox::ContainerPath,
    /// Mount access level.
    #[serde(default)]
    pub access: ContainerMountAccess,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    skyzen::ToSchema,
)]
/// Access level of a host path mounted into the container.
#[serde(rename_all = "snake_case")]
pub enum ContainerMountAccess {
    /// Mount is readable but not writable.
    #[default]
    ReadOnly,
    /// Mount is readable and writable.
    ReadWrite,
}

/// A profile with its organized sessions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct ProfileSessions {
    /// The profile these sessions belong to.
    pub profile: ProfileInfo,
    /// Pinned items (sessions and folders) in display order.
    pub pinned: Vec<PinnedItem>,
    /// Unpinned, non-archived sessions.
    pub regular: Vec<SessionSummary>,
    /// Archived sessions.
    pub archived: Vec<SessionSummary>,
}

/// Lightweight profile info (from DB model).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfileInfo {
    /// Profile id.
    pub id: String,
    /// Short URL/display slug.
    pub slug: Option<String>,
    /// Display name.
    pub name: String,
    /// Icon identifier, if any.
    pub icon: Option<String>,
    /// Environment the profile's agents run in.
    pub environment: HomeEnvironment,
    /// Persona text folded into the system prompt.
    pub persona: Option<String>,
    /// User-tunable profile preferences.
    pub preferences: Option<ProfilePreferences>,
    /// Whether location context is injected for this profile.
    pub location_aware: bool,
    /// Maximum number of pinned sessions and folders.
    pub pinned_limit: i32,
    /// Display order among profiles.
    pub sort_order: i32,
    /// SSH server names (matching `SshServerConfig.name` in `config.toml`)
    /// attached to this profile as auxiliary environments. Ordered by
    /// `profile_remote_servers.sort_order`.
    pub remote_attachments: Vec<String>,
    /// The cloud provider accounts the profile's machines are created in,
    /// set up with `/cloud`.
    pub cloud: crate::cloud::ProfileCloud,
}

/// The single designated home environment for a profile.
///
/// Every profile owns exactly one home environment, and all session sandboxes
/// and IPC-based terminal commands are rooted in that environment.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(transparent)]
pub struct HomeEnvironment(ProfileEnvironment);

/// Environment type for a profile's home environment.
///
/// Only `Local` and `Container` are valid home environments. SSH servers are
/// attached via the `profile_remote_servers` junction table and exposed on
/// [`ProfileInfo::remote_attachments`]; agents reach them by invoking the
/// `terminal` tool with `location: <name>` rather than
/// by treating the whole profile as remote.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ProfileEnvironment {
    /// Run agents on the host.
    Local,
    /// Run agents inside a container.
    Container {
        /// Running container id once started.
        container_id: Option<String>,
    },
}

/// An item in the pinned section: either a bare session or a folder.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PinnedItem {
    /// A single pinned session (not grouped in a folder).
    Session(Box<SessionSummary>),
    /// A named group of pinned sessions.
    Folder {
        /// Folder id.
        id: String,
        /// Folder display name.
        name: String,
        /// Display order within the pinned section.
        sort_order: i32,
        /// Sessions grouped in the folder.
        sessions: Vec<SessionSummary>,
    },
}

impl ContainerMountAccess {
    /// Persisted `snake_case` string form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::ReadWrite => "read_write",
        }
    }
}

impl core::str::FromStr for ContainerMountAccess {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "read_only" => Ok(Self::ReadOnly),
            "read_write" => Ok(Self::ReadWrite),
            other => Err(format!("invalid container mount access: {other}")),
        }
    }
}

impl ProfileSessions {
    /// Marks the sessions in `busy` as working: they run a background
    /// command or subagent the database does not know about.
    pub fn mark_working(&mut self, busy: &std::collections::BTreeSet<String>) {
        let sessions = self
            .pinned
            .iter_mut()
            .flat_map(|item| match item {
                PinnedItem::Session(session) => std::slice::from_mut(&mut **session).iter_mut(),
                PinnedItem::Folder { sessions, .. } => sessions.iter_mut(),
            })
            .chain(self.regular.iter_mut())
            .chain(self.archived.iter_mut());
        for session in sessions {
            session.working |= busy.contains(session.id.as_str());
        }
    }
}

impl HomeEnvironment {
    /// Wrap an environment kind.
    #[must_use]
    pub const fn new(environment: ProfileEnvironment) -> Self {
        Self(environment)
    }

    /// The underlying environment kind.
    #[must_use]
    pub const fn kind(&self) -> &ProfileEnvironment {
        &self.0
    }

    /// Unwrap into the environment kind.
    #[must_use]
    pub fn into_kind(self) -> ProfileEnvironment {
        self.0
    }

    /// Persisted database string form.
    #[must_use]
    pub const fn as_db_str(&self) -> &str {
        self.0.as_db_str()
    }
}

impl AsRef<ProfileEnvironment> for HomeEnvironment {
    fn as_ref(&self) -> &ProfileEnvironment {
        self.kind()
    }
}

impl ProfileEnvironment {
    /// Return the string stored in the DB `environment` column.
    #[must_use]
    pub const fn as_db_str(&self) -> &str {
        match self {
            Self::Local => "local",
            Self::Container { .. } => "container",
        }
    }
}

impl PinnedItem {
    /// Sort order used for interleaving sessions and folders.
    #[must_use]
    pub fn sort_order(&self) -> i32 {
        match self {
            Self::Session(s) => s.pin_sort_order.unwrap_or(0),
            Self::Folder { sort_order, .. } => *sort_order,
        }
    }
}
