//! Checking text the agent publishes to external contributors.
//!
//! Credentials May keeps, strings shaped like keys or tokens, this machine's
//! host names, and the user's home directory path are refused locally before
//! any model request is made. Clean text is judged once through the fast
//! model ([`ModelSlot::generate`]).

use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use aither_core::{LanguageModel, llm::Message};
use arc_swap::ArcSwap;
use askama::Template;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::secret_guard::{SecretGuard, SecretKind};
use crate::{agent::ModelSlot, auth_store::load_json, config::MayConfig, secrets::SecretStore};

/// Minimum length of a credential value checked verbatim against published
/// text, matching [`crate::secrets`] so short dummy values never block prose.
const MIN_SECRET_LEN: usize = 6;

const REVIEW_PROMPT: &str = include_str!("../../templates/publish_review_prompt.txt");
const DEFAULT_MODEL_REFUSAL: &str =
    "rewrite in a contributor's voice without agent working-process or user-environment details";

/// Kind of public destination a published text targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// A new GitHub issue (`issue create`).
    Issue,
    /// An edited GitHub issue or pull request title/body (`issue edit`).
    IssueEdit,
    /// A new GitHub pull request (`pr open`).
    PullRequest,
    /// A comment on a GitHub issue or pull request (`issue comment`).
    Comment,
    /// Commit messages pushed to a remote branch (`pr update`).
    CommitMessages,
}

impl TargetKind {
    /// Human-readable label shown to the review model.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::IssueEdit => "issue edit",
            Self::PullRequest => "pull request",
            Self::Comment => "comment",
            Self::CommitMessages => "commit messages",
        }
    }
}

/// One commit message pushed to a remote repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitMessage<'a> {
    /// Abbreviated commit hash identifying the commit to the reviewer.
    pub short_sha: &'a str,
    /// Full raw commit message (`subject\n\nbody`).
    pub message: &'a str,
}

/// Text about to be published, together with where it goes.
#[derive(Debug, Clone)]
pub struct Publication<'a> {
    /// Public destination kind.
    pub target: TargetKind,
    /// Title, when the publication carries one.
    pub title: Option<&'a str>,
    /// Body text, when the publication carries one.
    pub body: Option<&'a str>,
    /// Commit messages pushed as part of the operation, when any.
    pub commits: &'a [CommitMessage<'a>],
}

impl<'a> Publication<'a> {
    /// Text of `issue create`.
    #[must_use]
    pub const fn issue(title: &'a str, body: Option<&'a str>) -> Self {
        Self {
            target: TargetKind::Issue,
            title: Some(title),
            body,
            commits: &[],
        }
    }

    /// Text of `issue edit`.
    #[must_use]
    pub const fn issue_edit(title: Option<&'a str>, body: Option<&'a str>) -> Self {
        Self {
            target: TargetKind::IssueEdit,
            title,
            body,
            commits: &[],
        }
    }

    /// Text of `issue comment`.
    #[must_use]
    pub const fn comment(body: &'a str) -> Self {
        Self {
            target: TargetKind::Comment,
            title: None,
            body: Some(body),
            commits: &[],
        }
    }

    /// Text of `pr open`.
    #[must_use]
    pub const fn pull_request(
        title: &'a str,
        body: &'a str,
        commits: &'a [CommitMessage<'a>],
    ) -> Self {
        Self {
            target: TargetKind::PullRequest,
            title: Some(title),
            body: Some(body),
            commits,
        }
    }

    /// Commit messages pushed by `pr update`.
    #[must_use]
    pub const fn commit_messages(commits: &'a [CommitMessage<'a>]) -> Self {
        Self {
            target: TargetKind::CommitMessages,
            title: None,
            body: None,
            commits,
        }
    }

    fn is_empty(&self) -> bool {
        self.title.is_none_or(|t| t.trim().is_empty())
            && self.body.is_none_or(|b| b.trim().is_empty())
            && self.commits.iter().all(|c| c.message.trim().is_empty())
    }

    fn texts(&self) -> impl Iterator<Item = &str> {
        self.title
            .into_iter()
            .chain(self.body)
            .chain(self.commits.iter().map(|c| c.message))
            .filter(|text| !text.is_empty())
    }
}

