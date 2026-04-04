//! Configuration for May agent.
//!
//! Configuration is loaded from `~/.may/config.toml`.
//!
//! # Architecture
//!
//! The configuration is split into three main sections:
//!
//! - **Providers**: Credential stores for LLM services. Each provider has a unique ID
//!   (the section key), type, and credentials. Providers don't select models - they
//!   just provide access.
//!
//! - **Models**: User preferences for which models to use per tier. Each tier (advanced,
//!   balanced, fast, `image_generation`) can have multiple models in priority order.
//!   The system will try providers in order and failover to the next model if all fail.
//!
//! - **Preferences**: Global settings like load balancing strategy and warnings.
//!
//! # Example config
//!
//! ```toml
//! max_iterations = 0
//!
//! [provider.anthropic]
//! api_key = "sk-ant-..."
//!
//! [provider.openrouter]
//! api_key = "or-..."
//!
//! [provider.google]
//! api_key = "AIza..."
//!
//! [provider.my-ollama]
//! type = "ollama"
//! base_url = "http://192.168.1.100:11434/v1"
//!
//! [models]
//! advanced = ["claude-opus-4", "gpt-4.5"]
//! balanced = ["claude-sonnet-4", "gemini-2.5-flash"]
//! fast = ["claude-haiku-4", "gpt-4o-mini"]
//! image_generation = ["dall-e-3"]
//! default_tier = "balanced"
//!
//! [preferences]
//! load_balance_strategy = "round_robin"
//! warn_on_model_change = true
//! ```

use async_fs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::network_policy::NetworkPolicyConfig;

/// Infer model tier from aither-models metadata.
///
/// Uses model mode, pricing percentiles, and context window as signals.
#[must_use]
pub fn get_model_tier(model_id: &str) -> ModelTier {
    let chat_price_breakpoints = chat_price_breakpoints();
    aither_models::lookup(model_id).map_or(ModelTier::Balanced, |info| {
        infer_model_tier(info, chat_price_breakpoints)
    })
}

fn chat_price_breakpoints() -> Option<(f64, f64)> {
    let mut prices = aither_models::models_by_mode(aither_models::ModelMode::Chat)
        .map(|info| info.pricing().output_per_token())
        .filter(|price| price.is_finite() && *price > 0.0)
        .collect::<Vec<_>>();
    if prices.len() < 3 {
        return None;
    }

    prices.sort_by(f64::total_cmp);
    let low_index = prices.len() / 3;
    let high_index = (prices.len() * 2) / 3;
    Some((prices[low_index], prices[high_index]))
}

fn infer_model_tier(
    info: &aither_models::ModelEntry,
    chat_price_breakpoints: Option<(f64, f64)>,
) -> ModelTier {
    match info.mode() {
        aither_models::ModelMode::ImageGeneration => return ModelTier::ImageGeneration,
        aither_models::ModelMode::Chat => {}
        _ => return ModelTier::Balanced,
    }

    let output_price = info.pricing().output_per_token();
    if let Some((fast_ceiling, advanced_floor)) = chat_price_breakpoints
        && output_price.is_finite()
        && output_price > 0.0
    {
        if output_price <= fast_ceiling {
            return ModelTier::Fast;
        }
        if output_price >= advanced_floor {
            return ModelTier::Advanced;
        }
        return ModelTier::Balanced;
    }

    match info.max_input_tokens() {
        Some(tokens) if tokens <= 32_768 => ModelTier::Fast,
        Some(tokens) if tokens >= 200_000 => ModelTier::Advanced,
        _ => ModelTier::Balanced,
    }
}

fn normalize_base_url(url: &str) -> &str {
    url.trim_end_matches('/')
}

/// Normalize a model ID for relaxed comparison by stripping trailing date and
/// `-preview` suffixes.  Examples:
///   `claude-3-5-sonnet-20241022` → `claude-3-5-sonnet`
///   `o3-2025-04-16`              → `o3`
///   `gpt-4o-2024-08-06-preview`  → `gpt-4o`
pub fn normalize_model_id_for_relaxed_match(model_id: &str) -> String {
    static SUFFIX_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"-(?:\d{8}|\d{4}-\d{2}-\d{2}|preview)$")
            .expect("model suffix regex must be valid")
    });

    let mut current = model_id.trim().to_ascii_lowercase();
    loop {
        let Some(m) = SUFFIX_RE.find(&current) else {
            break;
        };
        current.truncate(m.start());
    }
    current
}

/// Model ID matching mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    /// Only exact (case-insensitive) matches.
    Strict,
    /// Normalized relaxed matching (strips dates, versions, etc.).
    Relaxed,
}

impl MatchMode {
    /// Create from a `strict_model_matching` preference flag.
    #[must_use]
    pub const fn from_strict_flag(strict: bool) -> Self {
        if strict { Self::Strict } else { Self::Relaxed }
    }
}

#[must_use]
pub fn model_ids_match(candidate: &str, requested: &str, mode: MatchMode) -> bool {
    if candidate.eq_ignore_ascii_case(requested) {
        return true;
    }
    if mode == MatchMode::Strict {
        return false;
    }
    normalize_model_id_for_relaxed_match(candidate)
        == normalize_model_id_for_relaxed_match(requested)
}

/// Configuration error.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Home directory not found")]
    NoHomeDir,
    #[error("Failed to read config: {0}")]
    Read(#[from] std::io::Error),
    #[error("Failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("Failed to serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error(
        "legacy config section `[playwright]` is no longer supported; rename it to `[browser]`"
    )]
    LegacyPlaywrightSection,
    #[error("invalid browser config: {0}")]
    InvalidBrowserConfig(String),
}

