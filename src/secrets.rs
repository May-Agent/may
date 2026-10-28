//! User secrets: keychain-backed storage, transcript masking, and shell
//! environment injection.
//!
//! `/secret KEY VALUE` stores a value in the OS keychain under the `may`
//! service; a names index lives at `~/.may/secrets.json` so stored keys can
//! be enumerated (keychain APIs cannot list entries). Values are injected
//! into shells may spawns — bash-mode commands directly as env vars, the
//! agent's terminal via a `.may-secrets` env file written into the session
//! working directory — and masked out of every transcript-facing string.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Keychain service secrets are stored under.
const SERVICE: &str = "dev.may.secrets";

/// File name of the env file written into a session's working directory.
pub const SECRETS_ENV_FILE: &str = ".may-secrets";

/// Warning shown when a secret is detected in bash-mode input or output.
pub const SECRET_WARNING: &str = "Secret detected — masked from transcript. Please don't type secrets in bash mode; use /secret KEY VALUE to store them in the keychain.";

/// Errors from secret storage and masking.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// Keychain operation failed.
    #[error("keychain error: {0}")]
    Keychain(#[from] keyring::Error),
    /// Index file could not be read or written.
    #[error("secret index I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Index file contents were malformed.
    #[error("secret index parse error: {0}")]
    Parse(#[from] serde_json::Error),
    /// The key name is not a valid environment variable name.
    #[error("invalid secret name {0:?}: use [A-Za-z_][A-Za-z0-9_]*")]
    InvalidName(String),
}

/// Keychain access plus the names index under `~/.may`.
#[derive(Debug)]
pub struct SecretStore {
    index_path: PathBuf,
}

impl SecretStore {
    /// Store rooted at `may_dir` (`~/.may`).
    #[must_use]
    pub fn new(may_dir: &Path) -> Self {
        Self {
            index_path: may_dir.join("secrets.json"),
        }
    }

    /// Env-var-safe secret name?
    fn validate_name(key: &str) -> Result<&str, SecretError> {
        let valid = !key.is_empty()
            && key
                .chars()
                .enumerate()
                .all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()));
        if valid {
            Ok(key)
        } else {
            Err(SecretError::InvalidName(key.to_string()))
        }
    }

    /// Read the names index; an absent file means no stored secrets.
    fn names(&self) -> Vec<String> {
        #[derive(Deserialize)]
        struct Index {
            #[serde(default)]
            names: Vec<String>,
        }
        std::fs::read_to_string(&self.index_path)
            .ok()
            .and_then(|content| serde_json::from_str::<Index>(&content).ok())
            .map(|index| index.names)
            .unwrap_or_default()
    }

    fn write_names(&self, names: &[String]) -> Result<(), SecretError> {
        #[derive(Serialize)]
        struct Index<'a> {
            names: &'a [String],
        }
        if let Some(parent) = self.index_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(&Index { names })?;
        std::fs::write(&self.index_path, content)?;
        Ok(())
    }

    fn entry(key: &str) -> Result<keyring::Entry, SecretError> {
        Ok(keyring::Entry::new(SERVICE, key)?)
    }

    /// Store or update `key = value` in the keychain and index the name.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when the name is not env-var-safe or the
    /// keychain/index write fails.
    pub fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        Self::entry(Self::validate_name(key)?)?.set_password(value)?;
        let mut names = self.names();
        if !names.iter().any(|name| name == key) {
            names.push(key.to_string());
            names.sort();
            self.write_names(&names)?;
        }
        Ok(())
    }

    /// Remove `key` from the keychain and index. Returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when the keychain delete or index write fails.
    pub fn remove(&self, key: &str) -> Result<bool, SecretError> {
        let existed = match Self::entry(key)?.delete_credential() {
            Ok(()) => true,
            Err(keyring::Error::NoEntry) => false,
            Err(error) => return Err(error.into()),
        };
        if existed {
            let names: Vec<String> = self
                .names()
                .into_iter()
                .filter(|name| name != key)
                .collect();
            self.write_names(&names)?;
        }
        Ok(existed)
    }

    /// All `(name, value)` pairs; names whose keychain read fails are skipped.
    #[must_use]
    pub fn values(&self) -> Vec<(String, String)> {
        self.names()
            .into_iter()
            .filter_map(|name| {
                let entry = Self::entry(&name).ok()?;
                entry
                    .get_password()
                    .map_err(|error| {
                        tracing::warn!(secret = %name, %error, "failed to read secret from keychain");
                        error
                    })
                    .ok()
                    .map(|value| (name, value))
            })
            .collect()
    }
}

/// A loaded snapshot of maskable secret values.
///
/// Carries keychain secrets (also used for env injection) plus provider API
/// keys from the config (masking only — they are not env-scoped names and
/// must not be re-injected).
#[derive(Clone)]
pub struct Secrets {
    /// Keychain `(name, value)` pairs — env-injectable.
    entries: Vec<(String, String)>,
    /// Extra values masked but never injected (provider API keys).
    extra_values: Vec<String>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("entries", &self.entries.len())
            .field("extra_values", &self.extra_values.len())
            .finish()
    }
}