/// Decision returned by the fast-model review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReviewAction {
    /// Safe to publish as written.
    Publish,
    /// Must be rewritten before publishing.
    Refuse,
}

/// Structured judgment from one fast-model call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelReview {
    /// Whether the text may be published or must be refused.
    pub decision: ReviewAction,
    /// Specific problems to remove or rewrite when refused; empty when approved.
    #[serde(default)]
    pub findings: Vec<String>,
}

/// Why a publication was stopped before leaving May.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublishRefusal {
    /// Stopped locally without calling any model API (credential, token shape,
    /// machine host name, or user home directory path).
    #[error("{0}")]
    Local(String),
    /// Stopped by the fast-model review with findings to address.
    #[error("{0}")]
    Model(String),
    /// A credential store or fast-model request failed before review could
    /// settle safely.
    #[error("publish review unavailable: {0}")]
    Unavailable(String),
}

/// Host names and home directory paths belonging to this machine, refused
/// locally without sending text to any model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalEnvironment {
    /// System host names (`uname` nodename / OS hostname and short label).
    pub system_hosts: Vec<String>,
    /// Local network mDNS names (`LocalHostName` and `<name>.local`).
    pub local_hosts: Vec<String>,
    /// Tailscale node names (`DNSName` and short node label).
    pub tailscale_hosts: Vec<String>,
    /// Absolute paths of the user's home directory.
    pub home_dirs: Vec<PathBuf>,
}

impl LocalEnvironment {
    /// Empty environment with no host names or home paths (useful in isolated tests).
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            system_hosts: Vec::new(),
            local_hosts: Vec::new(),
            tailscale_hosts: Vec::new(),
            home_dirs: Vec::new(),
        }
    }

    /// Detect the current machine's system host name, local network name,
    /// Tailscale node name, and user home directory paths.
    pub async fn detect(tailscale_binary: Option<&str>) -> Self {
        let tailscale_bin = tailscale_binary
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(crate::config::DEFAULT_TAILSCALE_BINARY)
            .to_string();
        blocking::unblock(move || detect_local_environment(&tailscale_bin)).await
    }

    fn find_labels(&self, text: &str) -> Vec<String> {
        let mut labels = Vec::new();
        let mut push_unique = |label: &str| {
            if !labels.iter().any(|existing| existing == label) {
                labels.push(label.to_string());
            }
        };
        for home in &self.home_dirs {
            if contains_home_dir(text, home) {
                push_unique("the user's home directory path");
                break;
            }
        }
        if self
            .system_hosts
            .iter()
            .any(|host| contains_host_name(text, host))
        {
            push_unique("the machine's system host name");
        }
        if self
            .local_hosts
            .iter()
            .any(|host| contains_host_name(text, host))
        {
            push_unique("the machine's local network name");
        }
        if self
            .tailscale_hosts
            .iter()
            .any(|host| contains_host_name(text, host))
        {
            push_unique("the machine's Tailscale node name");
        }
        labels
    }
}

#[derive(Template)]
#[template(path = "publish_review_input.txt", escape = "none")]
struct ReviewInput<'a> {
    target: &'static str,
    title: Option<&'a str>,
    body: Option<&'a str>,
    commits: &'a [CommitMessage<'a>],
}

#[derive(Template)]
#[template(path = "publish_precheck_refusal.txt", escape = "none")]
struct PrecheckRefusal<'a> {
    items: &'a [String],
}

#[derive(Template)]
#[template(path = "publish_model_refusal.txt", escape = "none")]
struct ModelRefusal<'a> {
    findings: &'a [String],
}

trait Judge: Send + Sync {
    fn review<'a>(
        &'a self,
        input: String,
    ) -> Pin<Box<dyn Future<Output = Result<ModelReview, String>> + Send + 'a>>;
}

struct ModelSlotJudge<LLM> {
    slot: ModelSlot<LLM>,
}