/// LLM provider types.
///
/// This represents the type of provider (API format and default base URL).
/// Users configure individual provider instances with credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ProviderType {
    /// Anthropic Claude models
    #[default]
    Anthropic,
    /// `OpenAI` GPT models
    OpenAI,
    /// Google Gemini models
    Google,
    /// GitHub Copilot (OAuth, `OpenAI` format)
    Copilot,
    /// `OpenRouter` (aggregator, `OpenAI` format)
    OpenRouter,
    /// `DeepSeek` (`OpenAI` format)
    DeepSeek,
    /// Ollama (local, `OpenAI` format)
    Ollama,
    /// LM Studio local server (`OpenAI` format)
    LMStudio,
    /// Groq (`OpenAI` format)
    Groq,
    /// Together AI (`OpenAI` format)
    Together,
    /// Mistral AI
    Mistral,
    /// xAI Grok
    XAI,
    /// Custom provider (user specifies `base_url`)
    Custom,
}

impl ProviderType {
    const fn catalog_provider(self) -> Option<aither_models::Provider> {
        match self {
            Self::Anthropic => Some(aither_models::Provider::Anthropic),
            Self::OpenAI => Some(aither_models::Provider::OpenAI),
            Self::Google => Some(aither_models::Provider::Google),
            Self::DeepSeek => Some(aither_models::Provider::DeepSeek),
            Self::Mistral => Some(aither_models::Provider::Mistral),
            Self::XAI => Some(aither_models::Provider::XAI),
            Self::Groq => Some(aither_models::Provider::Groq),
            Self::Together => Some(aither_models::Provider::Together),
            Self::OpenRouter | Self::Ollama | Self::LMStudio | Self::Custom | Self::Copilot => None,
        }
    }

    fn provider_matches_catalog(self, provider: &aither_models::Provider) -> bool {
        match self {
            Self::Copilot => matches!(
                provider,
                aither_models::Provider::OpenAI
                    | aither_models::Provider::Anthropic
                    | aither_models::Provider::Copilot
            ),
            _ => self
                .catalog_provider()
                .is_some_and(|expected| expected == *provider),
        }
    }

    fn supports_canonical_model_id(self, canonical_model_id: &str) -> bool {
        match self {
            Self::Copilot => aither_models::all_models().any(|info| {
                info.id().eq_ignore_ascii_case(canonical_model_id)
                    && self.provider_matches_catalog(info.provider())
            }),
            _ => self.catalog_provider().is_some_and(|provider_kind| {
                aither_models::models_for_provider(&provider_kind)
                    .any(|info| info.id().eq_ignore_ascii_case(canonical_model_id))
            }),
        }
    }

    /// All known provider types for UI display.
    pub const ALL: &'static [Self] = &[
        Self::Anthropic,
        Self::OpenAI,
        Self::Google,
        Self::Copilot,
        Self::OpenRouter,
        Self::DeepSeek,
        Self::Ollama,
        Self::LMStudio,
        Self::Groq,
        Self::Together,
        Self::Mistral,
        Self::XAI,
        Self::Custom,
    ];

    /// Display name for UI.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic (Claude)",
            Self::OpenAI => "OpenAI (GPT)",
            Self::Google => "Google (Gemini)",
            Self::Copilot => "GitHub Copilot",
            Self::OpenRouter => "OpenRouter",
            Self::DeepSeek => "DeepSeek",
            Self::Ollama => "Ollama (Local)",
            Self::LMStudio => "LM Studio",
            Self::Groq => "Groq",
            Self::Together => "Together AI",
            Self::Mistral => "Mistral AI",
            Self::XAI => "xAI (Grok)",
            Self::Custom => "Custom Provider",
        }
    }

    /// Short name for the provider type.
    #[must_use]
    pub const fn short_name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAI => "openai",
            Self::Google => "google",
            Self::Copilot => "copilot",
            Self::OpenRouter => "openrouter",
            Self::DeepSeek => "deepseek",
            Self::Ollama => "ollama",
            Self::LMStudio => "lmstudio",
            Self::Groq => "groq",
            Self::Together => "together",
            Self::Mistral => "mistral",
            Self::XAI => "xai",
            Self::Custom => "custom",
        }
    }

    /// Get the API format for this provider type.
    #[must_use]
    pub const fn api_format(self) -> ApiFormat {
        match self {
            Self::Anthropic => ApiFormat::Anthropic,
            Self::Google => ApiFormat::Gemini,
            _ => ApiFormat::OpenAI,
        }
    }

    /// Get default base URL for this provider type.
    #[must_use]
    pub const fn base_url(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("https://api.anthropic.com"),
            Self::OpenAI => Some("https://api.openai.com/v1"),
            Self::Google => Some("https://generativelanguage.googleapis.com/v1beta"),
            Self::Copilot => Some("https://api.githubcopilot.com"),
            Self::OpenRouter => Some("https://openrouter.ai/api/v1"),
            Self::DeepSeek => Some("https://api.deepseek.com/v1"),
            Self::Ollama => Some("http://localhost:11434/v1"),
            Self::LMStudio => Some("http://127.0.0.1:1234/v1"),
            Self::Groq => Some("https://api.groq.com/openai/v1"),
            Self::Together => Some("https://api.together.xyz/v1"),
            Self::Mistral => Some("https://api.mistral.ai/v1"),
            Self::XAI => Some("https://api.x.ai/v1"),
            Self::Custom => None,
        }
    }

    /// Get the provider string for aither-models lookup.
    #[must_use]
    pub const fn aither_provider(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAI => "openai",
            Self::Google => "google",
            Self::Copilot => "copilot",
            Self::DeepSeek => "deepseek",
            Self::Mistral => "mistral",
            Self::XAI => "xai",
            Self::Groq => "groq",
            Self::Together => "together",
            Self::OpenRouter | Self::Ollama | Self::LMStudio | Self::Custom => "unknown",
        }
    }

    /// Whether this provider type requires an API key.
    /// Custom providers don't require API keys since they might be local servers.
    #[must_use]
    pub const fn requires_api_key(self) -> bool {
        !matches!(
            self,
            Self::Ollama | Self::LMStudio | Self::Copilot | Self::Custom
        )
    }

    /// Whether this provider type requires a base URL to be configured.
    #[must_use]
    pub const fn requires_base_url(self) -> bool {
        matches!(self, Self::Custom)
    }

    /// Whether this provider type uses OAuth authentication.
    #[must_use]
    pub const fn uses_oauth(self) -> bool {
        matches!(self, Self::Copilot)
    }

    /// Parse provider type from an ID string.
    /// Returns Custom if the ID doesn't match any known provider.
    #[must_use]
    pub fn from_id(id: &str) -> Self {
        match id.to_lowercase().as_str() {
            "anthropic" | "claude" => Self::Anthropic,
            "openai" | "gpt" => Self::OpenAI,
            "google" | "gemini" => Self::Google,
            "copilot" | "github" => Self::Copilot,
            "openrouter" => Self::OpenRouter,
            "deepseek" => Self::DeepSeek,
            "ollama" => Self::Ollama,
            "lmstudio" | "lm-studio" => Self::LMStudio,
            "groq" => Self::Groq,
            "together" => Self::Together,
            "mistral" => Self::Mistral,
            "xai" | "grok" => Self::XAI,
            _ => Self::Custom,
        }
    }

    /// Check if this provider type can serve a given model.
    ///
    /// Aggregator providers (`OpenRouter`, Ollama, Custom) accept any model.
    /// Native providers consult the `aither_models` catalog as the single source
    /// of truth. If the model is unknown (not in the catalog), it is not claimable
    /// by any native provider.
    #[must_use]
    pub fn can_serve_model(self, model_id: &str) -> bool {
        if self == Self::OpenRouter
            || self == Self::Ollama
            || self == Self::LMStudio
            || self == Self::Custom
        {
            return true;
        }

        let Some(canonical_model_id) =
            aither_models::lookup(model_id).map(aither_models::ModelEntry::id)
        else {
            return false;
        };

        self.supports_canonical_model_id(canonical_model_id)
    }
}