impl Secrets {
    /// An empty snapshot.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            entries: Vec::new(),
            extra_values: Vec::new(),
        }
    }

    /// Load keychain values and configured provider API keys.
    #[must_use]
    pub fn load(store: &SecretStore, config: &crate::config::MayConfig) -> Self {
        let extra_values = config
            .provider
            .values()
            .filter_map(|instance| instance.api_key.clone())
            .filter(|key| !key.is_empty())
            .collect();
        Self {
            entries: store.values(),
            extra_values,
        }
    }

    /// `(name, value)` pairs safe to inject as environment variables.
    pub fn env_pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Stored env-var names — for context rendering, never carries values.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// Mask every detectable secret in `text` with `***`.
    ///
    /// Replaces stored keychain values and configured provider keys
    /// verbatim, then literal tokens that look like secrets (`NAME=value`
    /// assignments on secret-named variables, well-known token formats).
    /// Returns the masked text and whether anything was redacted.
    #[must_use]
    pub fn mask(&self, text: &str) -> (String, bool) {
        let mut masked = text.to_string();
        // Longest-first so overlapping values (prefix of another) redact cleanly.
        let mut values: Vec<&str> = self
            .entries
            .iter()
            .map(|(_, value)| value.as_str())
            .chain(self.extra_values.iter().map(String::as_str))
            .filter(|value| value.len() >= 6)
            .collect();
        values.sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));
        for value in values {
            masked = masked.replace(value, "***");
        }
        for pattern in literal_patterns() {
            masked = pattern.replace_all(&masked, "${name}***").into_owned();
        }
        for pattern in token_patterns() {
            masked = pattern.replace_all(&masked, "***").into_owned();
        }
        let changed = masked != text;
        (masked, changed)
    }
}

/// `SECRET_NAMED=value` style assignments — mask the value side.
fn literal_patterns() -> &'static [regex::Regex] {
    static PATTERNS: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        // FOO_KEY=..., API_TOKEN: ..., --password value, etc. Value must
        // be >= 8 chars to keep ordinary config text untouched.
        vec![regex::Regex::new(
            r#"(?i)(?P<name>[a-z_]*(?:api[_-]?key|secret|token|password|passwd|credential|private[_-]?key)[a-z0-9_]*[=:]\s*["']?)[^\s"']{8,}"#,
        )
        .expect("secret literal pattern compiles")]
    })
}

/// Well-known secret token shapes masked wholesale.
fn token_patterns() -> &'static [regex::Regex] {
    static PATTERNS: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"sk-(?:ant-|live-|test-)?[A-Za-z0-9_-]{16,}",
            r"(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}",
            r"github_pat_[A-Za-z0-9_]{20,}",
            r"xox[baprs]-[A-Za-z0-9-]{10,}",
            r"AKIA[0-9A-Z]{16}",
            r"AIza[0-9A-Za-z_-]{30,}",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[^-]*-----END [A-Z ]*PRIVATE KEY-----",
        ]
        .iter()
        .map(|pattern| regex::Regex::new(pattern).expect("secret token pattern compiles"))
        .collect()
    })
}

/// Render `export NAME='<value>'` lines the session shell can source.
#[must_use]
pub fn render_env_file(secrets: &Secrets) -> String {
    let mut content = String::from(
        "# Provisioned by `may /secret` — source this file to load secrets\n\
         # as environment variables. Never print its contents.\n",
    );
    for (name, value) in secrets.env_pairs() {
        content.push_str("export ");
        content.push_str(name);
        content.push('=');
        content.push_str(&shell_words::quote(value));
        content.push('\n');
    }
    content
}

/// Write the secrets env file with owner-only permissions; removes a stale
/// file when no secrets remain.
///
/// # Errors
///
/// Returns `std::io::Error` when the file cannot be created or written.
pub async fn write_env_file(path: &Path, secrets: &Secrets) -> std::io::Result<()> {
    use std::io::Write;

    if secrets.env_pairs().next().is_none() {
        return remove_env_file(path).await;
    }
    let content = render_env_file(secrets);
    let path = path.to_path_buf();
    blocking::unblock(move || {
        let file = std::fs::File::create(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let mut file = file;
        file.write_all(content.as_bytes())
    })
    .await
}

/// Remove the env file, ignoring a missing one.
///
/// # Errors
///
/// Returns `std::io::Error` when the file exists but cannot be removed.
pub async fn remove_env_file(path: &Path) -> std::io::Result<()> {
    match async_fs::remove_file(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation() {
        assert!(SecretStore::validate_name("ANTHROPIC_API_KEY").is_ok());
        assert!(SecretStore::validate_name("_PRIVATE").is_ok());
        assert!(SecretStore::validate_name("K9").is_ok());
        assert!(SecretStore::validate_name("9K").is_err());
        assert!(SecretStore::validate_name("A-B").is_err());
        assert!(SecretStore::validate_name("").is_err());
    }

    #[test]
    fn mask_replaces_known_values() {
        let secrets = Secrets {
            entries: vec![("MY_TOKEN".to_string(), "supersecretvalue".to_string())],
            extra_values: Vec::new(),
        };
        let (masked, hit) = secrets.mask("echo supersecretvalue && echo hi");
        assert!(hit);
        assert_eq!(masked, "echo *** && echo hi");
        let (_, hit) = secrets.mask("nothing here");
        assert!(!hit);
    }

    #[test]
    fn mask_catches_token_shapes_and_assignments() {
        let secrets = Secrets::empty();
        let (masked, hit) = secrets.mask("curl -H 'key: ghp_abcdefghijklmnopqrst'");
        assert!(hit);
        assert!(!masked.contains("ghp_"));
        let (masked, hit) = secrets.mask("export MY_SECRET=hunter2hunter2");
        assert!(hit);
        assert_eq!(masked, "export MY_SECRET=***");
        let (_, hit) = secrets.mask("set name=lexo debug=1");
        assert!(!hit);
    }

    #[test]
    fn env_file_renders_escaped_exports() {
        let secrets = Secrets {
            entries: vec![
                ("A".to_string(), "one".to_string()),
                ("B".to_string(), "two 'quoted'".to_string()),
            ],
            extra_values: Vec::new(),
        };
        let content = render_env_file(&secrets);
        assert!(content.contains("export A=one"));
        assert!(content.contains("export B='two '\\''quoted'\\'''"));
    }
}
