//! Resolving the prompt file a call names: the paths a subagent's caller
//! writes are virtual, so each is mapped onto the host directory it really
//! refers to without letting one escape its root.
//!
//! A container session sees no host paths at all: the session directory is
//! mounted at a fixed container root (`/workspace`), so an absolute path a
//! call names is a container path that maps through that root, never a host
//! file to open.

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
    /// The absolute container path `base_dir` is mounted at, for sessions
    /// whose commands run inside a container; `None` on the host.
    container_root: Option<PathBuf>,
}

fn strip_curdir(path: &Path) -> PathBuf {
    path.components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect()
}

/// `path` escapes its own path: a `..` or drive prefix it cannot be taken
/// below anything. `strip_prefix` itself refuses a mismatched root, so a
/// leading `/` is no escape — container paths are absolute by nature.
fn escapes(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
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
            container_root: None,
        }
    }

    /// Resolves container paths: `base_dir` is mounted at `container_root`
    /// (`/workspace` in a container session), so absolute paths a call names
    /// are container paths translated through it, and a caller working
    /// directory inside it maps onto the host tree.
    #[must_use]
    pub fn with_container_root(mut self, container_root: impl Into<PathBuf>) -> Self {
        self.container_root = Some(container_root.into());
        self
    }

    /// The host directory `caller` names, when it maps into the host tree.
    ///
    /// On the host the caller's directory is used as it stands. In a
    /// container session it is a container path: below `container_root` it
    /// maps onto `base_dir`; anywhere else it names nothing the host can
    /// see, so no mount is added for it.
    fn host_caller(&self, caller: &Path) -> Option<PathBuf> {
        match &self.container_root {
            None => Some(caller.to_path_buf()),
            Some(root) => {
                let base = self.base_dir.as_deref()?;
                let relative = strip_curdir(caller.strip_prefix(root).ok()?);
                Some(if relative.as_os_str().is_empty() {
                    base.to_path_buf()
                } else {
                    base.join(relative)
                })
            }
        }
    }

    /// The mounts for a call made from `caller`: its own directory first,
    /// then the configured ones.
    fn effective(&self, caller: Option<&Path>) -> Vec<SubagentFileMount> {
        let caller = caller
            .and_then(|dir| self.host_caller(dir))
            .filter(|dir| self.base_dir.as_deref() != Some(dir.as_path()))
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

    /// The canonical host root a candidate must stay under: `mount`'s host
    /// root resolved, so a symlink inside it cannot reach outside it.
    async fn canonical_root(mount: &SubagentFileMount) -> anyhow::Result<PathBuf> {
        async_fs::canonicalize(&mount.host_root)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "failed to resolve mounted path '{}': {error}",
                    mount.host_root.display()
                )
            })
    }

    /// `candidate` canonicalized under `mount`'s canonical root; `None` when
    /// the file does not exist or its canonical form escapes the root.
    async fn canonical_candidate(
        candidate: &Path,
        canonical_root: &Path,
    ) -> anyhow::Result<Option<PathBuf>> {
        match async_fs::canonicalize(candidate).await {
            Ok(canonical) if canonical.starts_with(canonical_root) => Ok(Some(canonical)),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(anyhow::anyhow!(
                "failed to resolve mounted file '{}': {error}",
                candidate.display()
            )),
        }
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
            let canonical_root = Self::canonical_root(mount).await?;
            if let Some(canonical) = Self::canonical_candidate(&candidate, &canonical_root).await? {
                return Ok(canonical);
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
        if let Some(root) = &self.container_root {
            if file.is_absolute() {
                // A container session has no host-absolute paths: the file a
                // call names absolutely is a container path, served only when
                // it sits under the mounted workspace.
                let Some(relative) = mount_relative(file, root) else {
                    anyhow::bail!(
                        "Subagent file '{}' is outside the mounted workspace",
                        file.display()
                    );
                };
                return Self::through_mounts(file, &relative, &mounts).await;
            }
            return Self::through_mounts(file, file, &mounts).await;
        }
        if file.is_absolute() {
            if let Some(base) = &self.base_dir
                && let Ok(stripped) = file.strip_prefix(base)
            {
                let relative = strip_curdir(stripped);
                if !relative.as_os_str().is_empty() {
                    return Self::through_mounts(&relative, &relative, &mounts).await;
                }
            }
            // An absolute path a call names resolves through the mounts:
            // the file exists on the host but outside a mounted root, it
            // discloses nothing.
            return Self::through_mounts(file, file, &mounts).await;
        }
        Self::through_mounts(file, file, &mounts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTAINER_WORKSPACE: &str = "/workspace";

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
        assert_eq!(
            resolved,
            dunce::canonicalize(caller.join("brief.md")).unwrap()
        );
        let missing =
            futures_lite::future::block_on(files.resolve(Path::new("../brief.md"), Some(&caller)))
                .expect_err("escapes");
        assert!(missing.to_string().contains("forbidden path components"));
    }

    /// A container caller's directory under the workspace maps onto the
    /// session directory; one outside it names no host directory at all.
    #[test]
    fn a_container_caller_maps_below_the_workspace_and_nowhere_else() {
        let base = PathBuf::from("/host/session");
        let files = PromptFiles::new(base.clone(), []).with_container_root(CONTAINER_WORKSPACE);
        assert_eq!(
            files.host_caller(Path::new("/workspace/sub")),
            Some(base.join("sub"))
        );
        assert_eq!(files.host_caller(Path::new("/workspace")), Some(base));
        assert_eq!(files.host_caller(Path::new("/tmp")), None);
        assert_eq!(files.host_caller(Path::new("/etc")), None);
    }

    /// A container call naming a host-absolute path gets nothing: absolute
    /// paths resolve as container paths under the workspace mount only.
    #[test]
    fn a_container_call_cannot_name_a_host_absolute_path() {
        let dir = tempfile::tempdir().expect("dir");
        let host_secret = dir.path().join("secret.md");
        std::fs::write(&host_secret, "top secret").expect("secret");
        std::fs::write(dir.path().join("brief.md"), "the brief").expect("brief");
        let files =
            PromptFiles::new(dir.path().to_path_buf(), []).with_container_root(CONTAINER_WORKSPACE);

        let refused = futures_lite::future::block_on(
            files.resolve(&host_secret, Some(Path::new("/workspace"))),
        )
        .expect_err("a host-absolute path must not resolve in a container session");
        assert!(
            refused
                .to_string()
                .contains("outside the mounted workspace"),
            "{refused}"
        );

        let resolved = futures_lite::future::block_on(files.resolve(
            Path::new("/workspace/brief.md"),
            Some(Path::new("/workspace")),
        ))
        .expect("a workspace path maps to the host file");
        assert_eq!(
            resolved,
            dunce::canonicalize(dir.path().join("brief.md")).unwrap()
        );
    }

    /// A symlink inside a mounted directory pointing outside it resolves to
    /// nothing: the canonical candidate must stay under the canonical root.
    #[cfg(unix)]
    #[test]
    fn a_symlink_escaping_its_mount_resolves_nothing() {
        let dir = tempfile::tempdir().expect("dir");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.md"), "top secret").expect("secret");
        std::os::unix::fs::symlink(
            outside.path().join("secret.md"),
            dir.path().join("linked.md"),
        )
        .expect("symlink");
        let files = PromptFiles::new(dir.path().to_path_buf(), []);

        let error = futures_lite::future::block_on(files.resolve(Path::new("linked.md"), None))
            .expect_err("an escaping symlink must not resolve");
        assert!(error.to_string().contains("not found"), "{error}");
    }
}