impl<LLM> Judge for ModelSlotJudge<LLM>
where
    LLM: LanguageModel + Clone + Sync + Send + 'static,
{
    fn review<'a>(
        &'a self,
        input: String,
    ) -> Pin<Box<dyn Future<Output = Result<ModelReview, String>> + Send + 'a>> {
        Box::pin(async move {
            self.slot
                .generate::<ModelReview>(vec![Message::system(REVIEW_PROMPT), Message::user(input)])
                .await
                .map_err(|error| error.to_string())
        })
    }
}

/// Credentials May keeps plus explicit extra secrets for testing.
#[derive(Clone)]
struct CredentialConfig {
    may_dir: Option<PathBuf>,
    session_id: String,
    secret_store: Option<SecretStore>,
    live_config: Option<Arc<ArcSwap<MayConfig>>>,
    fixed_secrets: Vec<(String, String)>,
}

/// Outbound publish review shared by `issue`, `pr`, and commit pushes.
///
/// Cheap to clone: shares the compiled shape guard, local machine names, and
/// fast-model handle.
#[derive(Clone)]
pub struct PublishReview {
    base_guard: SecretGuard,
    credentials: CredentialConfig,
    environment: Arc<LocalEnvironment>,
    judge: Arc<dyn Judge>,
}

impl std::fmt::Debug for PublishReview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishReview")
            .field("environment", &self.environment)
            .finish_non_exhaustive()
    }
}

impl PublishReview {
    /// Creates a publish review backed by `slot`, live session credentials in
    /// `may_dir` / `live_config`, and detected `environment`.
    #[must_use]
    pub fn new<LLM>(
        slot: ModelSlot<LLM>,
        may_dir: PathBuf,
        session_id: String,
        live_config: Option<Arc<ArcSwap<MayConfig>>>,
        environment: LocalEnvironment,
    ) -> Self
    where
        LLM: LanguageModel + Clone + Sync + Send + 'static,
    {
        Self {
            base_guard: SecretGuard::new(),
            credentials: CredentialConfig {
                secret_store: Some(SecretStore::new(&may_dir)),
                may_dir: Some(may_dir),
                session_id,
                live_config,
                fixed_secrets: Vec::new(),
            },
            environment: Arc::new(environment),
            judge: Arc::new(ModelSlotJudge { slot }),
        }
    }

    /// Creates a self-contained publish review with explicit `environment` and
    /// no filesystem/keychain lookup unless configured via builders.
    #[must_use]
    pub fn for_slot<LLM>(slot: ModelSlot<LLM>, environment: LocalEnvironment) -> Self
    where
        LLM: LanguageModel + Clone + Sync + Send + 'static,
    {
        Self {
            base_guard: SecretGuard::new(),
            credentials: CredentialConfig {
                may_dir: None,
                session_id: String::new(),
                secret_store: None,
                live_config: None,
                fixed_secrets: Vec::new(),
            },
            environment: Arc::new(environment),
            judge: Arc::new(ModelSlotJudge { slot }),
        }
    }

    /// Registers a credential `(name, value)` checked locally without touching
    /// any model API.
    #[must_use]
    pub fn with_credential(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let value = value.into();
        if !value.is_empty() {
            self.credentials.fixed_secrets.push((name.into(), value));
        }
        self
    }

    /// Reads stored secrets from `store` for `session_id` during local
    /// pre-screening.
    #[must_use]
    pub fn with_secret_store(mut self, store: SecretStore, session_id: impl Into<String>) -> Self {
        self.credentials.secret_store = Some(store);
        self.credentials.session_id = session_id.into();
        self
    }

    /// Reads configured provider API keys from `live_config` during local
    /// pre-screening.
    #[must_use]
    pub fn with_live_config(mut self, live_config: Arc<ArcSwap<MayConfig>>) -> Self {
        self.credentials.live_config = Some(live_config);
        self
    }