/// API format types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ApiFormat {
    #[default]
    OpenAI,
    Anthropic,
    Gemini,
}

/// Model tier preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    /// Flagship models - highest capability
    Advanced,
    /// Balanced models - good capability/cost ratio
    #[default]
    Balanced,
    /// Fast models - optimized for speed
    Fast,
    /// Image generation models
    ImageGeneration,
}

impl ModelTier {
    /// All tiers for iteration.
    pub const ALL: &'static [Self] = &[
        Self::Advanced,
        Self::Balanced,
        Self::Fast,
        Self::ImageGeneration,
    ];

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Advanced => "Advanced (Flagship)",
            Self::Balanced => "Balanced",
            Self::Fast => "Fast",
            Self::ImageGeneration => "Image Generation",
        }
    }

    #[must_use]
    pub const fn short_name(self) -> &'static str {
        match self {
            Self::Advanced => "advanced",
            Self::Balanced => "balanced",
            Self::Fast => "fast",
            Self::ImageGeneration => "image",
        }
    }
}

/// A configured provider instance with credentials.
///
/// The provider ID comes from the config section key (e.g., `[provider.anthropic]`).
/// If the ID matches a known provider type, the type is inferred automatically.
/// Otherwise, use the `type` field to specify the provider type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInstance {
    /// The type of provider (inferred from ID if not specified)
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<ProviderType>,
    /// API key (not needed for Ollama/Copilot)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Base URL override (required for Custom, optional for others)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Display name override
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Whether this provider is enabled
    #[serde(default = "default_true")]
    pub enabled: bool,
}

const fn default_true() -> bool {
    true
}

impl Default for ProviderInstance {
    fn default() -> Self {
        Self {
            provider_type: None,
            api_key: None,
            base_url: None,
            display_name: None,
            enabled: true,
        }
    }
}

impl ProviderInstance {
    /// Create a new provider instance with explicit type.
    #[must_use]
    pub fn new(provider_type: ProviderType) -> Self {
        Self {
            provider_type: Some(provider_type),
            ..Default::default()
        }
    }

    /// Set API key.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Set base URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Resolve provider type from ID or explicit type field.
    #[must_use]
    pub fn resolve_type(&self, id: &str) -> ProviderType {
        // If explicit type is set, use it
        if let Some(t) = self.provider_type {
            return t;
        }
        // Try to infer from ID
        ProviderType::from_id(id)
    }

    /// Get effective base URL.
    #[must_use]
    pub fn effective_base_url(&self, id: &str) -> Option<&str> {
        self.base_url
            .as_deref()
            .or_else(|| self.resolve_type(id).base_url())
    }

    /// Get display name.
    #[must_use]
    pub fn display_name_for(&self, id: &str) -> String {
        self.display_name
            .clone()
            .unwrap_or_else(|| self.resolve_type(id).display_name().to_string())
    }

    /// Check if this instance is valid/complete.
    #[must_use]
    pub fn is_valid(&self, id: &str) -> bool {
        if !self.enabled {
            return false;
        }
        let ptype = self.resolve_type(id);
        // Check API key requirement
        if ptype.requires_api_key() && self.api_key.is_none() {
            return false;
        }
        // Custom provider needs base_url
        if ptype.requires_base_url() && self.base_url.is_none() {
            return false;
        }
        true
    }

    /// Check if this provider can serve a model.
    #[must_use]
    pub fn can_serve_model(&self, id: &str, model_id: &str) -> bool {
        if !self.is_valid(id) {
            return false;
        }

        if self.is_generic_model_proxy(id) {
            return true;
        }

        self.resolve_type(id).can_serve_model(model_id)
    }

