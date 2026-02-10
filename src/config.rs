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
//!   balanced, fast, image_generation) can have multiple models in priority order.
//!   The system will try providers in order and failover to the next model if all fail.
//!
//! - **Preferences**: Global settings like load balancing strategy and warnings.
//!
//! # Example config
//!
//! ```toml
//! max_iterations = 50
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

use aither_models::ModelTier as AitherTier;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Get model tier from aither-models database.
/// Returns the primary tier if found, otherwise returns Balanced as default.
pub fn get_model_tier(model_id: &str) -> ModelTier {
    aither_models::lookup(model_id)
        .and_then(|info| info.primary_tier())
        .map(|tier| match tier {
            AitherTier::Flagship => ModelTier::Advanced,
            AitherTier::Balanced => ModelTier::Balanced,
            AitherTier::Fast => ModelTier::Fast,
        })
        .unwrap_or(ModelTier::Balanced)
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
    /// OpenAI GPT models
    OpenAI,
    /// Google Gemini models
    Google,
    /// GitHub Copilot (OAuth, OpenAI format)
    Copilot,
    /// OpenRouter (aggregator, OpenAI format)
    OpenRouter,
    /// DeepSeek (OpenAI format)
    DeepSeek,
    /// Ollama (local, OpenAI format)
    Ollama,
    /// Groq (OpenAI format)
    Groq,
    /// Together AI (OpenAI format)
    Together,
    /// Mistral AI
    Mistral,
    /// xAI Grok
    XAI,
    /// Custom provider (user specifies base_url)
    Custom,
}

