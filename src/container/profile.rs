//! Profile-driven container overrides (Dockerfile + mounts).

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use aither_sandbox::{ContainerImageSpec, MountSpec};
use async_fs;
use futures_lite::{StreamExt, io::AsyncReadExt};

use super::{ContainerError, image::MAY_IMAGE_TAG};
use crate::db::entity::profiles;
use crate::session::SessionId;

#[derive(Debug, serde::Deserialize)]
struct ProfilePreferences {
    #[serde(default)]
    container: Option<ContainerPreferences>,
}

#[derive(Debug, serde::Deserialize)]
struct ContainerPreferences {
    #[serde(default)]
    dockerfile: Option<String>,
    #[serde(default)]
    mounts: Vec<ContainerMountPreference>,
}

#[derive(Debug, serde::Deserialize)]
struct ContainerMountPreference {
    host_path: String,
    container_path: String,
    #[serde(default = "default_mount_access")]
    access: ContainerMountAccess,
}

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ContainerMountAccess {
    ReadOnly,
    ReadWrite,
}

const fn default_mount_access() -> ContainerMountAccess {
    ContainerMountAccess::ReadOnly
}

/// Build a container image spec from profile preferences.
pub fn image_spec_for_profile(
    may_dir: &Path,
    profile: &profiles::Model,
    default_tag: &str,
) -> Result<ContainerImageSpec, ContainerError> {
    let context_dir = may_dir.join("container");
    let mut spec = ContainerImageSpec::new(default_tag.to_string(), context_dir.clone());
    let Some(preferences) = parse_preferences(profile)? else {
        return Ok(spec);
    };
    let Some(dockerfile) = preferences.dockerfile else {
        return Ok(spec);
    };

    let dockerfile_path = normalize_path(&context_dir, &dockerfile);
    spec = spec.with_dockerfile(dockerfile_path);
    Ok(spec)
}

/// Build typed mount specs from profile preferences.
pub fn mounts_for_profile(profile: &profiles::Model) -> Result<Vec<MountSpec>, ContainerError> {
    let Some(preferences) = parse_preferences(profile)? else {
        return Ok(Vec::new());
    };

    let mut mounts = Vec::with_capacity(preferences.mounts.len());
    for mount in preferences.mounts {
        let host_path = PathBuf::from(mount.host_path);
        let container_path = PathBuf::from(mount.container_path);
        let spec = match mount.access {
            ContainerMountAccess::ReadOnly => MountSpec::read_only(host_path, container_path),
            ContainerMountAccess::ReadWrite => MountSpec::read_write(host_path, container_path),
        };
        mounts.push(spec);
    }
    Ok(mounts)
}

fn parse_preferences(
    profile: &profiles::Model,
) -> Result<Option<ContainerPreferences>, ContainerError> {
    let Some(raw) = profile.preferences_json.as_deref() else {
        return Ok(None);
    };

    let preferences: ProfilePreferences = serde_json::from_str(raw).map_err(|e| {
        ContainerError::Bootstrap(format!(
            "invalid profile.preferences_json for profile {}: {e}",
            profile.id
        ))
    })?;
    Ok(preferences.container)
}

fn normalize_path(context_dir: &Path, configured: &str) -> PathBuf {
    let path = PathBuf::from(configured);
    if path.is_absolute() {
        return path;
    }
    context_dir.join(path)
}

/// Resolve the default image tag for a profile when no explicit `container_image` is set.
///
/// Profiles without a custom Dockerfile share the global May workspace image to
/// avoid duplicate builds across `daily` / `work` style profiles.
pub async fn default_image_tag(
    may_dir: &Path,
    profile: &profiles::Model,
) -> Result<String, ContainerError> {
    if let Some(image) = profile
        .container_image
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(image.to_string());
    }

    super::heel::ensure_container_heel_binary(may_dir).await?;

    let uses_custom_dockerfile = parse_preferences(profile)?
        .and_then(|preferences| preferences.dockerfile)
        .is_some_and(|dockerfile| !dockerfile.trim().is_empty());
    let context_hash = container_context_hash_hex(may_dir).await?;
    let repo = MAY_IMAGE_TAG
        .split(':')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("may-workspace");

    if uses_custom_dockerfile {
        return Ok(format!(
            "{repo}-{}:{context_hash}",
            profile.slug.as_deref().unwrap_or(profile.id.as_str()),
        ));
    }

    Ok(format!("{repo}:{context_hash}"))
}

async fn container_context_hash_hex(may_dir: &Path) -> Result<String, ContainerError> {
    let context_dir = may_dir.join("container");
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_path_recursive(&context_dir, &context_dir, &mut hasher).await?;
    Ok(format!("{:016x}", hasher.finish()))
}

