//! User secrets: keychain-backed storage, transcript masking, and shell
//! environment injection.
//!
//! `/secret KEY VALUE` stores a value in the OS keychain under the `may`
//! service; a names index lives at `~/.may/secrets.json` so stored keys can
//! be enumerated (keychain APIs cannot list entries). Values are injected
//! into shells may spawns — bash-mode commands directly as env vars, the
//! agent's terminal via a `.may-secrets` env file written into the session
//! working directory — and masked out of every transcript-facing string.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aither_core::llm::{
    CommandOutcome, CommandStream, LLMRequest, Message, ToolResult, tool::ToolResultPart,
};
use serde::{Deserialize, Serialize};

/// Keychain service secrets are stored under.
const SERVICE: &str = "dev.may.secrets";

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

/// Who can read a stored secret.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SecretScope {
    /// Every session's terminals, like a `/secret KEY VALUE` value.
    Global,
    /// Only the terminals of the session that asked for it; it is deleted
    /// with that session.
    Session,
}

impl SecretScope {
    /// The scope as the database stores it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Session => "session",
        }
    }

    /// The scope stored as `value`, or `None` when it names none.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "global" => Some(Self::Global),
            "session" => Some(Self::Session),
            _ => None,
        }
    }
}

/// The names index: the global names, and the names each session holds.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    names: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    sessions: BTreeMap<String, Vec<String>>,
}

/// Keychain access plus the names index under `~/.may`.
#[derive(Debug, Clone)]
pub struct SecretStore {
    index_path: PathBuf,
    /// Credentials tests keep in memory instead of the OS keychain.
    #[cfg(test)]
    memory: Option<std::sync::Arc<std::sync::Mutex<BTreeMap<String, String>>>>,
}

impl SecretStore {
    /// Store rooted at `may_dir` (`~/.may`).
    #[must_use]
    pub fn new(may_dir: &Path) -> Self {
        Self {
            index_path: may_dir.join("secrets.json"),
            #[cfg(test)]
            memory: None,
        }
    }

    /// Store rooted at `may_dir` whose credentials live in memory, shared by
    /// its clones, instead of the OS keychain.
    #[cfg(test)]
    pub(crate) fn in_memory(may_dir: &Path) -> Self {
        Self {
            memory: Some(std::sync::Arc::default()),
            ..Self::new(may_dir)
        }
    }