impl ProviderType {
    /// All known provider types for UI display.
    pub const ALL: &'static [ProviderType] = &[
        ProviderType::Anthropic,
        ProviderType::OpenAI,
        ProviderType::Google,
        ProviderType::Copilot,
        ProviderType::OpenRouter,
        ProviderType::DeepSeek,
        ProviderType::Ollama,
        ProviderType::Groq,
        ProviderType::Together,
        ProviderType::Mistral,
        ProviderType::XAI,
        ProviderType::Custom,
    ];

    /// Display name for UI.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic (Claude)",
            Self::OpenAI => "OpenAI (GPT)",
            Self::Google => "Google (Gemini)",
            Self::Copilot => "GitHub Copilot",
            Self::OpenRouter => "OpenRouter",
            Self::DeepSeek => "DeepSeek",
            Self::Ollama => "Ollama (Local)",
            Self::Groq => "Groq",
            Self::Together => "Together AI",
            Self::Mistral => "Mistral AI",
            Self::XAI => "xAI (Grok)",
            Self::Custom => "Custom Provider",
        }
    }

    /// Short name for the provider type.
    pub fn short_name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAI => "openai",
            Self::Google => "google",
            Self::Copilot => "copilot",
            Self::OpenRouter => "openrouter",
            Self::DeepSeek => "deepseek",
            Self::Ollama => "ollama",
            Self::Groq => "groq",
            Self::Together => "together",
            Self::Mistral => "mistral",
            Self::XAI => "xai",
            Self::Custom => "custom",
        }
    }

    /// Get the API format for this provider type.
    pub fn api_format(self) -> ApiFormat {
        match self {
            Self::Anthropic => ApiFormat::Anthropic,
            Self::Google => ApiFormat::Gemini,
            _ => ApiFormat::OpenAI,
        }
    }

    /// Get default base URL for this provider type.
    pub fn base_url(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("https://api.anthropic.com"),
            Self::OpenAI => Some("https://api.openai.com/v1"),
            Self::Google => Some("https://generativelanguage.googleapis.com/v1beta"),
            Self::Copilot => Some("https://api.githubcopilot.com"),
            Self::OpenRouter => Some("https://openrouter.ai/api/v1"),
            Self::DeepSeek => Some("https://api.deepseek.com/v1"),
            Self::Ollama => Some("http://localhost:11434/v1"),
            Self::Groq => Some("https://api.groq.com/openai/v1"),
            Self::Together => Some("https://api.together.xyz/v1"),
            Self::Mistral => Some("https://api.mistral.ai/v1"),
            Self::XAI => Some("https://api.x.ai/v1"),
            Self::Custom => None,
        }
    }

    /// Get the provider string for aither-models lookup.
    pub fn aither_provider(self) -> &'static str {
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
            Self::OpenRouter | Self::Ollama | Self::Custom => "unknown",
        }
    }

    /// Whether this provider type requires an API key.
    /// Custom providers don't require API keys since they might be local servers.
    pub fn requires_api_key(self) -> bool {
        !matches!(self, Self::Ollama | Self::Copilot | Self::Custom)
    }

    /// Whether this provider type requires a base URL to be configured.
    pub fn requires_base_url(self) -> bool {
        matches!(self, Self::Custom)
    }

    /// Whether this provider type uses OAuth authentication.
    pub fn uses_oauth(self) -> bool {
        matches!(self, Self::Copilot)
    }

    /// Parse provider type from an ID string.
    /// Returns Custom if the ID doesn't match any known provider.
    pub fn from_id(id: &str) -> Self {
        match id.to_lowercase().as_str() {
            "anthropic" | "claude" => Self::Anthropic,
            "openai" | "gpt" => Self::OpenAI,
            "google" | "gemini" => Self::Google,
            "copilot" | "github" => Self::Copilot,
            "openrouter" => Self::OpenRouter,
            "deepseek" => Self::DeepSeek,
            "ollama" => Self::Ollama,
            "groq" => Self::Groq,
            "together" => Self::Together,
            "mistral" => Self::Mistral,
            "xai" | "grok" => Self::XAI,
            _ => Self::Custom,
        }
    }

    /// Check if this provider type can serve a given model.
    ///
    /// OpenRouter can serve most models from other providers.
    /// Native providers can only serve their own models.
    pub fn can_serve_model(self, model_id: &str) -> bool {
        // OpenRouter is an aggregator - it can serve most models
        if self == Self::OpenRouter {
            return true;
        }

        // Ollama can run any model that's been pulled locally
        if self == Self::Ollama {
            return true;
        }

        // Custom providers - we don't know what they support
        if self == Self::Custom {
            return true;
        }

        // Copilot can serve OpenAI and Anthropic models, but not Google models.
        if self == Self::Copilot {
            if let Some(info) = aither_models::lookup(model_id) {
                return info.provider.eq_ignore_ascii_case("openai")
                    || info.provider.eq_ignore_ascii_case("anthropic");
            }
            // Unknown model - allow the provider to try
            return true;
        }

        // For native providers, check if the model belongs to them
        if let Some(info) = aither_models::lookup(model_id) {
            info.provider.eq_ignore_ascii_case(self.aither_provider())
        } else {
            // Unknown model - allow the provider to try
            true
        }
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
    pub const ALL: &'static [ModelTier] = &[
        ModelTier::Advanced,
        ModelTier::Balanced,
        ModelTier::Fast,
        ModelTier::ImageGeneration,
    ];

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Advanced => "Advanced (Flagship)",
            Self::Balanced => "Balanced",
            Self::Fast => "Fast",
            Self::ImageGeneration => "Image Generation",
        }
    }

    pub fn short_name(self) -> &'static str {
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