    /// Checks local secrets, token shapes, host names, and home directory
    /// paths first (without making any model call), then runs one fast-model
    /// judgment call on clean non-empty text.
    ///
    /// # Errors
    /// Returns [`PublishRefusal`] naming what must be removed or rewritten
    /// (never repeating any secret value) or describing why review failed.
    ///
    /// # Panics
    /// Never: the review input template renders for every input.
    pub async fn review(&self, publication: Publication<'_>) -> Result<(), PublishRefusal> {
        self.precheck(&publication).await?;
        if publication.is_empty() {
            return Ok(());
        }
        let input = ReviewInput {
            target: publication.target.label(),
            title: publication.title,
            body: publication.body,
            commits: publication.commits,
        }
        .render()
        .expect("the publish review input template renders typed input");
        let verdict = self
            .judge
            .review(input)
            .await
            .map_err(PublishRefusal::Unavailable)?;
        match verdict.decision {
            ReviewAction::Publish => Ok(()),
            ReviewAction::Refuse => {
                let fallback;
                let findings = if verdict.findings.is_empty() {
                    fallback = [DEFAULT_MODEL_REFUSAL.to_string()];
                    fallback.as_slice()
                } else {
                    verdict.findings.as_slice()
                };
                Err(PublishRefusal::Model(
                    ModelRefusal { findings }
                        .render()
                        .expect("the publish review refusal template renders typed input"),
                ))
            }
        }
    }

    async fn precheck(&self, publication: &Publication<'_>) -> Result<(), PublishRefusal> {
        let guard = self.load_secret_guard().await?;
        let mut items = Vec::new();
        let mut push_unique = |item: String| {
            if !items.contains(&item) {
                items.push(item);
            }
        };
        for text in publication.texts() {
            for finding in guard.find(text) {
                let label = match finding.kind {
                    SecretKind::Registered { name } => name.to_string(),
                    SecretKind::Shaped(shape) => shape.to_string(),
                };
                push_unique(label);
            }
            for env_label in self.environment.find_labels(text) {
                push_unique(env_label);
            }
        }
        if items.is_empty() {
            Ok(())
        } else {
            let wording = PrecheckRefusal { items: &items }
                .render()
                .expect("the publish precheck refusal template renders typed input");
            Err(PublishRefusal::Local(wording))
        }
    }

    async fn load_secret_guard(&self) -> Result<SecretGuard, PublishRefusal> {
        let guard = self.base_guard.empty_sharing_shapes();
        for (name, value) in &self.credentials.fixed_secrets {
            guard.register(name, value);
        }
        if let Some(live_config) = &self.credentials.live_config {
            let config = live_config.load();
            for (provider_id, instance) in &config.provider {
                if let Some(key) = instance.api_key.as_deref()
                    && key.len() >= MIN_SECRET_LEN
                {
                    guard.register(format!("provider {provider_id} API key"), key);
                }
            }
        }
        if let Some(store) = self.credentials.secret_store.clone() {
            let session_id = self.credentials.session_id.clone();
            let stored = blocking::unblock(move || {
                let mut pairs = store.values()?;
                if !session_id.is_empty() {
                    pairs.extend(store.session_values(&session_id)?);
                }
                Ok::<_, crate::secrets::SecretError>(pairs)
            })
            .await
            .map_err(|error| PublishRefusal::Unavailable(error.to_string()))?;
            for (name, value) in stored {
                if value.len() >= MIN_SECRET_LEN {
                    guard.register(format!("stored secret {name}"), value);
                }
            }
        }
        if let Some(may_dir) = &self.credentials.may_dir {
            register_oauth_tokens(&guard, may_dir).await?;
        }
        Ok(guard)
    }
}

#[derive(Deserialize)]
struct CodexTokenFile {
    #[serde(rename = "access_token")]
    access: String,
    #[serde(rename = "refresh_token")]
    refresh: String,
    #[serde(default, rename = "id_token")]
    id: String,
}

#[derive(Deserialize)]
struct GrokTokenFile {
    access_token: String,
    refresh_token: String,
}

#[derive(Deserialize)]
struct CopilotOAuthFile {
    access_token: String,
}

#[derive(Deserialize)]
struct CopilotSessionFile {
    token: String,
}