    /// Whether this instance should be treated as a generic model proxy.
    ///
    /// Generic proxies can serve arbitrary model IDs and should be appended
    /// after explicit provider/model matches in fallback ordering.
    #[must_use]
    pub fn is_generic_model_proxy(&self, id: &str) -> bool {
        let provider_type = self.resolve_type(id);
        if matches!(
            provider_type,
            ProviderType::OpenRouter
                | ProviderType::Ollama
                | ProviderType::LMStudio
                | ProviderType::Custom
        ) {
            return true;
        }

        if !matches!(
            provider_type,
            ProviderType::OpenAI
                | ProviderType::DeepSeek
                | ProviderType::Groq
                | ProviderType::Together
                | ProviderType::Mistral
                | ProviderType::XAI
        ) {
            return false;
        }

        let Some(explicit_base_url) = self.base_url.as_deref() else {
            return false;
        };
        let normalized_explicit = normalize_base_url(explicit_base_url);
        provider_type.base_url().map_or(true, |default_url| {
            normalize_base_url(default_url) != normalized_explicit
        })
    }
}

/// Load balancing strategy for distributing requests across providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalanceStrategy {
    /// Distribute requests evenly across providers (default)
    #[default]
    RoundRobin,
    /// Only use secondary providers when primary fails
    FailoverOnly,
    /// Random selection among available providers
    Random,
}

impl LoadBalanceStrategy {
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::RoundRobin => "Round Robin (Even Distribution)",
            Self::FailoverOnly => "Failover Only",
            Self::Random => "Random",
        }
    }
}

/// Model preferences per tier.
///
/// Each tier has a list of models in priority order.
/// The system tries providers for the first model, then fails over to the next.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelsConfig {
    /// Advanced/flagship models in priority order
    #[serde(default)]
    pub advanced: Vec<String>,
    /// Balanced models in priority order
    #[serde(default)]
    pub balanced: Vec<String>,
    /// Fast models in priority order
    #[serde(default)]
    pub fast: Vec<String>,
    /// Image generation models in priority order
    #[serde(default)]
    pub image_generation: Vec<String>,
    /// Global embedding model used for semantic search/indexing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    /// Optional explicit embedding dimensions for providers that require it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_dimensions: Option<usize>,
    /// Model to provider mapping (`model_id` -> `provider_id`).
    /// Populated when models are added via the tier picker.
    #[serde(default)]
    pub model_providers: HashMap<String, String>,
}

impl ModelsConfig {
    /// Get models for a tier.
    #[must_use]
    pub fn models_for_tier(&self, tier: ModelTier) -> &[String] {
        match tier {
            ModelTier::Advanced => &self.advanced,
            ModelTier::Balanced => &self.balanced,
            ModelTier::Fast => &self.fast,
            ModelTier::ImageGeneration => &self.image_generation,
        }
    }

    /// Get mutable models for a tier.
    pub const fn models_for_tier_mut(&mut self, tier: ModelTier) -> &mut Vec<String> {
        match tier {
            ModelTier::Advanced => &mut self.advanced,
            ModelTier::Balanced => &mut self.balanced,
            ModelTier::Fast => &mut self.fast,
            ModelTier::ImageGeneration => &mut self.image_generation,
        }
    }

    /// Check if any models are configured.
    #[must_use]
    pub const fn has_any_models(&self) -> bool {
        !self.advanced.is_empty()
            || !self.balanced.is_empty()
            || !self.fast.is_empty()
            || !self.image_generation.is_empty()
    }

    /// Check if no models are configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.has_any_models()
    }

    /// Add a model to a tier (avoids duplicates).
    /// Also stores the provider mapping for direct provider lookup during model switching.
    pub fn add_model(&mut self, tier: ModelTier, model_id: &str, provider_id: &str) {
        let list = self.models_for_tier_mut(tier);
        if !list.contains(&model_id.to_string()) {
            list.push(model_id.to_string());
        }
        // Store the provider mapping
        self.model_providers
            .insert(model_id.to_string(), provider_id.to_string());
    }

    /// Get the provider ID for a model (if known).
    #[must_use]
    pub fn provider_for_model(&self, model_id: &str, strict: bool) -> Option<&str> {
        if let Some(provider) = self.model_providers.get(model_id) {
            return Some(provider.as_str());
        }

        if strict {
            return None;
        }

        self.model_providers
            .iter()
            .find(|(saved, _)| model_ids_match(saved, model_id, MatchMode::Relaxed))
            .map(|(_, provider)| provider.as_str())
    }

    #[must_use]
    pub fn embedding_model(&self) -> Option<&str> {
        self.embedding_model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SshServerConfig {
    pub name: String,
    pub target: String,
}

/// Global preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preferences {
    /// Load balancing strategy
    #[serde(default)]
    pub load_balance_strategy: LoadBalanceStrategy,
    /// Warn user when changing model mid-conversation
    #[serde(default = "default_true")]
    pub warn_on_model_change: bool,
    /// Show token/context usage in chat UI.
    #[serde(default = "default_true")]
    pub show_token_usage: bool,
    /// Strict model matching (no relaxed fallback aliases).
    #[serde(default)]
    pub strict_model_matching: bool,
    /// Global network restriction settings.
    #[serde(default)]
    pub network_policy: NetworkPolicyConfig,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            load_balance_strategy: LoadBalanceStrategy::RoundRobin,
            warn_on_model_change: true,
            show_token_usage: true,
            strict_model_matching: false,
            network_policy: NetworkPolicyConfig::default(),
        }
    }
}

/// UI appearance preferences persisted in config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Appearance {
    #[serde(default = "default_ui_font")]
    pub font: String,
    #[serde(default = "default_ui_theme")]
    pub theme: String,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            font: default_ui_font(),
            theme: default_ui_theme(),
        }
    }
}

fn default_ui_font() -> String {
    "Inter".to_string()
}

fn default_ui_theme() -> String {
    "system".to_string()
}

/// Browser automation configuration.
///
/// When enabled, May connects sessions to a browser runtime that May owns and
/// manages. The runtime may be provisioned locally by May or exposed through a
/// remote CDP endpoint. Agents use the May-managed `browser` command and must
/// not install or manage browser binaries themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserBackendKind {
    Managed,
    RemoteCdp,
}

