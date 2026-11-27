//! Small filesystem helpers shared across the crate.
//!
//! Prefer adding to this module only functions that carry nontrivial
//! invariants worth stating once; trivial `std::fs` / `async_fs` calls should
//! stay inline at their call sites.

use std::path::Path;

/// Render `path` for model-facing text with the host home shown as `~`.
///
/// Commands run in POSIX sh where `~` expands to the same home, so the
/// compressed form is usable verbatim while costing fewer tokens. Paths
/// outside the home render unchanged.
#[must_use]
pub fn display_path(path: &Path) -> String {
    let Ok(home) = crate::config::MayConfig::home_dir() else {
        return path.display().to_string();
    };
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.to_string_lossy().replace('\\', "/")),
        Err(_) => path.display().to_string(),
    }
}

/// Recursively copies every entry inside `source` into `target`.
///
/// `target` is created if it doesn't exist and existing files whose contents
/// differ are overwritten; files that already match `source` byte-for-byte are
/// left untouched. `target` itself is the destination root — no basename of
/// `source` is added, so copying `./a/` into `./b/` places the contents of
/// `a` at the top level of `b`.
///
/// This is a synchronous (blocking) operation; callers running on an
/// async runtime should wrap it with `blocking::unblock` or equivalent.
///
/// # Errors
///
/// Returns the underlying IO error.
#[allow(
    clippy::disallowed_methods,
    reason = "explicitly documented synchronous copy helper for blocking::unblock"
)]
pub fn copy_dir_contents_recursive(source: &Path, target: &Path) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(target)?;

    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = target.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_contents_recursive(&src_path, &dst_path)?;
        } else if file_type.is_symlink() {
            let link_target = std::fs::read_link(&src_path)?;
            if let Ok(dst_meta) = std::fs::symlink_metadata(&dst_path) {
                if dst_meta.file_type().is_symlink()
                    && std::fs::read_link(&dst_path).ok().as_deref() == Some(link_target.as_path())
                {
                    continue;
                }
                if dst_meta.is_dir() {
                    std::fs::remove_dir_all(&dst_path)?;
                } else {
                    std::fs::remove_file(&dst_path)?;
                }
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link_target, &dst_path)?;
            #[cfg(windows)]
            if src_path.is_dir() {
                std::os::windows::fs::symlink_dir(&link_target, &dst_path)?;
            } else {
                std::os::windows::fs::symlink_file(&link_target, &dst_path)?;
            }
        } else {
            let src_meta = entry.metadata()?;
            if let Ok(dst_meta) = std::fs::symlink_metadata(&dst_path) {
                if dst_meta.file_type().is_file()
                    && dst_meta.len() == src_meta.len()
                    && files_equal(&src_path, &dst_path)?
                {
                    continue;
                }
                if dst_meta.is_dir() {
                    std::fs::remove_dir_all(&dst_path)?;
                } else if dst_meta.file_type().is_symlink() {
                    std::fs::remove_file(&dst_path)?;
                }
            }
            std::fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

#[allow(
    clippy::disallowed_methods,
    reason = "synchronous comparison helper used inside copy_dir_contents_recursive"
)]
fn files_equal(left: &Path, right: &Path) -> Result<bool, std::io::Error> {
    use std::io::Read as _;

    let mut left_file = std::fs::File::open(left)?;
    let mut right_file = std::fs::File::open(right)?;
    let mut left_buf = [0u8; 8192];
    let mut right_buf = [0u8; 8192];
    loop {
        let n_left = left_file.read(&mut left_buf)?;
        let n_right = right_file.read(&mut right_buf)?;
        if n_left != n_right || left_buf[..n_left] != right_buf[..n_left] {
            return Ok(false);
        }
        if n_left == 0 {
            return Ok(true);
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn copy_dir_contents_recursive_skips_unchanged_and_overwrites_changed() {
        let src = tempfile::tempdir().expect("src");
        let dst = tempfile::tempdir().expect("dst");

        std::fs::create_dir_all(src.path().join("nested")).expect("nested dir");
        std::fs::write(src.path().join("same.txt"), "unchanged\n").expect("write same");
        std::fs::write(src.path().join("nested/change.txt"), "v1\n").expect("write v1");

        copy_dir_contents_recursive(src.path(), dst.path()).expect("first copy");
        assert_eq!(
            std::fs::read_to_string(dst.path().join("same.txt")).expect("read same"),
            "unchanged\n"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("nested/change.txt")).expect("read v1"),
            "v1\n"
        );

        let same_mtime_before = std::fs::metadata(dst.path().join("same.txt"))
            .expect("meta")
            .modified()
            .expect("mtime");
        std::fs::write(src.path().join("nested/change.txt"), "v2-updated\n").expect("write v2");

        copy_dir_contents_recursive(src.path(), dst.path()).expect("second copy");
        let same_mtime_after = std::fs::metadata(dst.path().join("same.txt"))
            .expect("meta after")
            .modified()
            .expect("mtime after");
        assert_eq!(
            same_mtime_before, same_mtime_after,
            "unchanged file should not be rewritten"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("nested/change.txt")).expect("read v2"),
            "v2-updated\n"
        );
    }
}