async fn hash_path_recursive(
    root: &Path,
    path: &Path,
    hasher: &mut std::collections::hash_map::DefaultHasher,
) -> Result<(), ContainerError> {
    enum Pending {
        DirEntry(String),
        Path(PathBuf),
    }

    let mut stack = vec![Pending::Path(path.to_path_buf())];
    while let Some(item) = stack.pop() {
        match item {
            Pending::DirEntry(name) => {
                "dir-entry".hash(hasher);
                name.hash(hasher);
            }
            Pending::Path(path) => {
                let metadata = async_fs::metadata(&path).await?;
                if metadata.is_dir() {
                    let mut entries = BTreeMap::<String, PathBuf>::new();
                    let mut read_dir = async_fs::read_dir(&path).await?;
                    while let Some(entry) = read_dir.next().await {
                        let entry = entry?;
                        let name = entry.file_name().to_string_lossy().to_string();
                        entries.insert(name, entry.path());
                    }
                    for (name, child_path) in entries.into_iter().rev() {
                        stack.push(Pending::Path(child_path));
                        stack.push(Pending::DirEntry(name));
                    }
                    continue;
                }

                if !metadata.is_file() {
                    continue;
                }

                "file".hash(hasher);
                path.strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .hash(hasher);

                let mut file = async_fs::File::open(&path).await?;
                let mut buffer = [0_u8; 16 * 1024];
                loop {
                    let read = file.read(&mut buffer).await?;
                    if read == 0 {
                        break;
                    }
                    buffer[..read].hash(hasher);
                }
            }
        }
    }
    Ok(())
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
    use std::fs;

    use aither_sandbox::MountAccess;
    use tempfile::TempDir;

    use super::*;

    fn profile_with_preferences(preferences_json: &str) -> profiles::Model {
        profiles::Model {
            id: "profile-1".to_string(),
            slug: Some("daily".to_string()),
            name: "Daily".to_string(),
            icon: None,
            environment: "container".to_string(),
            container_image: None,
            container_id: None,
            persona: None,
            preferences_json: Some(preferences_json.to_string()),
            location_aware: false,
            pinned_limit: 10,
            sort_order: 0,
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
        }
    }

    fn make_may_dir_with_container_context() -> TempDir {
        let temp = tempfile::tempdir().expect("tempdir");
        let context_dir = temp.path().join("container");
        fs::create_dir_all(context_dir.join("heel").join("bin")).expect("create context");
        fs::write(context_dir.join("Dockerfile"), "FROM debian:bookworm\n").expect("dockerfile");
        fs::write(context_dir.join("heel").join("bin").join("heel"), "binary")
            .expect("heel binary");
        temp
    }

    #[test]
    fn image_spec_uses_profile_dockerfile_relative_to_context() {
        let profile = profile_with_preferences(
            r#"{"container":{"dockerfile":"custom/Dockerfile.profile","mounts":[]}}"#,
        );
        let may_dir = PathBuf::from("/tmp/may");
        let spec =
            image_spec_for_profile(&may_dir, &profile, "may-workspace:test").expect("valid spec");

        assert_eq!(spec.tag, "may-workspace:test");
        assert_eq!(spec.context_dir, PathBuf::from("/tmp/may/container"));
        assert_eq!(
            spec.dockerfile,
            Some(PathBuf::from(
                "/tmp/may/container/custom/Dockerfile.profile"
            ))
        );
    }

    #[test]
    fn mounts_support_read_only_and_read_write() {
        let profile = profile_with_preferences(
            r#"{
                "container":{
                    "mounts":[
                        {"host_path":"/host/ro","container_path":"/mnt/ro","access":"read_only"},
                        {"host_path":"/host/rw","container_path":"/mnt/rw","access":"read_write"}
                    ]
                }
            }"#,
        );

        let mounts = mounts_for_profile(&profile).expect("valid mounts");
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].host_path, PathBuf::from("/host/ro"));
        assert_eq!(mounts[0].container_path, PathBuf::from("/mnt/ro"));
        assert_eq!(mounts[0].access, MountAccess::ReadOnly);
        assert_eq!(mounts[1].access, MountAccess::ReadWrite);
    }

    #[test]
    fn invalid_profile_preferences_fail_fast() {
        let profile = profile_with_preferences("{invalid-json");
        let err = mounts_for_profile(&profile).expect_err("invalid json should fail");
        assert!(
            err.to_string().contains("invalid profile.preferences_json"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn container_context_hash_changes_when_heel_binary_changes() {
        let may_dir = make_may_dir_with_container_context();
        let first_hash = container_context_hash_hex(may_dir.path())
            .await
            .expect("first hash");
        fs::write(
            may_dir.path().join("container/heel/bin/heel"),
            "updated binary",
        )
        .expect("rewrite heel binary");
        let second_hash = container_context_hash_hex(may_dir.path())
            .await
            .expect("second hash");

        assert_ne!(first_hash, second_hash);
    }

    #[test]
    fn container_name_is_profile_session_scoped_and_sanitized() {
        let profile = profiles::Model {
            slug: Some("daily🔥".to_string()),
            ..profile_with_preferences("{}")
        };
        let session_id = SessionId::from_string("chat-123");
        let name = container_name_for_profile_session(&profile, &session_id);
        assert_eq!(name, "may-daily--chat-123");
    }
}