impl Default for BrowserBackendKind {
    fn default() -> Self {
        Self::Managed
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserConfig {
    /// Whether browser automation is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// How May reaches the browser runtime.
    #[serde(default)]
    pub backend: BrowserBackendKind,
    /// CDP remote debugging port on the host.
    #[serde(default = "default_browser_port")]
    pub port: u16,
    /// CDP endpoint for a remotely managed browser runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_cdp_endpoint: Option<String>,
    /// JPEG quality for screencast frames (1-100). Lower = less bandwidth.
    #[serde(default = "default_screencast_quality")]
    pub screencast_quality: u32,
    /// Max width for screencast frames in pixels. Lower = less bandwidth.
    #[serde(default = "default_screencast_max_width")]
    pub screencast_max_width: u32,
}

const fn default_browser_port() -> u16 {
    39222
}

const fn default_screencast_quality() -> u32 {
    60
}

const fn default_screencast_max_width() -> u32 {
    1280
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: BrowserBackendKind::Managed,
            port: default_browser_port(),
            remote_cdp_endpoint: None,
            screencast_quality: default_screencast_quality(),
            screencast_max_width: default_screencast_max_width(),
        }
    }
}

impl BrowserConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.backend == BrowserBackendKind::RemoteCdp
            && self
                .remote_cdp_endpoint
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            return Err(ConfigError::InvalidBrowserConfig(
                "browser.remote_cdp_endpoint is required when browser.backend = \"remote_cdp\""
                    .to_string(),
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn requires_managed_runtime(&self) -> bool {
        matches!(self.backend, BrowserBackendKind::Managed)
    }

    pub fn remote_cdp_endpoint(&self) -> Result<&str, ConfigError> {
        self.remote_cdp_endpoint
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ConfigError::InvalidBrowserConfig(
                    "browser.remote_cdp_endpoint is required when browser.backend = \"remote_cdp\""
                        .to_string(),
                )
            })
    }
}

/// Cloudflare-based remote access configuration for the May UI server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAccessConfig {
    /// Whether the server should keep remote access enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Cloudflared executable path or command name.
    #[serde(default = "default_cloudflared_binary")]
    pub cloudflared_binary: String,
    /// Remotely managed Cloudflare tunnel token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_token: Option<String>,
    /// Public HTTPS URL routed to this server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// Whether local-environment profiles may be exposed remotely.
    #[serde(default)]
    pub allow_local_environment: bool,
}

fn default_cloudflared_binary() -> String {
    "cloudflared".to_string()
}

impl Default for RemoteAccessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cloudflared_binary: default_cloudflared_binary(),
            tunnel_token: None,
            public_url: None,
            allow_local_environment: false,
        }
    }
}

/// May agent configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MayConfig {
    /// Maximum iterations per query.
    ///
    /// `0` means unlimited.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    /// Configured provider instances, keyed by ID.
    /// Example: `[provider.anthropic]`, `[provider.my-ollama]`
    #[serde(default)]
    pub provider: HashMap<String, ProviderInstance>,
    /// Model preferences per tier.
    #[serde(default)]
    pub models: ModelsConfig,
    /// Global preferences.
    #[serde(default)]
    pub preferences: Preferences,
    /// UI appearance settings.
    #[serde(default)]
    pub appearance: Appearance,
    /// Managed browser automation settings.
    #[serde(default)]
    pub browser: BrowserConfig,
    /// Remote access settings.
    #[serde(default)]
    pub remote_access: RemoteAccessConfig,
    /// Last model selected via /model command (used as default for new sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model: Option<String>,
    /// Preconfigured SSH targets available to the agent.
    #[serde(default)]
    pub ssh_servers: Vec<SshServerConfig>,
}

const fn default_max_iterations() -> usize {
    0
}

impl Default for MayConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
            provider: HashMap::new(),
            models: ModelsConfig::default(),
            preferences: Preferences::default(),
            appearance: Appearance::default(),
            browser: BrowserConfig::default(),
            remote_access: RemoteAccessConfig::default(),
            last_model: None,
            ssh_servers: Vec::new(),
        }
    }
}

impl MayConfig {
    /// Get the May directory path (~/.may/).
    pub fn may_dir() -> Result<PathBuf, ConfigError> {
        dirs::home_dir()
            .map(|h| h.join(".may"))
            .ok_or(ConfigError::NoHomeDir)
    }

