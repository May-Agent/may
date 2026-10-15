//! Shared JSON credential-file storage for OAuth providers.
//!
//! Each provider stores its token material under `~/.may/`; this module
//! carries the file I/O once so provider modules only declare a file name
//! and a serde shape.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Error type for credential-file operations.
#[derive(Debug, thiserror::Error)]
pub enum AuthStoreError {
    /// Failed to read the credential file.
    #[error("Failed to read credential file: {0}")]
    Read(std::io::Error),
    /// Failed to write the credential file.
    #[error("Failed to write credential file: {0}")]
    Write(std::io::Error),
    /// Failed to parse the credential file.
    #[error("Failed to parse credential file: {0}")]
    Parse(#[from] serde_json::Error),
    /// Failed to resolve the May config directory.
    #[error("Failed to get May directory: {0}")]
    MayDir(#[from] crate::config::ConfigError),
}

/// Load a JSON file, returning `Ok(None)` if it does not exist.
pub(crate) async fn load_json<T: DeserializeOwned>(
    path: &Path,
) -> Result<Option<T>, AuthStoreError> {
    match async_fs::read_to_string(path).await {
        Ok(content) => Ok(Some(serde_json::from_str(&content)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AuthStoreError::Read(e)),
    }
}

/// Save a value as a JSON file, creating parent directories if needed.
pub(crate) async fn save_json<T: Serialize + Sync>(
    path: &Path,
    value: &T,
) -> Result<(), AuthStoreError> {
    if let Some(parent) = path.parent() {
        async_fs::create_dir_all(parent)
            .await
            .map_err(AuthStoreError::Write)?;
    }
    let content = serde_json::to_string_pretty(value)?;
    async_fs::write(path, content)
        .await
        .map_err(AuthStoreError::Write)
}

/// Delete a file if it exists.
pub(crate) async fn delete_file(path: &Path) -> Result<(), AuthStoreError> {
    match async_fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(AuthStoreError::Write(e)),
    }
}