    /// Env-var-safe secret name?
    ///
    /// # Errors
    ///
    /// Returns [`SecretError::InvalidName`] when `key` is not a valid name.
    pub fn validate_name(key: &str) -> Result<&str, SecretError> {
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
    fn index(&self) -> Index {
        std::fs::read_to_string(&self.index_path)
            .ok()
            .and_then(|content| serde_json::from_str::<Index>(&content).ok())
            .unwrap_or_default()
    }

    fn write_index(&self, index: &Index) -> Result<(), SecretError> {
        if let Some(parent) = self.index_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(index)?;
        std::fs::write(&self.index_path, content)?;
        Ok(())
    }

    /// Keychain account of `key`: its own name when global, a name no
    /// environment variable can have when held by a session.
    fn account(session: Option<&str>, key: &str) -> String {
        session.map_or_else(|| key.to_string(), |id| format!("session:{id}:{key}"))
    }

    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn write_credential(&self, account: &str, value: &str) -> Result<(), SecretError> {
        #[cfg(test)]
        if let Some(memory) = &self.memory {
            memory
                .lock()
                .expect("test credential map is not poisoned")
                .insert(account.to_string(), value.to_string());
            return Ok(());
        }
        keyring::Entry::new(SERVICE, account)?.set_password(value)?;
        Ok(())
    }

    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn read_credential(&self, account: &str) -> Result<Option<String>, SecretError> {
        #[cfg(test)]
        if let Some(memory) = &self.memory {
            return Ok(memory
                .lock()
                .expect("test credential map is not poisoned")
                .get(account)
                .cloned());
        }
        match keyring::Entry::new(SERVICE, account)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Delete the credential `account`; returns whether it existed.
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn delete_credential(&self, account: &str) -> Result<bool, SecretError> {
        #[cfg(test)]
        if let Some(memory) = &self.memory {
            return Ok(memory
                .lock()
                .expect("test credential map is not poisoned")
                .remove(account)
                .is_some());
        }
        match keyring::Entry::new(SERVICE, account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Store or update `key = value` for `session` (global when `None`) in
    /// the keychain and index the name.
    fn store(&self, session: Option<&str>, key: &str, value: &str) -> Result<(), SecretError> {
        Self::validate_name(key)?;
        self.write_credential(&Self::account(session, key), value)?;
        let mut index = self.index();
        let names = match session {
            None => &mut index.names,
            Some(id) => index.sessions.entry(id.to_string()).or_default(),
        };
        if !names.iter().any(|name| name == key) {
            names.push(key.to_string());
            names.sort();
            self.write_index(&index)?;
        }
        Ok(())
    }

    /// Store or update the global secret `key = value`.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when the name is not env-var-safe or the
    /// keychain/index write fails.
    pub fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.store(None, key, value)
    }

    /// Store or update `key = value` in `scope`; a session scope is held by
    /// `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when the name is not env-var-safe or the
    /// keychain/index write fails.
    pub fn set_scoped(
        &self,
        scope: SecretScope,
        session_id: &str,
        key: &str,
        value: &str,
    ) -> Result<(), SecretError> {
        match scope {
            SecretScope::Global => self.store(None, key, value),
            SecretScope::Session => self.store(Some(session_id), key, value),
        }
    }

    /// The stored value of the global secret `key`, or `None` when no such
    /// secret exists.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when the name is invalid or the keychain read fails.
    pub fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        self.read_credential(&Self::account(None, Self::validate_name(key)?))
    }

    /// Remove the global secret `key` from the keychain and index. Returns
    /// whether it existed.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when the keychain delete or index write fails.
    pub fn remove(&self, key: &str) -> Result<bool, SecretError> {
        let existed = self.delete_credential(&Self::account(None, key))?;
        if existed {
            let mut index = self.index();
            index.names.retain(|name| name != key);
            self.write_index(&index)?;
        }
        Ok(existed)
    }

    /// `(name, value)` pairs of `names`; names whose keychain read fails are
    /// skipped.
    fn read_all(&self, session: Option<&str>, names: Vec<String>) -> Vec<(String, String)> {
        names
            .into_iter()
            .filter_map(|name| {
                self.read_credential(&Self::account(session, &name))
                    .map_err(|error| {
                        tracing::warn!(secret = %name, %error, "failed to read secret from keychain");
                        error
                    })
                    .ok()
                    .flatten()
                    .map(|value| (name, value))
            })
            .collect()
    }

    /// All global `(name, value)` pairs.
    #[must_use]
    pub fn values(&self) -> Vec<(String, String)> {
        self.read_all(None, self.index().names)
    }

    /// All `(name, value)` pairs `session_id` holds alone.
    #[must_use]
    pub fn session_values(&self, session_id: &str) -> Vec<(String, String)> {
        let names = self.index().sessions.remove(session_id).unwrap_or_default();
        self.read_all(Some(session_id), names)
    }

    /// What `session_id`'s terminals see: the global secrets and the
    /// session's own, which win a name both hold.
    #[must_use]
    pub fn session_env(&self, session_id: &str) -> Vec<(String, String)> {
        let own = self.session_values(session_id);
        let mut env: Vec<(String, String)> = self
            .values()
            .into_iter()
            .filter(|(name, _)| !own.iter().any(|(own_name, _)| own_name == name))
            .collect();
        env.extend(own);
        env
    }

    /// Delete every secret `session_id` holds.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when a keychain delete or the index write fails.
    pub fn remove_session(&self, session_id: &str) -> Result<(), SecretError> {
        let mut index = self.index();
        let Some(names) = index.sessions.remove(session_id) else {
            return Ok(());
        };
        for name in &names {
            self.delete_credential(&Self::account(Some(session_id), name))?;
        }
        self.write_index(&index)
    }

    /// Delete the secrets of every session.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when a keychain delete or the index write fails.
    pub fn remove_all_sessions(&self) -> Result<(), SecretError> {
        let mut index = self.index();
        for (session_id, names) in std::mem::take(&mut index.sessions) {
            for name in &names {
                self.delete_credential(&Self::account(Some(&session_id), name))?;
            }
        }
        self.write_index(&index)
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

    /// [`Self::load`] plus the secrets `session_id` holds alone.
    #[must_use]
    pub fn load_for_session(
        store: &SecretStore,
        config: &crate::config::MayConfig,
        session_id: &str,
    ) -> Self {
        let mut secrets = Self::load(store, config);
        secrets.entries.extend(store.session_values(session_id));
        secrets
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
            .filter(|value| value.len() >= MIN_SECRET_LEN)
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

/// Shortest stored value that is masked or redacted: shorter values would
/// rewrite ordinary text.
const MIN_SECRET_LEN: usize = 6;

/// Reads the stored secrets afresh for every redaction, so a secret added
/// with `/secret` while a session runs is covered from its next request.
///
/// Cheap to clone: it holds only the path of the names index.
#[derive(Debug, Clone)]
pub struct SecretRedactor {
    store: SecretStore,
    /// The session whose own secrets are covered besides the global ones.
    session_id: Option<String>,
    /// Secrets tests stand in for the keychain.
    #[cfg(test)]
    fixed: Option<Vec<(String, String)>>,
}

impl SecretRedactor {
    /// Redactor over the global secrets stored under `may_dir` (`~/.may`).
    #[must_use]
    pub fn new(may_dir: &Path) -> Self {
        Self::over(SecretStore::new(may_dir), None)
    }

    /// Redactor over the global secrets and those `session_id` holds alone.
    #[must_use]
    pub fn for_session(may_dir: &Path, session_id: &str) -> Self {
        Self::over(SecretStore::new(may_dir), Some(session_id.to_string()))
    }

    /// Redactor over `store`, with the secrets of `session_id` when given.
    #[must_use]
    pub const fn over(store: SecretStore, session_id: Option<String>) -> Self {
        Self {
            store,
            session_id,
            #[cfg(test)]
            fixed: None,
        }
    }

    /// Redactor over exactly `secrets`, without touching the keychain.
    #[cfg(test)]
    pub(crate) fn fixed(secrets: &[(&str, &str)]) -> Self {
        Self {
            fixed: Some(
                secrets
                    .iter()
                    .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                    .collect(),
            ),
            ..Self::new(Path::new("/nonexistent"))
        }
    }

    /// The secrets stored right now. Keychain reads run off the caller's
    /// thread.
    pub async fn current(&self) -> Redactions {
        #[cfg(test)]
        if let Some(fixed) = &self.fixed {
            return Redactions::new(fixed.clone());
        }
        let store = self.store.clone();
        let session_id = self.session_id.clone();
        Redactions::new(
            blocking::unblock(move || {
                let mut secrets = store.values();
                if let Some(session_id) = session_id {
                    secrets.extend(store.session_values(&session_id));
                }
                secrets
            })
            .await,
        )
    }
}

/// A snapshot of stored secrets that replaces each value with its name.
///
/// Values are replaced longest first, ties by name, so the same text and the
/// same secrets always give the same bytes.
#[derive(Debug, Clone)]
pub struct Redactions {
    /// `(name, value)` pairs, longest value first.
    entries: Vec<(String, String)>,
}

impl Redactions {
    fn new(mut entries: Vec<(String, String)>) -> Self {
        entries.retain(|(_, value)| value.len() >= MIN_SECRET_LEN);
        entries.sort_unstable_by(|(left_name, left), (right_name, right)| {
            right
                .len()
                .cmp(&left.len())
                .then_with(|| left_name.cmp(right_name))
        });
        Self { entries }
    }

    /// `text` with every stored value replaced by its secret's name.
    #[must_use]
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut redacted = Cow::Borrowed(text);
        for (name, value) in &self.entries {
            if redacted.contains(value.as_str()) {
                redacted = Cow::Owned(redacted.replace(value.as_str(), name));
            }
        }
        redacted
    }

    fn redact_in_place(&self, text: &mut String) {
        if let Cow::Owned(redacted) = self.redact(text) {
            *text = redacted;
        }
    }

    fn redact_json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => self.redact_in_place(text),
            serde_json::Value::Array(items) => {
                for item in items {
                    self.redact_json(item);
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values_mut() {
                    self.redact_json(item);
                }
            }
            _ => {}
        }
    }

    fn redact_stream(&self, stream: &mut CommandStream) {
        match stream {
            CommandStream::Inline { text } => self.redact_in_place(text),
            CommandStream::Stored { preview, .. } => self.redact_in_place(preview),
            CommandStream::Image { .. } => {}
        }
    }

    /// Redacts the text a tool result carries.
    pub fn redact_tool_result(&self, result: &mut ToolResult) {
        match result {
            ToolResult::Text { text } | ToolResult::Tsv { text } => self.redact_in_place(text),
            ToolResult::Error { message } => self.redact_in_place(message),
            ToolResult::Command(record) => {
                self.redact_stream(&mut record.stdout);
                if let Some(stderr) = &mut record.stderr {
                    self.redact_stream(stderr);
                }
                if let CommandOutcome::Killed { notice, .. } = &mut record.outcome {
                    self.redact_in_place(notice);
                }
            }
            ToolResult::Parts { parts } => {
                for part in parts {
                    if let ToolResultPart::Text { text } | ToolResultPart::Tsv { text } = part {
                        self.redact_in_place(text);
                    }
                }
            }
            ToolResult::Done | ToolResult::Binary { .. } => {}
        }
    }

    /// Redacts every message of `request`: text of every role and the
    /// string arguments of tool calls. Reasoning state is left untouched.
    pub fn redact_request(&self, request: &mut LLMRequest) {
        if self.entries.is_empty() {
            return;
        }
        for message in request.messages_mut() {
            match message {
                Message::User { content, .. }
                | Message::System { content }
                | Message::Tool { content, .. } => self.redact_in_place(content),
                Message::Assistant {
                    content,
                    tool_calls,
                    ..
                } => {
                    self.redact_in_place(content);
                    for call in tool_calls {
                        self.redact_json(&mut call.arguments);
                    }
                }
            }
        }
    }
}

/// The secrets one session's terminals see: the global ones and its own,
/// re-read for every spawned command so a new secret takes effect without a
/// rebuild.
#[derive(Debug, Clone)]
pub struct SessionEnv {
    store: SecretStore,
    session_id: String,
}

impl SessionEnv {
    /// The environment of `session_id` over `store`.
    #[must_use]
    pub fn new(store: SecretStore, session_id: impl Into<String>) -> Self {
        Self {
            store,
            session_id: session_id.into(),
        }
    }
}

impl aither_sandbox::TerminalEnvSource for SessionEnv {
    fn env(&self) -> Vec<(String, String)> {
        self.store.session_env(&self.session_id)
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

    fn redactions() -> Redactions {
        Redactions::new(vec![
            ("SHORT".to_string(), "abc".to_string()),
            (
                "DB_PASSWORD".to_string(),
                "hunter2-correct-horse".to_string(),
            ),
            (
                "DB_PASSWORD_PREFIX".to_string(),
                "hunter2-correct".to_string(),
            ),
        ])
    }

    #[test]
    fn redaction_replaces_values_with_names_longest_first() {
        let redactions = redactions();
        assert_eq!(
            redactions.redact("a hunter2-correct-horse b hunter2-correct c"),
            "a DB_PASSWORD b DB_PASSWORD_PREFIX c"
        );
        // Values under the floor stay: they would rewrite ordinary text.
        assert_eq!(redactions.redact("abc"), "abc");
    }

    #[test]
    fn redaction_is_deterministic() {
        let text = "x hunter2-correct-horse y hunter2-correct";
        assert_eq!(redactions().redact(text), redactions().redact(text));
        let reversed = Redactions::new(vec![
            (
                "DB_PASSWORD_PREFIX".to_string(),
                "hunter2-correct".to_string(),
            ),
            (
                "DB_PASSWORD".to_string(),
                "hunter2-correct-horse".to_string(),
            ),
        ]);
        assert_eq!(reversed.redact(text), redactions().redact(text));
    }

    #[test]
    fn redaction_covers_command_results() {
        let redactions = redactions();
        let mut result = ToolResult::Command(Box::new(aither_core::llm::CommandResult {
            stdout: CommandStream::Inline {
                text: "hunter2-correct-horse\n".to_string(),
            },
            stderr: Some(CommandStream::Inline {
                text: "bad hunter2-correct-horse".to_string(),
            }),
            outcome: CommandOutcome::Exited { code: 0 },
            interruptions: Vec::new(),
        }));
        redactions.redact_tool_result(&mut result);
        let ToolResult::Command(record) = result else {
            panic!("a command result stays a command result");
        };
        assert_eq!(
            record.stdout,
            CommandStream::Inline {
                text: "DB_PASSWORD\n".to_string()
            }
        );
        assert_eq!(
            record.stderr,
            Some(CommandStream::Inline {
                text: "bad DB_PASSWORD".to_string()
            })
        );
    }

    #[test]
    fn request_redaction_covers_every_role_and_leaves_reasoning() {
        let reasoning = aither_core::llm::ReasoningState::new("claude", "hunter2-correct-horse");
        let call = aither_core::llm::ToolCall::new(
            "c1",
            "terminal",
            serde_json::json!({"command": "echo hunter2-correct-horse", "n": 1}),
        );
        let mut request = LLMRequest::new(vec![
            Message::system("sys hunter2-correct-horse"),
            Message::user("user hunter2-correct-horse"),
            Message::assistant_with_reasoning("", vec![call], vec![reasoning.clone()]),
            Message::tool("c1", "tool hunter2-correct-horse"),
        ]);
        redactions().redact_request(&mut request);
        let rendered = format!("{:?}", request.messages());
        assert!(!rendered.contains("echo hunter2"), "{rendered}");
        assert_eq!(request.messages()[0].content(), "sys DB_PASSWORD");
        assert_eq!(request.messages()[1].content(), "user DB_PASSWORD");
        assert_eq!(request.messages()[3].content(), "tool DB_PASSWORD");
        assert_eq!(
            request.messages()[2].tool_calls()[0].arguments["command"],
            "echo DB_PASSWORD"
        );
        assert_eq!(request.messages()[2].reasoning(), [reasoning]);
    }
}