async fn register_oauth_tokens(guard: &SecretGuard, may_dir: &Path) -> Result<(), PublishRefusal> {
    if let Some(codex) = load_json::<CodexTokenFile>(&may_dir.join("chatgpt_token.json"))
        .await
        .map_err(|error| PublishRefusal::Unavailable(error.to_string()))?
    {
        for (suffix, value) in [
            ("access token", codex.access),
            ("refresh token", codex.refresh),
            ("ID token", codex.id),
        ] {
            if value.len() >= MIN_SECRET_LEN {
                guard.register(format!("ChatGPT {suffix}"), value);
            }
        }
    }
    if let Some(grok) = load_json::<GrokTokenFile>(&may_dir.join("xai_token.json"))
        .await
        .map_err(|error| PublishRefusal::Unavailable(error.to_string()))?
    {
        for (suffix, value) in [
            ("access token", grok.access_token),
            ("refresh token", grok.refresh_token),
        ] {
            if value.len() >= MIN_SECRET_LEN {
                guard.register(format!("xAI Grok {suffix}"), value);
            }
        }
    }
    if let Some(copilot) = load_json::<CopilotOAuthFile>(&may_dir.join("copilot_token.json"))
        .await
        .map_err(|error| PublishRefusal::Unavailable(error.to_string()))?
        && copilot.access_token.len() >= MIN_SECRET_LEN
    {
        guard.register("GitHub Copilot OAuth token", copilot.access_token);
    }
    if let Some(copilot_session) =
        load_json::<CopilotSessionFile>(&may_dir.join("copilot_session.json"))
            .await
            .map_err(|error| PublishRefusal::Unavailable(error.to_string()))?
        && copilot_session.token.len() >= MIN_SECRET_LEN
    {
        guard.register("GitHub Copilot session token", copilot_session.token);
    }
    Ok(())
}

#[derive(Deserialize)]
struct TailscaleStatusSelf {
    #[serde(rename = "Self")]
    self_node: Option<TailscaleNodeName>,
}

#[derive(Deserialize)]
struct TailscaleNodeName {
    #[serde(rename = "DNSName", default)]
    dns_name: String,
}

fn push_host_variants(target: &mut Vec<String>, raw: &str) {
    let trimmed = raw.trim().trim_end_matches('.');
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("localhost")
        || trimmed.eq_ignore_ascii_case("localhost.localdomain")
    {
        return;
    }
    if !target.iter().any(|h| h.eq_ignore_ascii_case(trimmed)) {
        target.push(trimmed.to_string());
    }
    if let Some((short, _)) = trimmed.split_once('.')
        && !short.is_empty()
        && !short.eq_ignore_ascii_case("localhost")
        && !target.iter().any(|h| h.eq_ignore_ascii_case(short))
    {
        target.push(short.to_string());
    }
}

