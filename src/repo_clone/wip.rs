//! A checkout's work in progress as one commit.
//!
//! The session works on top of the checkout as it stands: every uncommitted
//! change and every untracked, unignored file is copied into the fresh clone
//! and committed there as the clone's base. A submodule whose commit moved is
//! carried as its gitlink, and an untracked nested repository as its whole
//! directory. Runs on the host while the clone is still host-owned, so no
//! configuration the agent writes takes part.

use std::path::Path;

use super::RepoCloneError;
use crate::host_git::HostGit;

/// Commit the work in progress of `checkout` onto `clone`'s HEAD and return
/// the new commit; `None` when the checkout has none.
///
/// # Errors
/// Fails when git or the filesystem fails, or the user has no git identity to
/// commit as.
pub(super) fn commit_onto(
    host: &HostGit,
    checkout: &Path,
    clone: &Path,
) -> Result<Option<String>, RepoCloneError> {
    let changed = Change::listing(&host.output(
        Some(checkout),
        &["diff", "--raw", "-z", "--no-renames", "HEAD"],
    )?);
    let untracked = nul_separated(&host.output(
        Some(checkout),
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )?);
    if changed.is_empty() && untracked.is_empty() {
        return Ok(None);
    }
    for change in &changed {
        if change.gitlink {
            carry_gitlink(host, checkout, clone, Path::new(&change.path))?;
        } else {
            carry(checkout, clone, Path::new(&change.path))?;
        }
    }
    for path in &untracked {
        carry(checkout, clone, Path::new(path))?;
    }
    host.output(Some(clone), &["add", "--all"])?;
    let name = host
        .config_value("user.name")?
        .ok_or(RepoCloneError::GitIdentity)?;
    let email = host
        .config_value("user.email")?
        .ok_or(RepoCloneError::GitIdentity)?;
    let (name, email) = (format!("user.name={name}"), format!("user.email={email}"));
    host.output(
        Some(clone),
        &[
            "-c",
            &name,
            "-c",
            &email,
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--no-verify",
            "--message",
            "Work in progress from the checkout",
        ],
    )?;
    Ok(Some(
        host.output(Some(clone), &["rev-parse", "HEAD"])?
            .trim()
            .to_string(),
    ))
}

/// One path of `git diff --raw -z`: `:<old mode> <new mode> <old id> <new id>
/// <status>` followed by the path.
struct Change {
    path: String,
    /// The path is a submodule (mode 160000 before or after).
    gitlink: bool,
}

impl Change {
    fn listing(output: &str) -> Vec<Self> {
        let mut fields = output.split('\0').filter(|field| !field.is_empty());
        let mut changes = Vec::new();
        while let Some(meta) = fields.next() {
            let path = fields
                .next()
                .unwrap_or_else(|| panic!("git diff --raw -z entry {meta:?} has no path"));
            let mut modes = meta.trim_start_matches(':').split(' ');
            let (old, new) = (modes.next(), modes.next());
            changes.push(Self {
                path: path.to_string(),
                gitlink: old == Some("160000") || new == Some("160000"),
            });
        }
        changes
    }
}

/// The entries of a `-z` listing.
fn nul_separated(output: &str) -> Vec<String> {
    output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect()
}

/// Make the submodule `relative` in `clone` point at the commit the
/// checkout's submodule has checked out, or remove it when the checkout
/// dropped it. The clone holds no submodule content, so only the pointer
/// travels.
fn carry_gitlink(
    host: &HostGit,
    checkout: &Path,
    clone: &Path,
    relative: &Path,
) -> Result<(), RepoCloneError> {
    let from = checkout.join(relative);
    let to = clone.join(relative);
    if from.join(".git").exists() {
        let commit = host.output(Some(&from), &["rev-parse", "HEAD"])?;
        // An uninitialized submodule is an empty directory.
        std::fs::create_dir_all(&to)?;
        host.output(
            Some(clone),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{},{}", commit.trim(), relative.display()),
            ],
        )?;
    } else {
        host.output(
            Some(clone),
            &[
                "rm",
                "--cached",
                "--quiet",
                "--ignore-unmatch",
                "--",
                &relative.to_string_lossy(),
            ],
        )?;
    }
    Ok(())
}

