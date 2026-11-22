//! The one install path both channels end in.

use std::path::Path;

use super::UpdateError;

/// Replaces the executable at `target` with `bytes`: written beside it and
/// renamed over it, so a running process keeps its old image.
pub(super) fn replace_executable(target: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    let directory = target
        .parent()
        .ok_or_else(|| std::io::Error::other("the executable has no directory"))?;
    let mut staged = tempfile::NamedTempFile::new_in(directory)?;
    staged.write_all(bytes)?;
    staged
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    staged.persist(target).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_swaps_the_file_and_makes_it_executable() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("a temp dir");
        let target = dir.path().join("may");
        std::fs::write(&target, b"old").expect("the old executable is written");
        replace_executable(&target, b"new").expect("the executable is replaced");
        assert_eq!(std::fs::read(&target).expect("it reads"), b"new");
        let mode = std::fs::metadata(&target)
            .expect("it has metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(std::fs::read_dir(dir.path()).expect("it lists").count(), 1);
    }
}