    /// Get the config file path (~/.may/config.toml).
    pub fn config_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::may_dir()?.join("config.toml"))
    }

    /// Load configuration from ~/.may/config.toml or use defaults.
    pub async fn load() -> Result<Self, ConfigError> {
        let path = Self::config_path()?;
        Self::load_from(&path).await
    }

    /// Load configuration from a specific path.
    pub async fn load_from(path: &Path) -> Result<Self, ConfigError> {
        match async_fs::read_to_string(path).await {
            Ok(content) => {
                reject_legacy_config_sections(&content)?;
                let config: Self = toml::from_str(&content)?;
                config.validate()?;
                Ok(config)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(ConfigError::Read(error)),
        }
    }

    /// Save configuration to ~/.may/config.toml.
    pub async fn save(&self) -> Result<(), ConfigError> {
        let path = Self::config_path()?;
        self.save_to(&path).await
    }

    /// Save configuration to a specific path.
    pub async fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            async_fs::create_dir_all(parent).await?;
        }
        let content = toml::to_string_pretty(self)?;
        async_fs::write(path, content).await?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        self.browser.validate()
    }

    /// Check if configuration is complete enough to use.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        // Need at least one valid provider
        let has_provider = self.provider.iter().any(|(id, p)| p.is_valid(id));
        // Need at least one model configured
        let has_models = self.models.has_any_models();
        has_provider && has_models
    }

    /// Get a provider by ID.
    #[must_use]
    pub fn get_provider(&self, id: &str) -> Option<&ProviderInstance> {
        self.provider.get(id)
    }

    /// Get a mutable provider by ID.
    pub fn get_provider_mut(&mut self, id: &str) -> Option<&mut ProviderInstance> {
        self.provider.get_mut(id)
    }

    /// Add or update a provider.
    pub fn upsert_provider(&mut self, id: impl Into<String>, provider: ProviderInstance) {
        self.provider.insert(id.into(), provider);
    }

    /// Remove a provider by ID.
    pub fn remove_provider(&mut self, id: &str) {
        self.provider.remove(id);
    }

    /// Get all valid provider IDs that can serve a model.
    #[must_use]
    pub fn providers_for_model(&self, model_id: &str) -> Vec<&str> {
        self.provider
            .iter()
            .filter(|(id, p)| p.can_serve_model(id, model_id))
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// Get all provider IDs.
    pub fn provider_ids(&self) -> impl Iterator<Item = &str> {
        self.provider.keys().map(std::string::String::as_str)
    }

    /// Get configured SSH servers.
    #[must_use]
    pub fn ssh_servers(&self) -> &[SshServerConfig] {
        &self.ssh_servers
    }

    /// Add or update an SSH server by name.
    pub fn upsert_ssh_server(&mut self, server: SshServerConfig) {
        if let Some(existing) = self.ssh_servers.iter_mut().find(|s| s.name == server.name) {
            *existing = server;
        } else {
            self.ssh_servers.push(server);
        }
    }

    /// Remove an SSH server by name. Returns true if removed.
    pub fn remove_ssh_server(&mut self, name: &str) -> bool {
        let before = self.ssh_servers.len();
        self.ssh_servers.retain(|s| s.name != name);
        before != self.ssh_servers.len()
    }

    /// Get the first available model for a tier.
    #[must_use]
    pub fn default_model_for_tier(&self, tier: ModelTier) -> Option<&str> {
        self.models
            .models_for_tier(tier)
            .first()
            .map(std::string::String::as_str)
    }

    /// Create a provider resolver for runtime model selection.
    #[must_use]
    pub fn create_resolver(&self) -> ProviderResolver {
        ProviderResolver::new(self)
    }

    /// Get all available models from configured providers.
    ///
    /// For known providers (anthropic, openai, google), uses `aither_models` database.
    /// For providers that can serve any model (openrouter, ollama, custom), includes
    /// all known models since they can proxy to multiple backends.
    #[must_use]
    pub fn available_models(&self) -> Vec<AvailableModel> {
        let mut models = Vec::new();
        let mut has_any_model_provider = false;
        let chat_price_breakpoints = chat_price_breakpoints();

        for (id, instance) in &self.provider {
            if !instance.is_valid(id) {
                continue;
            }

            let ptype = instance.resolve_type(id);

            // Providers that can serve arbitrary models (proxies/aggregators)
            if instance.is_generic_model_proxy(id) {
                has_any_model_provider = true;
                continue;
            }

            let provider_models = if ptype == ProviderType::Copilot {
                aither_models::all_models()
                    .filter(|info| ptype.provider_matches_catalog(info.provider()))
                    .collect::<Vec<_>>()
            } else {
                let Some(provider_kind) = ptype.catalog_provider() else {
                    continue;
                };
                aither_models::models_for_provider(&provider_kind).collect::<Vec<_>>()
            };

            for info in provider_models {
                if info.is_deprecated() {
                    continue;
                }
                models.push(AvailableModel {
                    id: info.id().to_string(),
                    provider_id: id.clone(),
                    tier: Some(infer_model_tier(info, chat_price_breakpoints)),
                });
            }
        }

        // If we have an "any model" provider (OpenRouter, Ollama, custom),
        // include ALL known models since they can potentially serve them
        if has_any_model_provider {
            for info in aither_models::all_models() {
                if info.is_deprecated() {
                    continue;
                }
                // Skip if we already have this model from a direct provider
                if models.iter().any(|m| m.id == info.id()) {
                    continue;
                }
                models.push(AvailableModel {
                    id: info.id().to_string(),
                    provider_id: String::new(), // Multiple providers could serve this
                    tier: Some(infer_model_tier(info, chat_price_breakpoints)),
                });
            }
        }

        // Sort by model ID
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models
    }
}

fn reject_legacy_config_sections(content: &str) -> Result<(), ConfigError> {
    let document = toml::from_str::<toml::Value>(content).map_err(ConfigError::Parse)?;
    if document.get("playwright").is_some() {
        return Err(ConfigError::LegacyPlaywrightSection);
    }
    Ok(())
}

/// An available model from a configured provider.
#[derive(Debug, Clone)]
pub struct AvailableModel {
    /// Model ID
    pub id: String,
    /// Provider ID that can serve this model
    pub provider_id: String,
    /// Suggested tier for this model
    pub tier: Option<ModelTier>,
}

/// Runtime resolver for selecting providers for models.
///
/// Handles load balancing and failover across providers.
pub struct ProviderResolver {
    /// Provider instances (cloned from config), keyed by ID
    providers: HashMap<String, ProviderInstance>,
    /// Models config
    models: ModelsConfig,
    /// Load balance strategy
    strategy: LoadBalanceStrategy,
    /// Round-robin counter per model
    counters: HashMap<String, AtomicUsize>,
    /// Temporarily failed providers (`model_id` -> `provider_ids`).
    /// Uses `RefCell` because `ProviderResolver` is always thread-local (created per-request).
    failed: std::cell::RefCell<HashMap<String, Vec<String>>>,
}

/// A selected provider with its ID.
#[derive(Debug, Clone)]
pub struct SelectedProvider<'a> {
    /// The provider ID
    pub id: &'a str,
    /// The provider instance
    pub instance: &'a ProviderInstance,
}

impl ProviderResolver {
    /// Create a new resolver from config.
    #[must_use]
    pub fn new(config: &MayConfig) -> Self {
        Self {
            providers: config.provider.clone(),
            models: config.models.clone(),
            strategy: config.preferences.load_balance_strategy,
            counters: HashMap::new(),
            failed: std::cell::RefCell::new(HashMap::new()),
        }
    }

