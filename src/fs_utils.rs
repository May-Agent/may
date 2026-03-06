use std::path::Path;

use fs_extra::dir::CopyOptions;

pub fn copy_dir_contents_recursive(source: &Path, target: &Path) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(target)?;

    let mut options = CopyOptions::new();
    options.copy_inside = true;
    options.content_only = true;

    fs_extra::dir::copy(source, target, &options)
        .map(|_| ())
        .map_err(|error| std::io::Error::other(error.to_string()))
}