fn default_true() -> bool {
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
    pub fn resolve_type(&self, id: &str) -> ProviderType {
        // If explicit type is set, use it
        if let Some(t) = self.provider_type {
            return t;
        }
        // Try to infer from ID
        ProviderType::from_id(id)
    }

    /// Get effective base URL.
    pub fn effective_base_url(&self, id: &str) -> Option<&str> {
        self.base_url
            .as_deref()
            .or_else(|| self.resolve_type(id).base_url())
    }

    /// Get display name.
    pub fn display_name_for(&self, id: &str) -> String {
        self.display_name
            .clone()
            .unwrap_or_else(|| self.resolve_type(id).display_name().to_string())
    }

    /// Check if this instance is valid/complete.
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
    pub fn can_serve_model(&self, id: &str, model_id: &str) -> bool {
        self.is_valid(id) && self.resolve_type(id).can_serve_model(model_id)
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
    pub fn display_name(self) -> &'static str {
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
    /// Model to provider mapping (model_id -> provider_id).
    /// Populated when models are added via the tier picker.
    #[serde(default)]
    pub model_providers: HashMap<String, String>,
}

impl ModelsConfig {
    /// Get models for a tier.
    pub fn models_for_tier(&self, tier: ModelTier) -> &[String] {
        match tier {
            ModelTier::Advanced => &self.advanced,
            ModelTier::Balanced => &self.balanced,
            ModelTier::Fast => &self.fast,
            ModelTier::ImageGeneration => &self.image_generation,
        }
    }

    /// Get mutable models for a tier.
    pub fn models_for_tier_mut(&mut self, tier: ModelTier) -> &mut Vec<String> {
        match tier {
            ModelTier::Advanced => &mut self.advanced,
            ModelTier::Balanced => &mut self.balanced,
            ModelTier::Fast => &mut self.fast,
            ModelTier::ImageGeneration => &mut self.image_generation,
        }
    }

    /// Check if any models are configured.
    pub fn has_any_models(&self) -> bool {
        !self.advanced.is_empty()
            || !self.balanced.is_empty()
            || !self.fast.is_empty()
            || !self.image_generation.is_empty()
    }

    /// Check if no models are configured.
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
    pub fn provider_for_model(&self, model_id: &str) -> Option<&str> {
        self.model_providers.get(model_id).map(|s| s.as_str())
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
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            load_balance_strategy: LoadBalanceStrategy::RoundRobin,
            warn_on_model_change: true,
        }
    }
}

/// May agent configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MayConfig {
    /// Maximum iterations per query.
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
    /// Last model selected via /model command (used as default for new sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model: Option<String>,
    /// Preconfigured SSH targets available to the agent.
    #[serde(default)]
    pub ssh_servers: Vec<SshServerConfig>,
}

fn default_max_iterations() -> usize {
    50
}

