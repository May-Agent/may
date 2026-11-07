//! A checkout's work in progress as one commit.
//!
//! The session works on top of the checkout as it stands: every uncommitted
//! change and every untracked, unignored file is copied into the fresh clone
//! and committed there as the clone's base. Runs on the host while the clone
//! is still host-owned, so no configuration the agent writes takes part.

use std::path::Path;

use super::RepoCloneError;
use crate::host_git::HostGit;

/// Commit the work in progress of `checkout` onto `clone`'s HEAD and return
/// the new commit; `None` when the checkout has none.
pub(super) fn commit_onto(
    host: &HostGit,
    checkout: &Path,
    clone: &Path,
) -> Result<Option<String>, RepoCloneError> {
    let changed = nul_separated(&host.output(
        Some(checkout),
        &["diff", "--name-only", "-z", "--no-renames", "HEAD"],
    )?);
    let untracked = nul_separated(&host.output(
        Some(checkout),
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )?);
    if changed.is_empty() && untracked.is_empty() {
        return Ok(None);
    }
    for path in changed.iter().chain(&untracked) {
        carry(checkout, clone, Path::new(path))?;
    }
    host.output(Some(clone), &["add", "--all"])?;
    let name = host
        .config_value("user.name")?
        .unwrap_or_else(|| "May".to_string());
    let email = host
        .config_value("user.email")?
        .unwrap_or_else(|| "may@localhost".to_string());
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

/// The entries of a `-z` listing.
fn nul_separated(output: &str) -> Vec<String> {
    output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect()
}

/// Make `relative` in `clone` match `checkout`: copy the file (or symlink)
/// there, or remove it when the checkout deleted it.
fn carry(checkout: &Path, clone: &Path, relative: &Path) -> Result<(), RepoCloneError> {
    let from = checkout.join(relative);
    let to = clone.join(relative);
    match std::fs::symlink_metadata(&from) {
        Ok(meta) => {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match std::fs::symlink_metadata(&to) {
                Ok(_) => std::fs::remove_file(&to)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            if meta.file_type().is_symlink() {
                std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)?;
            } else {
                std::fs::copy(&from, &to)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::remove_file(&to) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
