//! Small filesystem helpers shared across the crate.
//!
//! Prefer adding to this module only functions that carry nontrivial
//! configuration (e.g. `fs_extra::dir::CopyOptions`) or invariants worth
//! stating once; trivial `std::fs` / `async_fs` calls should stay inline
//! at their call sites.

use std::path::Path;

use fs_extra::dir::CopyOptions;

/// Render `path` for model-facing text with the host home directory shown as
/// `~`. Commands run in POSIX sh where `~` expands to the same home, so the
/// compressed form is usable verbatim while costing fewer tokens. Paths
/// outside the home render unchanged.
#[must_use]
pub fn display_path(path: &Path) -> String {
    let Some(home) = dirs::home_dir() else {
        return path.display().to_string();
    };
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.to_string_lossy().replace('\\', "/")),
        Err(_) => path.display().to_string(),
    }
}

/// Recursively copies every entry inside `source` into `target`, creating
/// `target` if it doesn't exist and overwriting existing files with the
/// same name. `target` itself is the destination root — no basename of
/// `source` is added, so copying `./a/` into `./b/` places the contents
/// of `a` at the top level of `b`.
///
/// This is a synchronous (blocking) operation; callers running on an
/// async runtime should wrap it with `blocking::unblock` or equivalent.
pub fn copy_dir_contents_recursive(source: &Path, target: &Path) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(target)?;

    let options = CopyOptions {
        copy_inside: true,
        content_only: true,
        overwrite: true,
        ..CopyOptions::new()
    };

    fs_extra::dir::copy(source, target, &options)
        .map(|_| ())
        .map_err(|error| std::io::Error::other(error.to_string()))
}
