//! The writable directories a sandboxed environment points its temporary,
//! home, cache and config variables at.
//!
//! A session's commands get them inside the session's working directory; a
//! subagent's commands get the same layout inside the subagent's own space,
//! because the session's are only readable to it. The layout lives here
//! once, for both.

use std::path::PathBuf;

/// The writable per-environment directories under one root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritableDirs {
    root: PathBuf,
}

impl WritableDirs {
    /// The layout under `root`, a directory the environment can write.
    #[must_use]
    pub fn under(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The temporary directory.
    #[must_use]
    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// The cache directory.
    #[must_use]
    pub fn cache(&self) -> PathBuf {
        self.root.join(".cache")
    }

    /// The config directory.
    #[must_use]
    pub fn config(&self) -> PathBuf {
        self.root.join(".config")
    }

    /// Each environment variable the layout sets, with the directory it names.
    #[must_use]
    pub fn variables(&self) -> [(&'static str, PathBuf); 7] {
        let tmp = self.tmp();
        let cache = self.cache();
        [
            ("TMPDIR", tmp.clone()),
            ("TMP", tmp.clone()),
            ("TEMP", tmp),
            ("HOME", self.root.clone()),
            ("XDG_CACHE_HOME", cache.clone()),
            ("XDG_CONFIG_HOME", self.config()),
            ("BUN_INSTALL_CACHE_DIR", cache.join("bun")),
        ]
    }

    /// Creates every directory of the layout.
    ///
    /// # Errors
    /// Fails when a directory cannot be created.
    pub async fn create(&self) -> std::io::Result<()> {
        let mut created: Vec<PathBuf> = Vec::new();
        for (_, directory) in self.variables() {
            if !created.contains(&directory) {
                async_fs::create_dir_all(&directory).await?;
                created.push(directory);
            }
        }
        Ok(())
    }
}