/// Make `relative` in `clone` match `checkout`: copy the file, symlink or
/// directory (recursively) there, or remove it when the checkout deleted it.
fn carry(checkout: &Path, clone: &Path, relative: &Path) -> Result<(), RepoCloneError> {
    let from = checkout.join(relative);
    let to = clone.join(relative);
    match std::fs::symlink_metadata(&from) {
        Ok(_) => {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            remove_entry(&to)?;
            copy_entry(&from, &to)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => remove_entry(&to)?,
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Remove whatever is at `path`; nothing there is fine.
fn remove_entry(path: &Path) -> Result<(), RepoCloneError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Copy the file, symlink or directory tree at `from` to `to`, never
/// following a symlink.
fn copy_entry(from: &Path, to: &Path) -> Result<(), RepoCloneError> {
    let file_type = std::fs::symlink_metadata(from)?.file_type();
    if file_type.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)?;
    } else if file_type.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_entry(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(from, to)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(dir: &Path) -> HostGit {
        let config = dir.join("gitconfig");
        std::fs::write(
            &config,
            "[user]\n\tname = t\n\temail = t@t\n[protocol \"file\"]\n\tallow = always\n",
        )
        .unwrap();
        HostGit::from_file(config)
    }

    fn commit_all(host: &HostGit, dir: &Path, message: &str) {
        host.output(Some(dir), &["add", "--all"]).unwrap();
        host.output(
            Some(dir),
            &["commit", "--quiet", "--no-verify", "-m", message],
        )
        .unwrap();
    }

    fn repo(host: &HostGit, dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        host.output(Some(dir), &["init", "--quiet", "-b", "main"])
            .unwrap();
    }

    #[test]
    fn a_moved_submodule_travels_as_its_gitlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let host = identity(&root);
        let sub = root.join("sub");
        repo(&host, &sub);
        std::fs::write(sub.join("a"), "a").unwrap();
        commit_all(&host, &sub, "a");
        let checkout = root.join("checkout");
        repo(&host, &checkout);
        host.output(
            Some(&checkout),
            &["submodule", "add", "--quiet", sub.to_str().unwrap(), "s"],
        )
        .unwrap();
        commit_all(&host, &checkout, "with a submodule");
        let clone = root.join("clone");
        host.output(
            None,
            &[
                "clone",
                "--quiet",
                checkout.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        )
        .unwrap();
        // The submodule moves on in the checkout, uncommitted there.
        std::fs::write(sub.join("b"), "b").unwrap();
        commit_all(&host, &sub, "b");
        host.output(Some(&checkout.join("s")), &["pull", "--quiet"])
            .unwrap();
        let moved = host
            .output(Some(&checkout.join("s")), &["rev-parse", "HEAD"])
            .unwrap();

        let base = commit_onto(&host, &checkout, &clone).unwrap().unwrap();

        let entry = host.output(Some(&clone), &["ls-tree", &base, "s"]).unwrap();
        assert_eq!(entry.trim(), format!("160000 commit {}\ts", moved.trim()));
    }

    #[test]
    fn an_untracked_nested_repository_is_copied_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let host = identity(&root);
        let checkout = root.join("checkout");
        repo(&host, &checkout);
        std::fs::write(checkout.join("a"), "a").unwrap();
        commit_all(&host, &checkout, "a");
        let clone = root.join("clone");
        host.output(
            None,
            &[
                "clone",
                "--quiet",
                checkout.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        )
        .unwrap();
        let nested = checkout.join("tools").join("nested");
        repo(&host, &nested);
        std::fs::write(nested.join("f.txt"), "nested").unwrap();
        commit_all(&host, &nested, "n");
        std::fs::create_dir(nested.join("dir")).unwrap();
        std::fs::write(nested.join("dir").join("g.txt"), "deep").unwrap();

        let base = commit_onto(&host, &checkout, &clone).unwrap();

        assert!(base.is_some());
        assert_eq!(
            std::fs::read_to_string(clone.join("tools/nested/dir/g.txt")).unwrap(),
            "deep"
        );
        assert!(clone.join("tools/nested/.git").exists());
    }

    #[test]
    fn no_git_identity_fails_with_a_message_naming_the_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let setup = identity(&root);
        let checkout = root.join("checkout");
        repo(&setup, &checkout);
        std::fs::write(checkout.join("a"), "a").unwrap();
        commit_all(&setup, &checkout, "a");
        let clone = root.join("clone");
        repo(&setup, &clone);
        std::fs::write(checkout.join("b"), "b").unwrap();

        let anonymous = HostGit::from_file(root.join("empty-config"));
        let error = commit_onto(&anonymous, &checkout, &clone).unwrap_err();

        let message = error.to_string();
        assert!(
            message.contains("user.name") && message.contains("user.email"),
            "{message}"
        );
    }
}