    /// Get all provider IDs that can serve a model, excluding failed ones.
    fn available_provider_ids(&self, model_id: &str) -> Vec<&str> {
        let failed = self.failed.borrow();
        let failed_ids = failed.get(model_id);

        let mut explicit = Vec::new();
        let mut generic = Vec::new();

        for (id, p) in &self.providers {
            if !p.can_serve_model(id, model_id) || failed_ids.is_some_and(|list| list.contains(id))
            {
                continue;
            }

            if p.is_generic_model_proxy(id) {
                generic.push(id.as_str());
            } else {
                explicit.push(id.as_str());
            }
        }

        // Prefer explicit provider-model matches first; keep generic aggregators as fallback.
        explicit.sort_unstable();
        generic.sort_unstable();
        explicit.extend(generic);
        explicit
    }

    /// Select the next provider for a model based on strategy.
    pub fn select_provider(&self, model_id: &str) -> Option<SelectedProvider<'_>> {
        let available = self.available_provider_ids(model_id);
        if available.is_empty() {
            return None;
        }

        let id = match self.strategy {
            LoadBalanceStrategy::FailoverOnly => available.first().copied(),
            LoadBalanceStrategy::Random => {
                use std::collections::hash_map::RandomState;
                use std::hash::{BuildHasher, Hasher};
                let idx = RandomState::new().build_hasher().finish() as usize % available.len();
                available.get(idx).copied()
            }
            LoadBalanceStrategy::RoundRobin => {
                // Get or create counter for this model
                let counter = self
                    .counters
                    .get(model_id)
                    .map_or(0, |c| c.fetch_add(1, Ordering::Relaxed));
                let idx = counter % available.len();
                available.get(idx).copied()
            }
        }?;

