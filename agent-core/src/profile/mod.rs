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
//! invoking the `terminal` tool with `location: <name>`.

pub use may_protocol::profile::*;

use crate::db::ChatDb;
use crate::session::SessionSummary;

/// User-facing preferences stored on a profile.
///
/// Preferences applying to container-environment profiles.
/// Access level granted to a container mount.
/// Resolve the SSH servers attached to a profile by joining the profile's
/// `profile_remote_servers` rows against the global `ssh_servers` catalog.
///
/// - Missing profile (`profile_id: None`) returns an empty list.
/// - Attachment names that no longer resolve to a configured server are
///   logged and dropped (config drift tolerated).
///
/// This is the single source of truth for attachment resolution; both the
/// in-process agent runtime and the `may exec` binary consume it.
///
/// # Errors
/// Returns [`crate::db::ChatDbError`] on storage failure.
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
        if let Some(server) = catalog.get(name.as_str()) {
            resolved.push((*server).clone());
        } else {
            tracing::warn!(
                profile_id,
                ssh_server = name.as_str(),
                "profile has an SSH attachment whose server is no longer configured; skipping"
            );
        }
    }
    Ok(resolved)
}

/// Query profile sessions from the database, grouped into pinned/regular/archived.
///
/// # Errors
/// Returns [`crate::db::ChatDbError`] on storage failure.
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

    for mut summary in all_summaries {
        summary.working = summary.runtime.state.is_running();
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
        pinned.push(PinnedItem::Session(Box::new(s)));
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
///
/// # Errors
/// Returns [`crate::db::ChatDbError`] on storage failure.
pub async fn list_profiles(db: &ChatDb) -> Result<Vec<ProfileInfo>, crate::db::ChatDbError> {
    let models = db.list_profiles().await?;
    let mut profiles = Vec::with_capacity(models.len());
    for model in models {
        profiles.push(db.profile_info_from_model(model).await?);
    }
    Ok(profiles)
}

#[cfg(test)]
mod tests {
    use super::ProfileEnvironment;
    use crate::db::entity::profiles;

    fn make_profile_model(environment: &str) -> profiles::Model {
        profiles::Model {
            id: "profile-id".to_string(),
            slug: Some("local".to_string()),
            name: "Local".to_string(),
            icon: None,
            environment: environment.to_string(),
            container_id: None,
            persona: None,
            location_aware: false,
            pinned_limit: 10,
            sort_order: 0,
            created_at: "2026-03-24T00:00:00Z".to_string(),
            updated_at: "2026-03-24T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn profile_info_wraps_environment_as_home_environment() {
        let info = may_db::profile::profile_from_db_parts(
            make_profile_model("local"),
            None,
            Vec::new(),
            crate::cloud::ProfileCloud::default(),
        )
        .expect("local profile should deserialize");

        assert!(matches!(info.environment.kind(), ProfileEnvironment::Local));
        assert_eq!(info.environment.as_db_str(), "local");
        assert_eq!(info.remote_attachments, [] as [std::string::String; 0]);
    }
}
