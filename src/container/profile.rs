//! Profile-driven container mounts and per-session naming.

use std::path::PathBuf;

use aither_sandbox::MountSpec;

use crate::db::entity::profiles;
use crate::profile::{ContainerMountAccess, ContainerProfilePreferences};
use crate::session::SessionId;

/// Build typed mount specs from profile preferences.
#[must_use]
pub fn mounts_for_profile(preferences: Option<ContainerProfilePreferences>) -> Vec<MountSpec> {
    let Some(preferences) = preferences else {
        return Vec::new();
    };

    let mut mounts = Vec::with_capacity(preferences.mounts.len());
    for mount in preferences.mounts {
        let host_path = PathBuf::from(mount.host_path);
        let spec = match mount.access {
            ContainerMountAccess::ReadOnly => MountSpec::read_only(host_path, mount.container_path),
            ContainerMountAccess::ReadWrite => {
                MountSpec::read_write(host_path, mount.container_path)
            }
        };
        mounts.push(spec);
    }
    mounts
}

/// Resolve a deterministic container name for one profile session.
#[must_use]
pub fn container_name_for_profile_session(
    profile: &profiles::Model,
    session_id: &SessionId,
) -> String {
    let profile_segment = profile
        .slug
        .as_deref()
        .unwrap_or(profile.id.as_str())
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '-',
        })
        .collect::<String>();

    format!("may-{profile_segment}-{}", session_id.as_str())
}

#[cfg(test)]
mod tests {
    use aither_sandbox::{ContainerPath, MountAccess};

    use super::*;
    use crate::profile::ContainerMountPreference;

    fn profile_model() -> profiles::Model {
        profiles::Model {
            id: "profile-1".to_string(),
            slug: Some("daily".to_string()),
            name: "Daily".to_string(),
            icon: None,
            environment: "container".to_string(),
            container_id: None,
            persona: None,
            location_aware: false,
            pinned_limit: 10,
            sort_order: 0,
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn mounts_support_read_only_and_read_write() {
        let mounts = mounts_for_profile(Some(ContainerProfilePreferences {
            mounts: vec![
                ContainerMountPreference {
                    host_path: "/host/ro".to_string(),
                    container_path: ContainerPath::new("/mnt/ro").unwrap(),
                    access: ContainerMountAccess::ReadOnly,
                },
                ContainerMountPreference {
                    host_path: "/host/rw".to_string(),
                    container_path: ContainerPath::new("/mnt/rw").unwrap(),
                    access: ContainerMountAccess::ReadWrite,
                },
            ],
        }));
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].host_path, PathBuf::from("/host/ro"));
        assert_eq!(mounts[0].container_path.as_str(), "/mnt/ro");
        assert_eq!(mounts[0].access, MountAccess::ReadOnly);
        assert_eq!(mounts[1].access, MountAccess::ReadWrite);
    }

    #[test]
    fn mount_preference_rejects_relative_container_path() {
        let error = serde_json::from_str::<ContainerMountPreference>(
            r#"{"hostPath":"/host/data","containerPath":"mnt/data"}"#,
        )
        .expect_err("relative container path must be rejected");
        assert!(
            error
                .to_string()
                .contains("container path must be absolute"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn container_name_is_profile_session_scoped_and_sanitized() {
        let profile = profiles::Model {
            slug: Some("daily🔥".to_string()),
            ..profile_model()
        };
        let session_id = SessionId::from_string("chat-123");
        let name = container_name_for_profile_session(&profile, &session_id);
        assert_eq!(name, "may-daily--chat-123");
    }
}