        self.providers
            .get(id)
            .map(|instance| SelectedProvider { id, instance })
    }

    /// Get a provider by ID.
    pub fn get_provider(&self, id: &str) -> Option<&ProviderInstance> {
        self.providers.get(id)
    }

    /// Mark a provider as failed for a model.
    pub fn mark_failed(&self, model_id: &str, provider_id: &str) {
        let mut failed = self.failed.borrow_mut();
        failed
            .entry(model_id.to_string())
            .or_default()
            .push(provider_id.to_string());
    }

    /// Clear failed status for a model (e.g., after some time).
    pub fn clear_failed(&self, model_id: &str) {
        let mut failed = self.failed.borrow_mut();
        failed.remove(model_id);
    }

    /// Clear all failed status.
    pub fn clear_all_failed(&self) {
        let mut failed = self.failed.borrow_mut();
        failed.clear();
    }

    /// Check if any providers are available for a model.
    pub fn has_available_provider(&self, model_id: &str) -> bool {
        !self.available_provider_ids(model_id).is_empty()
    }

    /// Get the next model in a tier after the current one fails.
    pub fn next_model_in_tier(&self, tier: ModelTier, current_model: &str) -> Option<&str> {
        let models = self.models.models_for_tier(tier);
        let current_idx = models.iter().position(|m| m == current_model)?;
        models.get(current_idx + 1).map(std::string::String::as_str)
    }

    /// Get the first model in a tier.
    pub fn first_model_in_tier(&self, tier: ModelTier) -> Option<&str> {
        self.models
            .models_for_tier(tier)
            .first()
            .map(std::string::String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = MayConfig::default();
        assert_eq!(config.max_iterations, 0);
        assert!(config.provider.is_empty());
        assert!(!config.is_configured());
    }

    #[test]
    fn test_parse_new_config() {
        let toml = include_str!("testdata/config_parse_new.toml");
        let config: MayConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.max_iterations, 100);
        assert_eq!(config.provider.len(), 2);

        // Check anthropic provider
        let anthropic = config.get_provider("anthropic").unwrap();
        assert_eq!(anthropic.resolve_type("anthropic"), ProviderType::Anthropic);
        assert_eq!(anthropic.api_key.as_deref(), Some("sk-ant-test"));

        assert_eq!(config.models.balanced.len(), 2);
        assert!(config.is_configured());
    }

    #[test]
    fn test_parse_custom_provider() {
        let toml = include_str!("testdata/config_parse_custom_provider.toml");
        let config: MayConfig = toml::from_str(toml).unwrap();
        let provider = config.get_provider("my-ollama").unwrap();
        assert_eq!(provider.resolve_type("my-ollama"), ProviderType::Ollama);
        assert_eq!(
            provider.base_url.as_deref(),
            Some("http://192.168.1.100:11434/v1")
        );
    }

    #[test]
    fn test_browser_config_defaults_to_managed_backend() {
        let config = MayConfig::default();
        assert_eq!(config.browser.backend, BrowserBackendKind::Managed);
        assert!(config.browser.requires_managed_runtime());
    }

    #[test]
    fn test_browser_config_requires_remote_endpoint_for_remote_backend() {
        let mut config = MayConfig::default();
        config.browser.enabled = true;
        config.browser.backend = BrowserBackendKind::RemoteCdp;
        let error = config
            .validate()
            .expect_err("remote browser must require endpoint");
        assert!(
            error
                .to_string()
                .contains("browser.remote_cdp_endpoint is required")
        );
    }

    #[test]
    fn test_browser_config_accepts_remote_backend_with_endpoint() {
        let mut config = MayConfig::default();
        config.browser.enabled = true;
        config.browser.backend = BrowserBackendKind::RemoteCdp;
        config.browser.remote_cdp_endpoint = Some("wss://browser.example/ws".to_string());
        config
            .validate()
            .expect("remote browser config should validate");
        assert_eq!(
            config
                .browser
                .remote_cdp_endpoint()
                .expect("endpoint must resolve"),
            "wss://browser.example/ws"
        );
    }

    #[test]
    fn test_provider_type_from_id() {
        assert_eq!(ProviderType::from_id("anthropic"), ProviderType::Anthropic);
        assert_eq!(ProviderType::from_id("claude"), ProviderType::Anthropic);
        assert_eq!(ProviderType::from_id("openai"), ProviderType::OpenAI);
        assert_eq!(ProviderType::from_id("google"), ProviderType::Google);
        assert_eq!(ProviderType::from_id("gemini"), ProviderType::Google);
        assert_eq!(ProviderType::from_id("lmstudio"), ProviderType::LMStudio);
        assert_eq!(ProviderType::from_id("lm-studio"), ProviderType::LMStudio);
        assert_eq!(
            ProviderType::from_id("openrouter"),
            ProviderType::OpenRouter
        );
        assert_eq!(
            ProviderType::from_id("my-custom-thing"),
            ProviderType::Custom
        );
    }

    #[test]
    fn test_provider_instance_validity() {
        // Valid anthropic provider (type inferred from ID)
        let valid = ProviderInstance::default().with_api_key("key");
        assert!(valid.is_valid("anthropic"));

        // Invalid - missing API key
        let invalid = ProviderInstance::default();
        assert!(!invalid.is_valid("anthropic"));

        // Ollama doesn't need API key
        let ollama = ProviderInstance::default();
        assert!(ollama.is_valid("ollama"));

        // LM Studio doesn't need API key
        let lmstudio = ProviderInstance::new(ProviderType::LMStudio);
        assert!(lmstudio.is_valid("lmstudio"));

        // Custom needs base_url (but not API key - might be local server)
        let custom_invalid = ProviderInstance::default();
        assert!(!custom_invalid.is_valid("my-custom"));

        // Custom with base_url is valid (API key optional)
        let custom_valid =
            ProviderInstance::new(ProviderType::Custom).with_base_url("http://localhost:8080");
        assert!(custom_valid.is_valid("my-custom"));
    }

    #[test]
    fn test_provider_can_serve_model() {
        let anthropic_model = "claude-sonnet-4-20250514";

        // OpenAI can serve GPT models even when catalog canonical IDs collide across providers.
        assert!(ProviderType::OpenAI.can_serve_model("gpt-4o"));

        // Anthropic can serve claude models
        assert!(ProviderType::Anthropic.can_serve_model(anthropic_model));

        // OpenRouter can serve anything
        assert!(ProviderType::OpenRouter.can_serve_model(anthropic_model));
        assert!(ProviderType::OpenRouter.can_serve_model("gpt-4o"));

        // LM Studio can serve arbitrary local model ids
        assert!(ProviderType::LMStudio.can_serve_model("qwen3-embedding-0.6b-dwq"));

        // Copilot can serve OpenAI and Anthropic models, but not Google models
        assert!(ProviderType::Copilot.can_serve_model("gpt-4o"));
        assert!(ProviderType::Copilot.can_serve_model(anthropic_model));
        assert!(!ProviderType::Copilot.can_serve_model("gemini-3-flash-preview"));
    }

    #[test]
    fn test_openai_custom_endpoint_treated_as_generic_proxy() {
        let official = ProviderInstance::new(ProviderType::OpenAI).with_api_key("sk-openai");
        assert!(!official.is_generic_model_proxy("openai"));
        assert!(!official.can_serve_model("openai", "gemini-3-flash-preview"));

        let proxy = ProviderInstance::new(ProviderType::OpenAI)
            .with_api_key("sk-proxy")
            .with_base_url("https://proxy.example.com/v1");
        assert!(proxy.is_generic_model_proxy("proxy-openai"));
        assert!(proxy.can_serve_model("proxy-openai", "gemini-3-flash-preview"));

        let lmstudio = ProviderInstance::new(ProviderType::LMStudio);
        assert!(lmstudio.is_generic_model_proxy("lmstudio"));
        assert!(lmstudio.can_serve_model("lmstudio", "qwen3-embedding-0.6b-dwq"));
    }

    #[test]
    fn test_providers_for_model() {
        let anthropic_model = "claude-sonnet-4-20250514";
        let mut config = MayConfig::default();
        config.upsert_provider("anthropic", ProviderInstance::default().with_api_key("key"));
        config.upsert_provider(
            "openrouter",
            ProviderInstance::default().with_api_key("key"),
        );
        config.models.balanced = vec![anthropic_model.to_string()];

        let providers = config.providers_for_model(anthropic_model);
        assert_eq!(providers.len(), 2); // Both can serve it
    }

    #[test]
    fn test_resolver_failover() {
        let anthropic_model = "claude-sonnet-4-20250514";
        let mut config = MayConfig::default();
        config.upsert_provider(
            "anthropic",
            ProviderInstance::default().with_api_key("key1"),
        );
        config.upsert_provider(
            "openrouter",
            ProviderInstance::default().with_api_key("key2"),
        );
        config.models.balanced = vec![anthropic_model.to_string()];
        config.preferences.load_balance_strategy = LoadBalanceStrategy::FailoverOnly;

        let resolver = config.create_resolver();

        // First selection should pick one of the providers
        let first = resolver.select_provider(anthropic_model);
        assert!(first.is_some());
        let first_id = first.unwrap().id;

        // Mark first as failed
        resolver.mark_failed(anthropic_model, first_id);

        // Now should select the other provider
        let second = resolver.select_provider(anthropic_model);
        assert!(second.is_some());
        assert_ne!(second.unwrap().id, first_id);

        // Clear failures
        resolver.clear_failed(anthropic_model);
        assert!(resolver.select_provider(anthropic_model).is_some());
    }

    #[test]
    fn test_serialize_config() {
        let mut config = MayConfig::default();
        config.upsert_provider(
            "anthropic",
            ProviderInstance::default().with_api_key("test-key"),
        );
        config.models.balanced = vec!["claude-sonnet-4".to_string()];

        let toml = toml::to_string_pretty(&config).unwrap();
        assert!(toml.contains("[provider.anthropic]"));
        assert!(toml.contains("[models]"));
    }
}
