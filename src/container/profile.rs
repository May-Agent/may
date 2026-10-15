//! Profile-driven container overrides (Dockerfile + mounts).

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use aither_sandbox::{ContainerImageSpec, MountSpec};
use async_fs;
use futures_lite::{StreamExt, io::AsyncReadExt};

use super::{ContainerError, image::MAY_IMAGE_TAG};
use crate::db::entity::profiles;
use crate::profile::{ContainerMountAccess, ContainerProfilePreferences};
use crate::session::SessionId;

/// Build a container image spec from profile preferences.
#[must_use]
pub fn image_spec_for_profile(
    may_dir: &Path,
    _profile: &profiles::Model,
    default_tag: &str,
    preferences: Option<ContainerProfilePreferences>,
) -> ContainerImageSpec {
    let context_dir = may_dir.join("container");
    let mut spec = ContainerImageSpec::new(default_tag.to_string(), context_dir.clone());
    let Some(preferences) = preferences else {
        return spec;
    };
    let Some(dockerfile) = preferences.dockerfile else {
        return spec;
    };

    let dockerfile_path = normalize_path(&context_dir, &dockerfile);
    spec = spec.with_dockerfile(dockerfile_path);
    spec
}

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

fn normalize_path(context_dir: &Path, configured: &str) -> PathBuf {
    let path = PathBuf::from(configured);
    if path.is_absolute() {
        return path;
    }
    context_dir.join(path)
}

/// Stage the assets the container runtime needs (the managed `heel` binary)
/// under `may_dir`.
///
/// # Errors
///
/// Returns [`ContainerError`] when preparing the heel binary fails.
pub async fn ensure_container_runtime_assets(may_dir: &Path) -> Result<(), ContainerError> {
    super::heel::ensure_container_heel_binary(may_dir).await
}

/// Resolve the default image tag for a profile when no explicit `container_image` is set.
///
/// Profiles without a custom Dockerfile share the global May workspace image to
/// avoid duplicate builds across `daily` / `work` style profiles.
///
/// # Errors
///
/// Returns [`ContainerError`] when preparing container runtime assets fails or
/// the container context directory cannot be hashed.
pub async fn default_image_tag(
    may_dir: &Path,
    profile: &profiles::Model,
    preferences: Option<ContainerProfilePreferences>,
) -> Result<String, ContainerError> {
    if let Some(image) = profile
        .container_image
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(image.to_string());
    }

    ensure_container_runtime_assets(may_dir).await?;

    let uses_custom_dockerfile = preferences
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
    Box::pin(hash_path_recursive(&context_dir, &context_dir, &mut hasher)).await?;
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

    use aither_sandbox::{ContainerPath, MountAccess};
    use tempfile::TempDir;

    use super::*;
    use crate::profile::ContainerMountPreference;

    fn profile_model() -> profiles::Model {
        profiles::Model {
            id: "profile-1".to_string(),
            slug: Some("daily".to_string()),
            name: "Daily".to_string(),
            icon: None,
            environment: "container".to_string(),
            container_image: None,
            container_id: None,
            persona: None,
            remote_ssh_server_id: None,
            container_dockerfile: None,
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
        let may_dir = PathBuf::from("/tmp/may");
        let spec = image_spec_for_profile(
            &may_dir,
            &profile_model(),
            "may-workspace:test",
            Some(ContainerProfilePreferences {
                dockerfile: Some("custom/Dockerfile.profile".to_string()),
                mounts: Vec::new(),
            }),
        );

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
        let mounts = mounts_for_profile(Some(ContainerProfilePreferences {
            dockerfile: None,
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
            ..profile_model()
        };
        let session_id = SessionId::from_string("chat-123");
        let name = container_name_for_profile_session(&profile, &session_id);
        assert_eq!(name, "may-daily--chat-123");
    }
}
