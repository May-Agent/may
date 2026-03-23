//! Profile system for organizing sessions into Arc browser-like profiles.
//!
//! Each profile contains:
//! - **Pinned section**: Pinned sessions and folders (folders group pinned sessions)
//! - **Regular section**: Recent sessions grouped by time
//! - **Archived section**: Auto-archived sessions (48h inactivity)

use crate::db::ChatDb;
use crate::db::entity::profiles;
use crate::session::SessionSummary;

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
pub struct ProfileInfo {
    pub id: String,
    pub slug: Option<String>,
    pub name: String,
    pub icon: Option<String>,
    pub environment: ProfileEnvironment,
    pub persona: Option<String>,
    pub preferences: Option<serde_json::Value>,
    pub location_aware: bool,
    pub pinned_limit: i32,
    pub sort_order: i32,
}

impl ProfileInfo {
    pub fn from_db_model(m: profiles::Model) -> Result<Self, crate::db::ChatDbError> {
        let remote_config = crate::remote_profile::parse_remote_profile_config(&m)
            .map_err(crate::db::ChatDbError::InvalidProfileEnvironment)?;
        let environment = match m.environment.as_str() {
            "container" => ProfileEnvironment::Container {
                image: m.container_image,
                container_id: m.container_id,
            },
            "local" => ProfileEnvironment::Local,
            "remote" => {
                let config = remote_config.ok_or_else(|| {
                    crate::db::ChatDbError::InvalidProfileEnvironment(
                        "remote profile requires preferences.remote.ssh_server_id".to_string(),
                    )
                })?;
                ProfileEnvironment::Remote {
                    ssh_server_id: config
                        .normalized_ssh_server_id()
                        .map(str::to_string)
                        .map_err(crate::db::ChatDbError::InvalidProfileEnvironment)?,
                }
            }
            other => {
                return Err(crate::db::ChatDbError::InvalidProfileEnvironment(
                    other.to_string(),
                ));
            }
        };
        let preferences = m
            .preferences_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        Ok(Self {
            id: m.id,
            slug: m.slug,
            name: m.name,
            icon: m.icon,
            environment,
            persona: m.persona,
            preferences,
            location_aware: m.location_aware,
            pinned_limit: m.pinned_limit,
            sort_order: m.sort_order,
        })
    }
}

/// Environment type for a profile.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ProfileEnvironment {
    Local,
    Container {
        image: Option<String>,
        container_id: Option<String>,
    },
    Remote {
        ssh_server_id: String,
    },
}

impl ProfileEnvironment {
    /// Return the string stored in the DB `environment` column.
    #[must_use]
    pub const fn as_db_str(&self) -> &str {
        match self {
            Self::Local => "local",
            Self::Container { .. } => "container",
            Self::Remote { .. } => "remote",
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

/// Query profile sessions from the database, grouped into pinned/regular/archived.
pub async fn query_profile_sessions(
    db: &ChatDb,
    profile_id: &str,
) -> Result<ProfileSessions, crate::db::ChatDbError> {
    let profile_model = db.get_profile(profile_id).await?.ok_or_else(|| {
        crate::db::ChatDbError::Db(sea_orm::DbErr::RecordNotFound(format!(
            "Profile not found: {profile_id}"
        )))
    })?;
    let profile = ProfileInfo::from_db_model(profile_model)?;

    let all_sessions = db.list_sessions_for_profile(profile_id).await?;
    let folders = db.list_folders(profile_id).await?;

    // Separate sessions into categories
    let mut pinned_bare: Vec<SessionSummary> = Vec::new();
    let mut folder_sessions: std::collections::HashMap<String, Vec<SessionSummary>> =
        std::collections::HashMap::new();
    let mut regular: Vec<SessionSummary> = Vec::new();
    let mut archived: Vec<SessionSummary> = Vec::new();

    for s in all_sessions {
        let mut summary = SessionSummary::from_db_model(s.clone());
        summary.runtime = db
            .get_session_runtime(summary.id.as_str())
            .await?
            .unwrap_or_default();
        if s.archived {
            archived.push(summary);
        } else if s.pinned {
            if let Some(ref fid) = s.folder_id {
                folder_sessions
                    .entry(fid.clone())
                    .or_default()
                    .push(summary);
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

/// List all profiles as [`ProfileInfo`].
pub async fn list_profiles(db: &ChatDb) -> Result<Vec<ProfileInfo>, crate::db::ChatDbError> {
    let models = db.list_profiles().await?;
    models
        .into_iter()
        .map(ProfileInfo::from_db_model)
        .collect::<Result<Vec<_>, _>>()
}
