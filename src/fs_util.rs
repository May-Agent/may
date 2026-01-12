use std::path::Path;

use async_fs;

pub async fn read_to_string(path: &Path) -> Result<String, std::io::Error> {
    async_fs::read_to_string(path).await
}