impl Default for MayConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
            provider: HashMap::new(),
            models: ModelsConfig::default(),
            preferences: Preferences::default(),
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
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::config_path()?;
        Self::load_from(&path)
    }

    /// Load configuration from a specific path.
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            Ok(toml::from_str(&content)?)
        } else {
            Ok(Self::default())
        }
    }

    /// Save configuration to ~/.may/config.toml.
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = Self::config_path()?;
        self.save_to(&path)
    }

    /// Save configuration to a specific path.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Check if configuration is complete enough to use.
    pub fn is_configured(&self) -> bool {
        // Need at least one valid provider
        let has_provider = self.provider.iter().any(|(id, p)| p.is_valid(id));
        // Need at least one model configured
        let has_models = self.models.has_any_models();
        has_provider && has_models
    }

    /// Get a provider by ID.
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
    pub fn providers_for_model(&self, model_id: &str) -> Vec<&str> {
        self.provider
            .iter()
            .filter(|(id, p)| p.can_serve_model(id, model_id))
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// Get all provider IDs.
    pub fn provider_ids(&self) -> impl Iterator<Item = &str> {
        self.provider.keys().map(|s| s.as_str())
    }

    /// Get configured SSH servers.
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
    pub fn default_model_for_tier(&self, tier: ModelTier) -> Option<&str> {
        self.models
            .models_for_tier(tier)
            .first()
            .map(|s| s.as_str())
    }

    /// Create a provider resolver for runtime model selection.
    pub fn create_resolver(&self) -> ProviderResolver {
        ProviderResolver::new(self)
    }

    /// Get all available models from configured providers.
    ///
    /// For known providers (anthropic, openai, google), uses aither_models database.
    /// For providers that can serve any model (openrouter, ollama, custom), includes
    /// all known models since they can proxy to multiple backends.
    pub fn available_models(&self) -> Vec<AvailableModel> {
        use aither_models::ModelTier as AitherTier;

        let mut models = Vec::new();
        let mut has_any_model_provider = false;

        for (id, instance) in &self.provider {
            if !instance.is_valid(id) {
                continue;
            }

            let ptype = instance.resolve_type(id);

            // Providers that can serve any model (proxies/aggregators)
            if matches!(
                ptype,
                ProviderType::OpenRouter | ProviderType::Ollama | ProviderType::Custom
            ) {
                has_any_model_provider = true;
                continue;
            }

            let provider_str = ptype.aither_provider();
            if provider_str == "unknown" {
                continue;
            }

            for info in aither_models::models_for_provider(provider_str) {
                if info.outdated {
                    continue;
                }
                let tier = info.primary_tier().map(|t| match t {
                    AitherTier::Flagship => ModelTier::Advanced,
                    AitherTier::Balanced => ModelTier::Balanced,
                    AitherTier::Fast => ModelTier::Fast,
                });
                models.push(AvailableModel {
                    id: info.id.to_string(),
                    provider_id: id.clone(),
                    tier,
                });
            }
        }

        // If we have an "any model" provider (OpenRouter, Ollama, custom),
        // include ALL known models since they can potentially serve them
        if has_any_model_provider {
            for info in aither_models::all_models() {
                if info.outdated {
                    continue;
                }
                // Skip if we already have this model from a direct provider
                if models.iter().any(|m| m.id == info.id) {
                    continue;
                }
                let tier = info.primary_tier().map(|t| match t {
                    AitherTier::Flagship => ModelTier::Advanced,
                    AitherTier::Balanced => ModelTier::Balanced,
                    AitherTier::Fast => ModelTier::Fast,
                });
                models.push(AvailableModel {
                    id: info.id.to_string(),
                    provider_id: String::new(), // Multiple providers could serve this
                    tier,
                });
            }
        }

        // Sort by model ID
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models
    }
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
    /// Temporarily failed providers (model_id -> provider_ids)
    failed: std::sync::RwLock<HashMap<String, Vec<String>>>,
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
    pub fn new(config: &MayConfig) -> Self {
        Self {
            providers: config.provider.clone(),
            models: config.models.clone(),
            strategy: config.preferences.load_balance_strategy,
            counters: HashMap::new(),
            failed: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Get all provider IDs that can serve a model, excluding failed ones.
    fn available_provider_ids(&self, model_id: &str) -> Vec<&str> {
        let failed = self.failed.read().expect("failed lock poisoned");
        let failed_ids = failed.get(model_id);

        self.providers
            .iter()
            .filter(|(id, p)| {
                p.can_serve_model(id, model_id)
                    && failed_ids.map(|ids| !ids.contains(*id)).unwrap_or(true)
            })
            .map(|(id, _)| id.as_str())
            .collect()
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
                    .map(|c| c.fetch_add(1, Ordering::Relaxed))
                    .unwrap_or(0);
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
        let mut failed = self.failed.write().expect("failed lock poisoned");
        failed
            .entry(model_id.to_string())
            .or_default()
            .push(provider_id.to_string());
    }

    /// Clear failed status for a model (e.g., after some time).
    pub fn clear_failed(&self, model_id: &str) {
        let mut failed = self.failed.write().expect("failed lock poisoned");
        failed.remove(model_id);
    }

    /// Clear all failed status.
    pub fn clear_all_failed(&self) {
        let mut failed = self.failed.write().expect("failed lock poisoned");
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
        models.get(current_idx + 1).map(|s| s.as_str())
    }

    /// Get the first model in a tier.
    pub fn first_model_in_tier(&self, tier: ModelTier) -> Option<&str> {
        self.models
            .models_for_tier(tier)
            .first()
            .map(|s| s.as_str())
    }
}

// Keep Provider as an alias for backward compatibility during migration
pub use ProviderType as Provider;

/// Legacy provider config for backward compatibility.
/// This will be removed after full migration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub name: ProviderType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl ProviderConfig {
    pub fn new(provider: ProviderType) -> Self {
        Self {
            name: provider,
            ..Default::default()
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    pub fn effective_base_url(&self) -> Option<&str> {
        self.base_url.as_deref().or_else(|| self.name.base_url())
    }

    pub fn is_valid(&self) -> bool {
        if self.name.requires_api_key() && self.api_key.is_none() {
            return false;
        }
        if self.name.requires_base_url() && self.base_url.is_none() {
            return false;
        }
        true
    }

    pub fn provider_display_name(&self) -> &str {
        self.display_name
            .as_deref()
            .unwrap_or_else(|| self.name.display_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = MayConfig::default();
        assert_eq!(config.max_iterations, 50);
        assert!(config.provider.is_empty());
        assert!(!config.is_configured());
    }

    #[test]
    fn test_parse_new_config() {
        let toml = r#"
max_iterations = 100

[provider.anthropic]
api_key = "sk-ant-test"

[provider.openrouter]
api_key = "or-test"

[models]
advanced = ["claude-opus-4"]
balanced = ["claude-sonnet-4", "gpt-4o"]
fast = ["claude-haiku-4"]

[preferences]
load_balance_strategy = "round_robin"
warn_on_model_change = true
"#;
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
        let toml = r#"
[provider.my-ollama]
type = "ollama"
base_url = "http://192.168.1.100:11434/v1"

[models]
fast = ["llama3"]
"#;
        let config: MayConfig = toml::from_str(toml).unwrap();
        let provider = config.get_provider("my-ollama").unwrap();
        assert_eq!(provider.resolve_type("my-ollama"), ProviderType::Ollama);
        assert_eq!(
            provider.base_url.as_deref(),
            Some("http://192.168.1.100:11434/v1")
        );
    }

    #[test]
    fn test_provider_type_from_id() {
        assert_eq!(ProviderType::from_id("anthropic"), ProviderType::Anthropic);
        assert_eq!(ProviderType::from_id("claude"), ProviderType::Anthropic);
        assert_eq!(ProviderType::from_id("openai"), ProviderType::OpenAI);
        assert_eq!(ProviderType::from_id("google"), ProviderType::Google);
        assert_eq!(ProviderType::from_id("gemini"), ProviderType::Google);
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
        // Anthropic can serve claude models
        assert!(ProviderType::Anthropic.can_serve_model("claude-sonnet-4"));

        // OpenRouter can serve anything
        assert!(ProviderType::OpenRouter.can_serve_model("claude-sonnet-4"));
        assert!(ProviderType::OpenRouter.can_serve_model("gpt-4o"));

        // Copilot can serve OpenAI and Anthropic models, but not Google models
        assert!(ProviderType::Copilot.can_serve_model("gpt-4o"));
        assert!(ProviderType::Copilot.can_serve_model("claude-3-5-sonnet"));
        assert!(!ProviderType::Copilot.can_serve_model("gemini-3-flash-preview"));
    }

    #[test]
    fn test_providers_for_model() {
        let mut config = MayConfig::default();
        config.upsert_provider("anthropic", ProviderInstance::default().with_api_key("key"));
        config.upsert_provider(
            "openrouter",
            ProviderInstance::default().with_api_key("key"),
        );
        config.models.balanced = vec!["claude-sonnet-4".to_string()];

        let providers = config.providers_for_model("claude-sonnet-4");
        assert_eq!(providers.len(), 2); // Both can serve it
    }

    #[test]
    fn test_resolver_failover() {
        let mut config = MayConfig::default();
        config.upsert_provider(
            "anthropic",
            ProviderInstance::default().with_api_key("key1"),
        );
        config.upsert_provider(
            "openrouter",
            ProviderInstance::default().with_api_key("key2"),
        );
        config.models.balanced = vec!["claude-sonnet-4".to_string()];
        config.preferences.load_balance_strategy = LoadBalanceStrategy::FailoverOnly;

        let resolver = config.create_resolver();

        // First selection should pick one of the providers
        let first = resolver.select_provider("claude-sonnet-4");
        assert!(first.is_some());
        let first_id = first.unwrap().id;

        // Mark first as failed
        resolver.mark_failed("claude-sonnet-4", first_id);

        // Now should select the other provider
        let second = resolver.select_provider("claude-sonnet-4");
        assert!(second.is_some());
        assert_ne!(second.unwrap().id, first_id);

        // Clear failures
        resolver.clear_failed("claude-sonnet-4");
        assert!(resolver.select_provider("claude-sonnet-4").is_some());
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
