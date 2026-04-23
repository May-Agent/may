//! Profile system for organizing sessions into Arc browser-like profiles.
//!
//! Each profile contains:
//! - **Pinned section**: Pinned sessions and folders (folders group pinned sessions)
//! - **Regular section**: Recent sessions grouped by time
//! - **Archived section**: Auto-archived sessions (48h inactivity)
//!
//! A profile's *home environment* is always [`ProfileEnvironment::Local`] or
//! [`ProfileEnvironment::Container`]. Remote SSH servers are attached
//! separately via the `profile_remote_servers` junction table and referenced
//! through [`ProfileInfo::remote_attachments`]; the agent reaches them by
//! invoking the `terminal` tool with `mode: "ssh"` and
//! `ssh_server_id: <name>`.

use crate::db::ChatDb;
use crate::db::entity::profiles;
use crate::session::SessionSummary;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePreferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerProfilePreferences>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerProfilePreferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<ContainerMountPreference>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerMountPreference {
    pub host_path: String,
    pub container_path: String,
    #[serde(default)]
    pub access: ContainerMountAccess,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerMountAccess {
    #[default]
    ReadOnly,
    ReadWrite,
}

impl ContainerMountAccess {
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

/// A profile with its organized sessions.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProfileSessions {
    pub profile: ProfileInfo,
    pub pinned: Vec<PinnedItem>,
    pub regular: Vec<SessionSummary>,
    pub archived: Vec<SessionSummary>,
}

/// Lightweight profile info (from DB model).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileInfo {
    pub id: String,
    pub slug: Option<String>,
    pub name: String,
    pub icon: Option<String>,
    pub environment: HomeEnvironment,
    pub persona: Option<String>,
    pub preferences: Option<ProfilePreferences>,
    pub location_aware: bool,
    pub pinned_limit: i32,
    pub sort_order: i32,
    /// SSH server names (matching `SshServerConfig.name` in `config.toml`)
    /// attached to this profile as auxiliary environments. Ordered by
    /// `profile_remote_servers.sort_order`.
    pub remote_attachments: Vec<String>,
}

impl ProfileInfo {
    pub fn from_db_parts(
        m: profiles::Model,
        preferences: Option<ProfilePreferences>,
        remote_attachments: Vec<String>,
    ) -> Result<Self, crate::db::ChatDbError> {
        let environment = match m.environment.as_str() {
            "container" => ProfileEnvironment::Container {
                image: m.container_image,
                container_id: m.container_id,
            },
            "local" => ProfileEnvironment::Local,
            other => {
                return Err(crate::db::ChatDbError::InvalidProfileEnvironment(
                    other.to_string(),
                ));
            }
        };
        Ok(Self {
            id: m.id,
            slug: m.slug,
            name: m.name,
            icon: m.icon,
            environment: HomeEnvironment::new(environment),
            persona: m.persona,
            preferences,
            location_aware: m.location_aware,
            pinned_limit: m.pinned_limit,
            sort_order: m.sort_order,
            remote_attachments,
        })
    }
}

/// The single designated home environment for a profile.
///
/// Every profile owns exactly one home environment, and all session sandboxes
/// and IPC-based terminal commands are rooted in that environment.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(transparent)]
pub struct HomeEnvironment(ProfileEnvironment);

impl HomeEnvironment {
    #[must_use]
    pub const fn new(environment: ProfileEnvironment) -> Self {
        Self(environment)
    }

    #[must_use]
    pub const fn kind(&self) -> &ProfileEnvironment {
        &self.0
    }

