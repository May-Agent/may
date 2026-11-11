//! Resolving the prompt file a call names: the paths a subagent's caller
//! writes are virtual, so each is mapped onto the host directory it really
//! refers to without letting one escape its root.

use std::path::{Component, Path, PathBuf};

/// Maps a path prefix the caller sees onto the host directory it names.
#[derive(Clone, Debug)]
pub struct SubagentFileMount {
    /// Path prefix as the caller sees it.
    pub virtual_prefix: PathBuf,
    /// Directory on the host that prefix resolves to.
    pub host_root: PathBuf,
}

impl SubagentFileMount {
    /// Maps `virtual_prefix` onto `host_root`.
    #[must_use]
    pub fn new(virtual_prefix: impl Into<PathBuf>, host_root: impl Into<PathBuf>) -> Self {
        Self {
            virtual_prefix: virtual_prefix.into(),
            host_root: host_root.into(),
        }
    }
}

/// The mounts and base directory prompt files resolve through.
#[derive(Clone, Debug, Default)]
pub struct PromptFiles {
    base_dir: Option<PathBuf>,
    mounts: Vec<SubagentFileMount>,
}

async fn exists(path: &Path) -> anyhow::Result<bool> {
    crate::harness::fs_util::path_exists(path)
        .await
        .map_err(|error| anyhow::anyhow!("failed to inspect path '{}': {error}", path.display()))
}

fn strip_curdir(path: &Path) -> PathBuf {
    path.components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect()
}

fn escapes(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

/// `path` below `prefix`, `None` when it is not, is empty or escapes.
fn mount_relative(path: &Path, prefix: &Path) -> Option<PathBuf> {
    let path = strip_curdir(path);
    let prefix = strip_curdir(prefix);
    if path.as_os_str().is_empty() || escapes(&path) || escapes(&prefix) {
        return None;
    }
    if prefix.as_os_str().is_empty() {
        return Some(path);
    }
    let relative = strip_curdir(path.strip_prefix(&prefix).ok()?);
    (!relative.as_os_str().is_empty()).then_some(relative)
}

impl PromptFiles {
    /// Files resolved against `base_dir` through `mounts`.
    #[must_use]
    pub fn new(base_dir: PathBuf, mounts: impl IntoIterator<Item = SubagentFileMount>) -> Self {
        Self {
            base_dir: Some(base_dir),
            mounts: mounts.into_iter().collect(),
        }
    }

    /// The mounts for a call made from `caller`: its own directory first,
    /// then the configured ones.
    fn effective(&self, caller: Option<&Path>) -> Vec<SubagentFileMount> {
        let caller = caller
            .filter(|dir| self.base_dir.as_deref() != Some(*dir))
            .map(|dir| SubagentFileMount::new(".", dir));
        let configured = if self.mounts.is_empty() {
            vec![SubagentFileMount::new(
                ".",
                self.base_dir.clone().unwrap_or_else(|| PathBuf::from(".")),
            )]
        } else {
            self.mounts.clone()
        };
        caller.into_iter().chain(configured).collect()
    }

    async fn through_mounts(
        shown: &Path,
        file: &Path,
        mounts: &[SubagentFileMount],
    ) -> anyhow::Result<PathBuf> {
        let mut matched: Vec<_> = mounts
            .iter()
            .enumerate()
            .filter_map(|(index, mount)| {
                mount_relative(file, &mount.virtual_prefix).map(|relative| (index, mount, relative))
            })
            .collect();
        matched.sort_by_key(|(index, mount, _)| {
            (
                std::cmp::Reverse(strip_curdir(&mount.virtual_prefix).components().count()),
                *index,
            )
        });
        let mut attempted = Vec::new();
        for (_, mount, relative) in matched {
            let candidate = mount.host_root.join(&relative);
            attempted.push(strip_curdir(&mount.virtual_prefix).join(&relative));
            if exists(&candidate).await? {
                return Ok(candidate);
            }
        }
        if attempted.is_empty() {
            anyhow::bail!(
                "Subagent file '{}' is outside mounted paths or uses forbidden path components",
                shown.display()
            );
        }
        let attempted: Vec<_> = attempted
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        anyhow::bail!(
            "Subagent file '{}' not found. Attempted: {}",
            shown.display(),
            attempted.join(", ")
        )
    }

    /// The host path of `file`, named by a call made from `caller`.
    ///
    /// # Errors
    /// Returns why the file resolves nowhere.
    pub async fn resolve(&self, file: &Path, caller: Option<&Path>) -> anyhow::Result<PathBuf> {
        let mounts = self.effective(caller);
        if file.is_absolute() {
            if let Some(base) = &self.base_dir
                && let Ok(stripped) = file.strip_prefix(base)
            {
                let relative = strip_curdir(stripped);
                if !relative.as_os_str().is_empty() {
                    return Self::through_mounts(&relative, &relative, &mounts).await;
                }
            }
            if exists(file).await? {
                return Ok(file.to_path_buf());
            }
            anyhow::bail!("Subagent file '{}' not found", file.display());
        }
        Self::through_mounts(file, file, &mounts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_resolves_below_its_prefix_and_never_above_it() {
        assert_eq!(
            mount_relative(Path::new("skills/a.md"), Path::new("skills")),
            Some(PathBuf::from("a.md"))
        );
        assert_eq!(mount_relative(Path::new("../a.md"), Path::new(".")), None);
        assert_eq!(mount_relative(Path::new("a.md"), Path::new("skills")), None);
    }

    #[test]
    fn a_file_resolves_through_the_callers_directory_first() {
        let dir = tempfile::tempdir().expect("dir");
        let caller = dir.path().join("space");
        std::fs::create_dir(&caller).expect("space");
        std::fs::write(caller.join("brief.md"), "x").expect("brief");
        let files = PromptFiles::new(dir.path().to_path_buf(), []);
        let resolved =
            futures_lite::future::block_on(files.resolve(Path::new("brief.md"), Some(&caller)))
                .expect("resolves");
        assert_eq!(resolved, caller.join("brief.md"));
        let missing =
            futures_lite::future::block_on(files.resolve(Path::new("../brief.md"), Some(&caller)))
                .expect_err("escapes");
        assert!(missing.to_string().contains("forbidden path components"));
    }
}
