//! Model fetching from provider APIs.

use aither_cloud::{
    ClaudeProvider, CloudModelProvider, CodexProvider, CopilotProvider, GeminiProvider,
};
use aither_core::llm::LanguageModelProvider;
use aither_models::{ModelMode, ModelRegistry, Provider, lookup as lookup_model_info};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;

use may_config::config::{MayConfig, ProviderInstance, ProviderType};

/// Error type for model fetching operations.
#[derive(Debug, thiserror::Error)]
pub enum ModelFetchError {
    /// Provider error.
    #[error("provider error: {0}")]
    Provider(String),
    /// HTTP error.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    /// Copilot not authenticated.
    #[error("Copilot not authenticated")]
    CopilotNotAuthenticated,
    /// Copilot auth error.
    #[error("Copilot auth error: {0}")]
    CopilotAuth(#[from] may_harness::copilot::CopilotTokenError),
    /// `ChatGPT` not authenticated.
    #[error("ChatGPT not authenticated")]
    ChatgptNotAuthenticated,
    /// `ChatGPT` auth error.
    #[error("ChatGPT auth error: {0}")]
    ChatgptAuth(#[from] may_harness::chatgpt::ChatgptAuthError),
    /// xAI Grok not authenticated.
    #[error("xAI Grok not authenticated")]
    GrokNotAuthenticated,
    /// xAI Grok auth error.
    #[error("xAI Grok auth error: {0}")]
    GrokAuth(#[from] may_harness::grok::GrokAuthError),
}

/// Fetched model info.
#[derive(Debug, Clone)]
pub struct FetchedModel {
    /// Model ID.
    pub id: String,
    /// Human-readable model nickname.
    pub nickname: String,
    /// Context window if known.
    pub context_window: Option<u32>,
    /// Model abilities if known.
    pub abilities: Vec<String>,
    /// Provider ID that can serve this model.
    pub provider_id: String,
    /// Retired by its provider or superseded by a newer generation of its
    /// line; listings hide it by default.
    pub outdated: bool,
}

/// Fetch result for one provider.
#[derive(Debug, Clone)]
pub struct ProviderModelFetchResult {
    /// Provider ID.
    pub provider_id: String,
    /// Models fetched from this provider.
    pub models: Vec<FetchedModel>,
    /// Error if fetch failed.
    pub error: Option<String>,
}

/// Longest one provider's model list may take before it counts as failed:
/// the fetch is best-effort enrichment, so a hung endpoint cannot stall a
/// turn that asked for the context window.
const PROVIDER_MODELS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// One provider HTTP call may take at most this long.
const FETCH_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Fetch models from all configured providers with per-provider status.
///
/// Providers are queried concurrently for maximum throughput.
pub async fn fetch_models_by_provider(config: &MayConfig) -> Vec<ProviderModelFetchResult> {
    let futures: Vec<_> = config
        .provider
        .iter()
        .map(|(id, instance)| {
            let id = id.clone();
            let instance = instance.clone();
            async move {
                if !instance.is_valid(&id) {
                    return ProviderModelFetchResult {
                        provider_id: id,
                        models: Vec::new(),
                        error: Some(
                            "provider is disabled or missing required credentials".to_string(),
                        ),
                    };
                }

                match bounded(
                    PROVIDER_MODELS_TIMEOUT,
                    fetch_models_for_provider(&id, &instance),
                    |timeout| {
                        ModelFetchError::Provider(format!(
                            "model list timed out after {}s",
                            timeout.as_secs()
                        ))
                    },
                )
                .await
                {
                    Ok(mut provider_models) => {
                        provider_models.sort_by(|a, b| a.id.cmp(&b.id));
                        provider_models.dedup_by(|a, b| a.id == b.id);
                        ProviderModelFetchResult {
                            provider_id: id,
                            models: provider_models,
                            error: None,
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch models from {id}: {e}");
                        ProviderModelFetchResult {
                            provider_id: id,
                            models: Vec::new(),
                            error: Some(e.to_string()),
                        }
                    }
                }
            }
        })
        .collect();

    let mut results = futures_util::future::join_all(futures).await;
    results.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
    results
}

/// Fetch models from all configured providers.
///
/// This calls each provider's /models endpoint to get the real list.
pub async fn fetch_all_models(config: &MayConfig) -> Vec<FetchedModel> {
    let mut models = fetch_models_by_provider(config)
        .await
        .into_iter()
        .flat_map(|entry| {
            entry.models.into_iter().map(move |mut model| {
                model.provider_id.clone_from(&entry.provider_id);
                model
            })
        })
        .collect::<Vec<_>>();

    // Keep provider-model pairs (do not dedupe only by model ID).
    models.sort_by(|a, b| a.id.cmp(&b.id).then(a.provider_id.cmp(&b.provider_id)));
    models.dedup_by(|a, b| a.id == b.id && a.provider_id == b.provider_id);
    models
}

async fn fetch_models_for_provider(
    id: &str,
    instance: &ProviderInstance,
) -> Result<Vec<FetchedModel>, ModelFetchError> {
    let ptype = instance.resolve_type(id);
    let api_key = instance.api_key.as_deref().unwrap_or("");
    let base_url = instance.effective_base_url(id);

    if ptype == ProviderType::Grok {
        let Some(token) = may_harness::grok::get_access_token().await? else {
            return Err(ModelFetchError::GrokNotAuthenticated);
        };
        return fetch_openai_compatible_models(id, &token, base_url).await;
    }

    if matches!(
        ptype,
        ProviderType::OpenAI
            | ProviderType::OpenRouter
            | ProviderType::DeepSeek
            | ProviderType::Groq
            | ProviderType::Together
            | ProviderType::Mistral
            | ProviderType::XAI
            | ProviderType::Ollama
            | ProviderType::LMStudio
            | ProviderType::Custom
    ) {
        return fetch_openai_compatible_models(id, api_key, base_url).await;
    }

    let provider: CloudModelProvider = match ptype {
        ProviderType::Anthropic => {
            let mut p = ClaudeProvider::new(api_key);
            if let Some(url) = base_url {
                p = p.base_url(url);
            }
            p.into()
        }
        ProviderType::Google => {
            let mut p = GeminiProvider::new(api_key);
            if let Some(url) = base_url {
                p = p.base_url(url);
            }
            p.into()
        }
        ProviderType::Copilot => {
            let credentials = may_harness::copilot::get_copilot_credentials()
                .await?
                .ok_or(ModelFetchError::CopilotNotAuthenticated)?;
            CopilotProvider::new(credentials.token)
                .base_url(credentials.api_endpoint)
                .into()
        }
        ProviderType::Chatgpt => {
            let mut credentials = may_harness::chatgpt::load_credentials()
                .await?
                .ok_or(ModelFetchError::ChatgptNotAuthenticated)?;
            if credentials.is_expired(std::time::Duration::from_secs(60)) {
                credentials = aither_codex::auth::refresh_tokens(&credentials.refresh_token)
                    .await
                    .map_err(may_harness::chatgpt::ChatgptAuthError::Codex)?;
                may_harness::chatgpt::save_credentials(&credentials).await?;
            }
            let mut provider = CodexProvider::new(credentials);
            if let Some(url) = base_url {
                provider = provider.with_base_url(url);
            }
            provider.into()
        }
        ProviderType::OpenAI
        | ProviderType::OpenRouter
        | ProviderType::DeepSeek
        | ProviderType::Groq
        | ProviderType::Together
        | ProviderType::Mistral
        | ProviderType::XAI
        | ProviderType::Grok
        | ProviderType::Ollama
        | ProviderType::LMStudio
        | ProviderType::Custom => unreachable!("handled above"),
    };

    let profiles = provider
        .list_models()
        .await
        .map_err(|error| ModelFetchError::Provider(error.to_string()))?;
    let registry = ModelRegistry::bundled();
    let outdated_index = OutdatedIndex::of(registry);
    Ok(profiles
        .into_iter()
        .map(|p| {
            let model_id = p.name;
            let provider_description = p.description.trim();
            let nickname = if provider_description.is_empty() {
                model_id.clone()
            } else {
                provider_description.to_string()
            };

            let mut abilities: Vec<String> = p
                .abilities
                .into_iter()
                .map(|a| a.label().to_string())
                .collect();

            // Enrich with abilities from the static model database when the
            // provider API returns none (Claude/Gemini list_models omit them).
            if abilities.is_empty()
                && let Some(info) = aither_models::lookup(&model_id)
            {
                abilities = info
                    .abilities()
                    .iter()
                    .map(|a| a.label().to_string())
                    .collect();
            }

            let outdated = outdated_index.is_outdated_id(registry, &model_id);
            FetchedModel {
                id: model_id,
                nickname,
                context_window: Some(p.context_length),
                abilities,
                provider_id: id.to_string(),
                outdated,
            }
        })
        .collect())
}

async fn fetch_openai_compatible_models(
    id: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<Vec<FetchedModel>, ModelFetchError> {
    let base_url = base_url.unwrap_or("https://api.openai.com/v1");
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    let mut headers = HeaderMap::new();
    if !api_key.is_empty() {
        let token = format!("Bearer {api_key}");
        let value = HeaderValue::from_str(&token)
            .expect("authorization header constructed from configured API key must be valid");
        headers.insert(AUTHORIZATION, value);
    }

    let response = reqwest::Client::builder()
        .timeout(FETCH_REQUEST_TIMEOUT)
        .build()
        .expect("the HTTP client builds with its default configuration")
        .get(endpoint)
        .headers(headers)
        .send()
        .await?
        .error_for_status()?
        .json::<OpenAiModelListResponse>()
        .await?;

    let registry = ModelRegistry::bundled();
    let outdated_index = OutdatedIndex::of(registry);
    Ok(response
        .data
        .into_iter()
        .map(|model| {
            let model_id = model.id;
            let context_window = model
                .context_length
                .or(model.max_tokens)
                .or(model.input_token_limit)
                .or_else(|| {
                    lookup_model_info(&model_id)
                        .and_then(aither_models::ModelEntry::max_input_tokens)
                });
            let nickname = model
                .description
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| model_id.clone());

            let abilities = aither_models::lookup(&model_id).map_or_default(|info| {
                info.abilities()
                    .iter()
                    .map(|ability| ability.label().to_string())
                    .collect()
            });

            let outdated = outdated_index.is_outdated_id(registry, &model_id);
            FetchedModel {
                id: model_id,
                nickname,
                context_window,
                abilities,
                provider_id: id.to_string(),
                outdated,
            }
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct OpenAiModelListResponse {
    data: Vec<OpenAiModelDescriptor>,
}

#[derive(Debug, Deserialize)]
struct OpenAiModelDescriptor {
    id: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    context_length: Option<u32>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default, alias = "inputTokenLimit", alias = "input_token_limit")]
    input_token_limit: Option<u32>,
}

/// Trailing tokens a provider catalog uses to encode a reasoning-effort rung
/// inside the model uid (`o4-mini-high`, `claude-opus-5-5-low`).
const EFFORT_UID_TOKENS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Speed markers that may follow the effort rung in a uid
/// (`claude-opus-5-5-low-fast`, `o4-mini-high-lightning`). The marker belongs to
/// the family, the effort rung does not.
const EFFORT_UID_TAGS: &[&str] = &["-fast", "-lightning"];

/// The family a catalog uid belongs to once its effort rung is stripped.
///
/// `o4-mini-high` and `o4-mini-max` share the `o4-mini` family, and a trailing
/// speed marker stays with the family (`o4-mini-high-lightning` →
/// `o4-mini-lightning`). Providers whose catalog encodes effort in the uid
/// pick the rung through the reasoning-effort ladder rather than a suffixed
/// model.
#[must_use]
pub fn effort_family_base(model_id: &str) -> String {
    for tag in EFFORT_UID_TAGS.iter().chain([&""]) {
        let Some(core) = model_id.strip_suffix(tag) else {
            continue;
        };
        for token in EFFORT_UID_TOKENS {
            let suffix = format!("-{token}");
            if let Some(base) = core.strip_suffix(&suffix)
                && !base.is_empty()
            {
                return format!("{base}{tag}");
            }
        }
    }
    model_id.to_string()
}

/// The context window of `model_id` on `provider_id`: what the provider
/// reports in its model list, else the `aither_models` table (also looked up
/// by effort family, so `o4-mini-high` resolves like `o4-mini`).
#[must_use]
pub fn resolve_context_window(
    fetched: &[FetchedModel],
    provider_id: &str,
    model_id: &str,
) -> Option<u32> {
    let family = effort_family_base(model_id);
    let reported = |wanted: &str| {
        fetched
            .iter()
            .find(|model| model.provider_id == provider_id && model.id == wanted)
            .and_then(|model| model.context_window)
    };
    let tabled = |wanted: &str| {
        lookup_model_info(wanted).and_then(aither_models::ModelEntry::max_input_tokens)
    };
    reported(model_id)
        .or_else(|| reported(&family))
        .or_else(|| tabled(model_id))
        .or_else(|| tabled(&family))
}

/// Whether two selector strings address the same selectable model.
///
/// The provider ids must match and the model ids must collapse to the same
/// effort family (`relay::o4-mini-high` and `relay::o4-mini` are one picker entry).
/// Strings that fail to parse only match exactly.
#[must_use]
pub fn same_selectable_model(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (Ok(a), Ok(b)) = (
        a.parse::<may_protocol::model_selector::ModelSelector>(),
        b.parse::<may_protocol::model_selector::ModelSelector>(),
    ) else {
        return false;
    };
    a.provider_id() == b.provider_id()
        && effort_family_base(a.model_id()) == effort_family_base(b.model_id())
}

/// Position of `rung` on the canonical effort order (`none` … `max`),
/// for sorting a family's served rungs.
#[must_use]
pub fn effort_rung_rank(rung: &str) -> usize {
    EFFORT_UID_TOKENS
        .iter()
        .position(|token| *token == rung)
        .unwrap_or(usize::MAX)
}

/// The effort rung a catalog uid expresses, if it carries one
/// (`o4-mini-high-lightning` → `high`).
#[must_use]
pub fn effort_uid_token(model_id: &str) -> Option<&'static str> {
    for tag in EFFORT_UID_TAGS.iter().chain([&""]) {
        let Some(core) = model_id.strip_suffix(tag) else {
            continue;
        };
        for token in EFFORT_UID_TOKENS {
            if core
                .strip_suffix(&format!("-{token}"))
                .is_some_and(|base| !base.is_empty())
            {
                return Some(token);
            }
        }
    }
    None
}

/// The effort rungs a catalog serves per `(provider id, family base)`, in
/// canonical order.
pub type FamilyEffortRungs = std::collections::HashMap<(String, String), Vec<&'static str>>;

/// Record the rungs the catalog encodes in model uids into `rungs`, keeping
/// each family's rungs sorted and unique.
pub fn record_family_effort_rungs(rungs: &mut FamilyEffortRungs, models: &[FetchedModel]) {
    for model in models {
        let Some(rung) = effort_uid_token(&model.id) else {
            continue;
        };
        let family = rungs
            .entry((model.provider_id.clone(), effort_family_base(&model.id)))
            .or_default();
        if !family.contains(&rung) {
            family.push(rung);
        }
    }
    for family in rungs.values_mut() {
        family.sort_by_key(|rung| effort_rung_rank(rung));
    }
}

/// Grouping key and version of a model id within its product line.
///
/// The key is the alpha tokens up to the first token carrying a digit
/// (`claude-sonnet-5-5` → `claude-sonnet`); the version is that token's
/// leading digits plus every following pure-digit token of at most two
/// digits (`5-5` → `[5, 5]`). Everything after the version run is a variant
/// tag, not part of the key (`gpt-4o-mini` shares `gpt-4o`'s `gpt`; `o4-mini`
/// and `o5` share `o`). Dated or numeric run-ons end the run
/// (`gpt-4o-2024-08-06` still parses as `gpt` `[4]`), and ids with no
/// version or no key — `whisper`, `codex-mini-latest` — return `None`:
/// without a version there is nothing to compare.
fn line_version(id: &str) -> Option<(String, Vec<u32>)> {
    let mut key = String::new();
    let mut version: Vec<u32> = Vec::new();
    for token in id.split(['-', '.']) {
        match token.bytes().position(|byte| byte.is_ascii_digit()) {
            _ if !version.is_empty() => {
                if token.bytes().all(|byte| byte.is_ascii_digit()) && token.len() <= 2 {
                    version.push(token.parse().ok()?);
                } else {
                    break;
                }
            }
            None => {
                if !key.is_empty() {
                    key.push('-');
                }
                key.push_str(token);
            }
            Some(at) => {
                if at > 0 {
                    if !key.is_empty() {
                        key.push('-');
                    }
                    key.push_str(&token[..at]);
                }
                let digits: String = token[at..]
                    .bytes()
                    .take_while(u8::is_ascii_digit)
                    .map(char::from)
                    .collect();
                version.push(digits.parse().ok()?);
            }
        }
    }
    (!key.is_empty() && !version.is_empty()).then_some((key, version))
}

/// Latest generation per `(provider, mode, line)` of a model registry, for
/// telling apart a model that is retired or superseded from one that is
/// merely not the latest a vendor offers.
///
/// `effort_family_base` folds effort rungs into their family first, so
/// `o4-mini-high` and `o4-mini` are one entry, never each other's newer
/// generation.
#[derive(Debug)]
pub struct OutdatedIndex {
    latest: std::collections::HashMap<(Provider, ModelMode, String), Vec<u32>>,
}

impl OutdatedIndex {
    /// Indexes `registry`'s entries by product line.
    #[must_use]
    pub fn of(registry: &ModelRegistry) -> Self {
        let mut latest: std::collections::HashMap<_, Vec<u32>> = std::collections::HashMap::new();
        for entry in registry.all() {
            let Some((line, version)) = line_version(&effort_family_base(entry.id())) else {
                continue;
            };
            let key = (entry.provider().clone(), entry.mode(), line);
            if latest.get(&key).is_none_or(|max| version > *max) {
                latest.insert(key, version);
            }
        }
        Self { latest }
    }

    /// Whether `entry` is retired by its provider or superseded by a newer
    /// generation of its own line. Entries carrying no version are never
    /// superseded — there is no newer one to name.
    #[must_use]
    pub fn is_outdated(&self, entry: &aither_models::ModelEntry) -> bool {
        if entry.is_deprecated() {
            return true;
        }
        let Some((line, version)) = line_version(&effort_family_base(entry.id())) else {
            return false;
        };
        self.latest
            .get(&(entry.provider().clone(), entry.mode(), line))
            .is_some_and(|max| version < *max)
    }

    /// Whether `model_id` is outdated in `registry`: the entry its id
    /// normalizes to is retired or superseded. Unknown ids are never
    /// outdated — there is no evidence either way.
    #[must_use]
    pub fn is_outdated_id(&self, registry: &ModelRegistry, model_id: &str) -> bool {
        registry
            .lookup(model_id)
            .is_some_and(|entry| self.is_outdated(entry))
    }
}

/// The level a session uses before any selection.
pub const DEFAULT_EFFORT: &str = "medium";

/// The reasoning-effort ladder a provider's wire protocol expresses.
///
/// Every rung is concrete — the session's effort is the one it last selected, and
/// "default" before any selection means medium, not a provider-decided
/// level. Families outside a ladder still take it: vendors map what they can
/// on their own API-compat layer and reject what a model does not have.
#[must_use]
pub const fn provider_effort_ladder(
    provider_type: Option<ProviderType>,
) -> &'static [&'static str] {
    const OPENAI: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
    const CLAUDE: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];
    const GEMINI: &[&str] = &["minimal", "low", "medium", "high"];
    match provider_type {
        Some(ProviderType::Anthropic) => CLAUDE,
        Some(ProviderType::Google) => GEMINI,
        _ => OPENAI,
    }
}

/// The effort rungs offered for `model_id` on `provider_id`: the family's
/// served rungs when the catalog reports them, else the provider protocol's
/// generic ladder.
#[must_use]
pub fn effort_ladder(
    family_rungs: &FamilyEffortRungs,
    provider_id: &str,
    model_id: &str,
    provider_type: Option<ProviderType>,
) -> Vec<&'static str> {
    family_rungs
        .get(&(provider_id.to_string(), effort_family_base(model_id)))
        .cloned()
        .unwrap_or_else(|| provider_effort_ladder(provider_type).to_vec())
}

/// Fails `fut` with `on_timeout` when it has not finished within `timeout`.
async fn bounded<T, E>(
    timeout: std::time::Duration,
    fut: impl std::future::Future<Output = Result<T, E>>,
    on_timeout: impl FnOnce(std::time::Duration) -> E,
) -> Result<T, E> {
    enum Out<T, E> {
        Value(Result<T, E>),
        Timeout,
    }
    match futures_lite::future::race(async { Out::Value(fut.await) }, async {
        async_io::Timer::after(timeout).await;
        Out::Timeout
    })
    .await
    {
        Out::Value(result) => result,
        Out::Timeout => Err(on_timeout(timeout)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FamilyEffortRungs, FetchedModel, ProviderType, effort_ladder, effort_rung_rank,
        record_family_effort_rungs, same_selectable_model,
    };

    fn fetched(provider_id: &str, id: &str) -> FetchedModel {
        FetchedModel {
            outdated: false,
            id: id.to_string(),
            nickname: id.to_string(),
            context_window: None,
            abilities: Vec::new(),
            provider_id: provider_id.to_string(),
        }
    }

    #[test]
    fn family_rungs_are_sorted_and_the_catalog_beats_the_provider_ladder() {
        let mut rungs = FamilyEffortRungs::new();
        record_family_effort_rungs(
            &mut rungs,
            &[
                fetched("relay", "o4-mini-high"),
                fetched("relay", "o4-mini-low"),
                fetched("relay", "o4-mini-high"),
            ],
        );
        assert_eq!(
            effort_ladder(&rungs, "relay", "o4-mini", None),
            ["low", "high"]
        );
        assert_eq!(
            effort_ladder(&rungs, "relay", "o4-mini-low", None),
            ["low", "high"]
        );
        assert_eq!(
            effort_ladder(&rungs, "relay", "gpt-x", Some(ProviderType::Google)),
            ["minimal", "low", "medium", "high"]
        );
    }

    fn with_window(provider_id: &str, id: &str, window: u32) -> FetchedModel {
        FetchedModel {
            outdated: false,
            context_window: Some(window),
            ..fetched(provider_id, id)
        }
    }

    fn registry() -> aither_models::ModelRegistry {
        aither_models::ModelRegistry::from_litellm_json(
            br#"{
                "gpt-4o": {"litellm_provider": "openai", "mode": "chat", "input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6},
                "gpt-4o-mini": {"litellm_provider": "openai", "mode": "chat", "input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6},
                "gpt-6.2": {"litellm_provider": "openai", "mode": "chat", "input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6},
                "o4-mini": {"litellm_provider": "openai", "mode": "chat", "input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6},
                "o4-mini-high": {"litellm_provider": "openai", "mode": "chat", "input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6},
                "claude-sonnet-4": {"litellm_provider": "anthropic", "mode": "chat", "input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6},
                "claude-sonnet-5-5": {"litellm_provider": "anthropic", "mode": "chat", "input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6},
                "claude-opus-4-5": {"litellm_provider": "anthropic", "mode": "chat", "input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6},
                "retired-model": {"litellm_provider": "openai", "mode": "chat", "input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6, "deprecation_date": "2020-01-01"},
                "unversioned": {"litellm_provider": "openai", "mode": "chat", "input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6}
            }"#,
        )
    }

    #[test]
    fn a_model_is_outdated_when_its_line_has_a_newer_generation() {
        let registry = registry();
        let index = super::OutdatedIndex::of(&registry);
        let outdated = |id: &str| index.is_outdated_id(&registry, id);
        assert!(outdated("gpt-4o"), "gpt-4o < gpt-6.2");
        assert!(outdated("gpt-4o-mini"), "same line, same old generation");
        assert!(!outdated("gpt-6.2"), "the line's latest is never outdated");
        assert!(
            !outdated("o4-mini"),
            "a different line is untouched by gpt-6.2"
        );
        assert!(
            !outdated("o4-mini-high"),
            "an effort rung is not its family's newer generation"
        );
        assert!(outdated("claude-sonnet-4"), "sonnet 4 < sonnet 5.5");
        assert!(!outdated("claude-opus-4-5"), "opus is its own line");
        assert!(outdated("retired-model"), "deprecated is outdated");
        assert!(!outdated("unversioned"), "unversioned is never outdated");
        assert!(
            !outdated("totally-unknown"),
            "unknown ids are never outdated"
        );
    }

    #[test]
    fn a_provider_reported_window_resolves_a_model_the_table_lacks() {
        let list = [with_window("relay", "relay-only-model", 262_144)];
        assert_eq!(
            super::resolve_context_window(&list, "relay", "relay-only-model"),
            Some(262_144)
        );
        assert_eq!(
            super::resolve_context_window(&list, "other", "relay-only-model"),
            None
        );
    }

    #[test]
    fn the_provider_window_beats_the_table_and_the_table_fills_the_rest() {
        let table = aither_models::lookup("gpt-4o")
            .and_then(aither_models::ModelEntry::max_input_tokens)
            .expect("the table lists gpt-4o");
        assert_eq!(
            super::resolve_context_window(&[], "relay", "gpt-4o"),
            Some(table)
        );
        let list = [fetched("relay", "gpt-4o")];
        assert_eq!(
            super::resolve_context_window(&list, "relay", "gpt-4o"),
            Some(table)
        );
        let list = [with_window("relay", "gpt-4o", 12_345)];
        assert_eq!(
            super::resolve_context_window(&list, "relay", "gpt-4o"),
            Some(12_345)
        );
    }

    #[test]
    fn an_effort_rung_resolves_through_its_family() {
        let list = [with_window("relay", "o4-mini", 4_321)];
        assert_eq!(
            super::resolve_context_window(&list, "relay", "o4-mini-high"),
            Some(4_321)
        );
    }

    #[test]
    fn same_model_matches_across_effort_rungs_on_one_provider() {
        assert!(same_selectable_model(
            "relay::o4-mini-high",
            "relay::o4-mini"
        ));
        assert!(same_selectable_model(
            "relay::o4-mini",
            "relay::o4-mini-max"
        ));
        assert!(same_selectable_model(
            "relay::o4-mini-high-lightning",
            "relay::o4-mini-lightning"
        ));
    }

    #[test]
    fn same_model_requires_the_same_provider_and_family() {
        assert!(!same_selectable_model("google::o4-mini", "relay::o4-mini"));
        assert!(!same_selectable_model(
            "relay::o4-mini-high",
            "relay::o5-mini"
        ));
    }

    #[test]
    fn unparseable_selectors_only_match_exactly() {
        assert!(same_selectable_model("o4-mini-high", "o4-mini-high"));
        assert!(!same_selectable_model("o4-mini-high", "o4-mini"));
    }

    #[test]
    fn effort_rung_rank_orders_the_ladder() {
        let mut rungs = vec!["max", "medium", "high"];
        rungs.sort_by_key(|rung| effort_rung_rank(rung));
        assert_eq!(rungs, ["medium", "high", "max"]);
    }
}