#[cfg(target_os = "macos")]
fn scutil_get(key: &str) -> Option<String> {
    let output = std::process::Command::new("scutil")
        .args(["--get", key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(unix)]
fn system_hostname() -> Option<String> {
    let mut buf = [0_u8; 256];
    // SAFETY: `buf` is a stack buffer of `buf.len()` bytes valid for writing
    // by `gethostname` across its full length.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = std::str::from_utf8(&buf[..len]).ok()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn detect_local_environment(tailscale_binary: &str) -> LocalEnvironment {
    let mut env = LocalEnvironment::empty();

    #[cfg(unix)]
    if let Some(nodename) = system_hostname() {
        push_host_variants(&mut env.system_hosts, &nodename);
        if let Some(mdns) = nodename
            .strip_suffix(".local")
            .or_else(|| nodename.split('.').next())
            && !mdns.is_empty()
            && !mdns.eq_ignore_ascii_case("localhost")
        {
            push_host_variants(&mut env.local_hosts, &format!("{mdns}.local"));
        }
    }
    #[cfg(not(unix))]
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(value) = std::env::var(key) {
            push_host_variants(&mut env.system_hosts, &value);
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(host_name) = scutil_get("HostName") {
            push_host_variants(&mut env.system_hosts, &host_name);
        }
        if let Some(local_name) = scutil_get("LocalHostName") {
            push_host_variants(&mut env.local_hosts, &local_name);
            push_host_variants(
                &mut env.local_hosts,
                &format!("{}.local", local_name.trim_end_matches('.')),
            );
        }
    }

    if let Ok(output) = std::process::Command::new(tailscale_binary)
        .args(["status", "--json"])
        .output()
        && output.status.success()
        && let Ok(parsed) = serde_json::from_slice::<TailscaleStatusSelf>(&output.stdout)
        && let Some(node) = parsed.self_node
    {
        push_host_variants(&mut env.tailscale_hosts, &node.dns_name);
    }

    if let Some(home) = dirs::home_dir() {
        env.home_dirs.push(home);
    }
    if let Ok(canonical) = MayConfig::home_dir()
        && !env.home_dirs.iter().any(|existing| existing == &canonical)
    {
        env.home_dirs.push(canonical);
    }

    env
}

const fn is_host_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-'
}

fn contains_host_name(text: &str, host: &str) -> bool {
    let host = host.trim().trim_end_matches('.');
    if host.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("localhost.localdomain")
    {
        return false;
    }
    let text_lower = text.to_ascii_lowercase();
    let host_lower = host.to_ascii_lowercase();
    let bytes = text_lower.as_bytes();
    let mut start = 0;
    while let Some(offset) = text_lower[start..].find(&host_lower) {
        let match_start = start + offset;
        let match_end = match_start + host_lower.len();
        let before_ok = match_start == 0 || !is_host_char(bytes[match_start - 1]);
        let after_ok = match_end == bytes.len() || !is_host_char(bytes[match_end]);
        if before_ok && after_ok {
            return true;
        }
        start = match_start + 1;
    }
    false
}

fn contains_home_dir(text: &str, home: &Path) -> bool {
    let Some(home) = home.to_str() else {
        return false;
    };
    let home = home.trim_end_matches(['/', '\\']);
    if home.is_empty() || !Path::new(home).is_absolute() {
        return false;
    }
    #[cfg(any(target_os = "macos", windows))]
    let (haystack, needle) = (text.to_ascii_lowercase(), home.to_ascii_lowercase());
    #[cfg(not(any(target_os = "macos", windows)))]
    let (haystack, needle) = (text, home);
    let needle: &str = &needle;

    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(offset) = haystack[start..].find(needle) {
        let match_start = start + offset;
        let match_end = match_start + needle.len();
        let before_ok = match_start == 0 || !bytes[match_start - 1].is_ascii_alphanumeric();
        let after_ok = match_end == bytes.len()
            || (!bytes[match_end].is_ascii_alphanumeric()
                && bytes[match_end] != b'-'
                && bytes[match_end] != b'_');
        if before_ok && after_ok {
            return true;
        }
        start = match_start + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use aither_core::llm::{
        Event, LLMRequest,
        model::{Parameters, Profile},
    };
    use futures::Stream;

    use super::*;

    #[derive(Clone)]
    struct CountingModel {
        calls: Arc<AtomicUsize>,
        reply: Arc<str>,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("counting model error")]
    struct CountingError;

    impl aither_core::llm::Classify for CountingError {
        fn failure(&self) -> aither_core::llm::Failure {
            aither_core::llm::Failure::permanent()
        }
    }

    impl LanguageModel for CountingModel {
        type Error = CountingError;

        fn respond(
            &self,
            _request: LLMRequest,
        ) -> impl Stream<Item = Result<Event, Self::Error>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let reply = self.reply.to_string();
            futures_lite::stream::iter([Ok(Event::Text(reply))])
        }

        fn profile(&self) -> impl Future<Output = Result<Profile, Self::Error>> + Send {
            std::future::ready(Ok(Profile::new(
                "mock",
                "test",
                "fast",
                "fast mock",
                100_000,
            )))
        }
    }

    fn review_with(reply: &str, env: LocalEnvironment) -> (PublishReview, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let model = CountingModel {
            calls: calls.clone(),
            reply: Arc::from(reply),
        };
        let review = PublishReview::for_slot(ModelSlot::new(model, Parameters::default()), env);
        (review, calls)
    }

    #[tokio::test]
    async fn clean_text_passes_one_fast_model_review() {
        let (review, calls) = review_with(
            r#"{"decision":"publish","findings":[]}"#,
            LocalEnvironment::empty(),
        );
        review
            .review(Publication::issue(
                "Fix parser panic on empty input",
                Some("Return a syntax error instead of unwrapping the first token."),
            ))
            .await
            .expect("clean issue publishes");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn model_refusal_returns_findings_and_blocks_publish() {
        let (review, calls) = review_with(
            r#"{"decision":"refuse","findings":["remove session narration","remove local build path"]}"#,
            LocalEnvironment::empty(),
        );
        let err = review
            .review(Publication::comment(
                "In session s1 I ran cargo test in target/tmp and fixed it.",
            ))
            .await
            .expect_err("review refusal stops publish");
        let message = err.to_string();
        assert!(matches!(err, PublishRefusal::Model(_)), "{message}");
        assert!(message.contains("remove session narration"), "{message}");
        assert!(message.contains("remove local build path"), "{message}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refuses_credentials_and_shapes_without_any_model_request() {
        let provider_secret = "custom-provider-secret-998877";
        let stored_secret = "stored-webhook-secret-554433";
        let oauth_secret = "codex-oauth-access-token-112233";
        let shaped_secret = "sk-proj-AbCdEfGhIjKlMnOpQrStUvWxYz012345";

        let dir = tempfile::tempdir().unwrap();
        let mut config = MayConfig::default();
        let mut instance =
            crate::config::ProviderInstance::new(crate::config::ProviderType::OpenAI);
        instance.api_key = Some(provider_secret.to_string());
        config.provider.insert("openai".to_string(), instance);
        let live_config = Arc::new(ArcSwap::from_pointee(config));
        let store = SecretStore::in_memory(dir.path());
        store.set("WEBHOOK_TOKEN", stored_secret).unwrap();
        std::fs::write(
            dir.path().join("chatgpt_token.json"),
            serde_json::json!({
                "access_token": oauth_secret,
                "refresh_token": "codex-refresh-token-445566",
                "id_token": "codex-id-token-778899",
            })
            .to_string(),
        )
        .unwrap();

        for secret in [provider_secret, stored_secret, oauth_secret, shaped_secret] {
            let calls = Arc::new(AtomicUsize::new(0));
            let model = CountingModel {
                calls: calls.clone(),
                reply: Arc::from(r#"{"decision":"publish","findings":[]}"#),
            };
            let review = PublishReview::new(
                ModelSlot::new(model, Parameters::default()),
                dir.path().to_path_buf(),
                String::new(),
                Some(live_config.clone()),
                LocalEnvironment::empty(),
            )
            .with_secret_store(store.clone(), "");

            let err = review
                .review(Publication::issue(
                    "Reproduce failure",
                    Some(&format!("Header used: {secret}")),
                ))
                .await
                .expect_err("secret must be refused locally");
            let message = err.to_string();
            assert!(matches!(err, PublishRefusal::Local(_)), "{message}");
            assert!(
                !message.contains(secret),
                "refusal leaked secret: {message}"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0, "no model request allowed");
        }
    }

    #[tokio::test]
    async fn refuses_host_names_and_home_dir_without_any_model_request() {
        let env = LocalEnvironment {
            system_hosts: vec!["workstation-01".to_string()],
            local_hosts: vec!["workstation-01.local".to_string()],
            tailscale_hosts: vec!["workstation-01.tail999.ts.net".to_string()],
            home_dirs: vec![PathBuf::from("/Users/alice")],
        };

        for bad in [
            "Tested on workstation-01 with cargo nextest.",
            "Server reachable at workstation-01.local:8080.",
            "Tailnet node is WORKSTATION-01.TAIL999.TS.NET.",
            "Config loaded from /Users/alice/.may/config.toml.",
        ] {
            let (review, calls) =
                review_with(r#"{"decision":"publish","findings":[]}"#, env.clone());
            let err = review
                .review(Publication::comment(bad))
                .await
                .expect_err("machine identifier must be refused");
            let message = err.to_string();
            assert!(matches!(err, PublishRefusal::Local(_)), "{message}");
            assert!(!message.contains("workstation-01"), "{message}");
            assert!(!message.contains("/Users/alice"), "{message}");
            assert_eq!(calls.load(Ordering::SeqCst), 0, "no model request allowed");
        }

        // Unrelated text with substrings that are not boundary matches passes.
        let (review, calls) = review_with(r#"{"decision":"publish","findings":[]}"#, env);
        review
            .review(Publication::comment(
                "Compare /Users/alice2 and workstation-01-beta.",
            ))
            .await
            .expect("boundary prevents false positive");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
