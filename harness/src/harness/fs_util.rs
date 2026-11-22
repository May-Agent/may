use async_fs as fs;
use std::path::Path;

/// Whether `path` exists.
///
/// # Errors
/// Fails when the filesystem cannot answer, other than for a missing path.
pub async fn path_exists(path: &Path) -> std::io::Result<bool> {
    match fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}