    #[must_use]
    pub fn into_kind(self) -> ProfileEnvironment {
        self.0
    }

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

/// Environment type for a profile's home environment.
///
/// Only `Local` and `Container` are valid home environments. SSH servers are
/// attached via the `profile_remote_servers` junction table and exposed on
/// [`ProfileInfo::remote_attachments`]; agents reach them by invoking the
/// `terminal` tool with `mode: "ssh"` and `ssh_server_id: <name>` rather than
/// by treating the whole profile as remote.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ProfileEnvironment {
    Local,
    Container {
        image: Option<String>,
        container_id: Option<String>,
    },
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

/// An item in the pinned section: either a bare session or a folder.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PinnedItem {
    Session(SessionSummary),
    Folder {
        id: String,
        name: String,
        sort_order: i32,
        sessions: Vec<SessionSummary>,
    },
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

/// Resolve the SSH servers attached to a profile by joining the profile's
/// `profile_remote_servers` rows against the global `ssh_servers` catalog.
///
/// - Missing profile (`profile_id: None`) returns an empty list.
/// - Attachment names that no longer resolve to a configured server are
///   logged and dropped (config drift tolerated).
///
/// This is the single source of truth for attachment resolution; both the
/// in-process agent runtime and the `may exec` binary consume it.
pub async fn resolve_attached_ssh_servers(
    db: &ChatDb,
    profile_id: Option<&str>,
    all_servers: &[crate::config::SshServerConfig],
) -> Result<Vec<crate::config::SshServerConfig>, crate::db::ChatDbError> {
    let Some(profile_id) = profile_id else {
        return Ok(Vec::new());
    };
    let attachments = db.list_profile_remote_servers(profile_id).await?;
    if attachments.is_empty() {
        return Ok(Vec::new());
    }
    let catalog: std::collections::HashMap<&str, &crate::config::SshServerConfig> = all_servers
        .iter()
        .map(|server| (server.name.as_str(), server))
        .collect();
    let mut resolved = Vec::with_capacity(attachments.len());
    for name in &attachments {
        match catalog.get(name.as_str()) {
            Some(server) => resolved.push((*server).clone()),
            None => tracing::warn!(
                profile_id,
                ssh_server = name.as_str(),
                "profile has an SSH attachment whose server is no longer configured; skipping"
            ),
        }
    }
    Ok(resolved)
}

/// Query profile sessions from the database, grouped into pinned/regular/archived.
pub async fn query_profile_sessions(
    db: &ChatDb,
    profile_id: &str,
) -> Result<ProfileSessions, crate::db::ChatDbError> {
    let (profile_model, db_sessions, folders) = tokio::join!(
        db.get_profile(profile_id),
        db.list_sessions_for_profile(profile_id),
        db.list_folders(profile_id),
    );
    let profile_model = profile_model?.ok_or_else(|| {
        crate::db::ChatDbError::RowNotFound(format!("Profile not found: {profile_id}"))
    })?;
    let profile = db.profile_info_from_model(profile_model).await?;
    let all_summaries = crate::session::summarize_db_sessions(db, db_sessions?).await?;
    let folders = folders?;

    // Separate sessions into categories
    let mut pinned_bare: Vec<SessionSummary> = Vec::new();
    let mut folder_sessions: std::collections::HashMap<String, Vec<SessionSummary>> =
        std::collections::HashMap::new();
    let mut regular: Vec<SessionSummary> = Vec::new();
    let mut archived: Vec<SessionSummary> = Vec::new();

    for summary in all_summaries {
        if summary.archived {
            archived.push(summary);
        } else if summary.pinned {
            if let Some(fid) = summary.folder_id.clone() {
                folder_sessions.entry(fid).or_default().push(summary);
            } else {
                pinned_bare.push(summary);
            }
        } else {
            regular.push(summary);
        }
    }

    // Build pinned items: interleave bare sessions and folders by sort_order
    let mut pinned: Vec<PinnedItem> = Vec::new();

    for s in pinned_bare {
        pinned.push(PinnedItem::Session(s));
    }

    for f in folders {
        let sessions = folder_sessions.remove(&f.id).unwrap_or_default();
        pinned.push(PinnedItem::Folder {
            id: f.id,
            name: f.name,
            sort_order: f.sort_order,
            sessions,
        });
    }

    pinned.sort_by_key(PinnedItem::sort_order);

    Ok(ProfileSessions {
        profile,
        pinned,
        regular,
        archived,
    })
}

#[cfg(test)]
mod tests {
    use super::{HomeEnvironment, ProfileEnvironment, ProfileInfo};
    use crate::db::entity::profiles;

    fn make_profile_model(environment: &str) -> profiles::Model {
        profiles::Model {
            id: "profile-id".to_string(),
            slug: Some("local".to_string()),
            name: "Local".to_string(),
            icon: None,
            environment: environment.to_string(),
            container_image: None,
            container_id: None,
            persona: None,
            remote_ssh_server_id: None,
            container_dockerfile: None,
            location_aware: false,
            pinned_limit: 10,
            sort_order: 0,
            created_at: "2026-03-24T00:00:00Z".to_string(),
            updated_at: "2026-03-24T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn profile_info_wraps_environment_as_home_environment() {
        let info = ProfileInfo::from_db_parts(make_profile_model("local"), None, Vec::new())
            .expect("local profile should deserialize");

        assert!(matches!(info.environment, HomeEnvironment(_)));
        assert!(matches!(info.environment.kind(), ProfileEnvironment::Local));
        assert_eq!(info.environment.as_db_str(), "local");
        assert!(info.remote_attachments.is_empty());
    }

    #[test]
    fn profile_info_rejects_legacy_remote_environment() {
        let error = ProfileInfo::from_db_parts(make_profile_model("remote"), None, Vec::new())
            .expect_err("legacy remote environment must not deserialize");
        let message = error.to_string();
        assert!(message.contains("remote"), "unexpected error: {message}");
    }
}

/// List all profiles as [`ProfileInfo`].
pub async fn list_profiles(db: &ChatDb) -> Result<Vec<ProfileInfo>, crate::db::ChatDbError> {
    let models = db.list_profiles().await?;
    let mut profiles = Vec::with_capacity(models.len());
    for model in models {
        let preferences = db.load_profile_preferences(&model).await?;
        let attachments = db.list_profile_remote_servers(&model.id).await?;
        profiles.push(ProfileInfo::from_db_parts(model, preferences, attachments)?);
    }
    Ok(profiles)
}
