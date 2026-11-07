//! Model fetching from provider APIs.

use aither_cloud::{
    ClaudeProvider, CloudModelProvider, CodexProvider, CopilotProvider, GeminiProvider,
};
use aither_core::llm::{LanguageModelProvider, model::Ability};
use aither_models::lookup as lookup_model_info;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;

use crate::config::{MayConfig, ProviderInstance, ProviderType};

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
    CopilotAuth(#[from] crate::copilot::CopilotTokenError),
    /// `ChatGPT` not authenticated.
    #[error("ChatGPT not authenticated")]
    ChatgptNotAuthenticated,
    /// `ChatGPT` auth error.
    #[error("ChatGPT auth error: {0}")]
    ChatgptAuth(#[from] crate::chatgpt::ChatgptAuthError),
    /// xAI Grok not authenticated.
    #[error("xAI Grok not authenticated")]
    GrokNotAuthenticated,
    /// xAI Grok auth error.
    #[error("xAI Grok auth error: {0}")]
    GrokAuth(#[from] crate::grok::GrokAuthError),
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

                match fetch_models_for_provider(&id, &instance).await {
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
        let Some(token) = crate::grok::get_access_token().await? else {
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
            let credentials = crate::copilot::get_copilot_credentials()
                .await?
                .ok_or(ModelFetchError::CopilotNotAuthenticated)?;
            CopilotProvider::new(credentials.token)
                .base_url(credentials.api_endpoint)
                .into()
        }
        ProviderType::Chatgpt => {
            let mut credentials = crate::chatgpt::load_credentials()
                .await?
                .ok_or(ModelFetchError::ChatgptNotAuthenticated)?;
            if credentials.is_expired(std::time::Duration::from_secs(60)) {
                credentials = aither_codex::auth::refresh_tokens(&credentials.refresh_token)
                    .await
                    .map_err(crate::chatgpt::ChatgptAuthError::Codex)?;
                crate::chatgpt::save_credentials(&credentials).await?;
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

            let mut abilities: Vec<String> =
                p.abilities.into_iter().map(ability_to_label).collect();

            // Enrich with abilities from the static model database when the
            // provider API returns none (Claude/Gemini list_models omit them).
            if abilities.is_empty()
                && let Some(info) = aither_models::lookup(&model_id)
            {
                abilities = info
                    .abilities()
                    .iter()
                    .map(|a| ability_to_label(*a))
                    .collect();
            }

            FetchedModel {
                id: model_id,
                nickname,
                context_window: Some(p.context_length),
                abilities,
                provider_id: id.to_string(),
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

    let response = reqwest::Client::new()
        .get(endpoint)
        .headers(headers)
        .send()
        .await?
        .error_for_status()?
        .json::<OpenAiModelListResponse>()
        .await?;

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

            let abilities = aither_models::lookup(&model_id)
                .map(|info| {
                    info.abilities()
                        .iter()
                        .map(|ability| ability_to_label(*ability))
                        .collect()
                })
                .unwrap_or_default();

            FetchedModel {
                id: model_id,
                nickname,
                context_window,
                abilities,
                provider_id: id.to_string(),
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

/// Stable lowercase label for a model [`Ability`], used in listings.
///
/// # Panics
///
/// Panics on an [`Ability`] variant added without a corresponding label —
/// keep the arm list in sync.
#[must_use]
pub fn ability_to_label(ability: Ability) -> String {
    match ability {
        Ability::ToolUse => "tool_use".to_string(),
        Ability::Vision => "vision".to_string(),
        Ability::Audio => "audio".to_string(),
        Ability::AudioOutput => "audio_output".to_string(),
        Ability::Video => "video".to_string(),
        Ability::WebSearch => "web_search".to_string(),
        Ability::Pdf => "pdf".to_string(),
        Ability::CodeExecution => "code_execution".to_string(),
        Ability::Reasoning => "reasoning".to_string(),
        Ability::ImageGeneration => "image_generation".to_string(),
        Ability::ComputerUse => "computer_use".to_string(),
        Ability::PromptCaching => "prompt_caching".to_string(),
        Ability::AssistantPrefill => "assistant_prefill".to_string(),
        _ => panic!("unsupported ability variant: {ability:?}"),
    }
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
        a.parse::<crate::ModelSelector>(),
        b.parse::<crate::ModelSelector>(),
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

#[cfg(test)]
mod tests {
    use super::{
        FamilyEffortRungs, FetchedModel, ProviderType, effort_ladder, effort_rung_rank,
        record_family_effort_rungs, same_selectable_model,
    };

    fn fetched(provider_id: &str, id: &str) -> FetchedModel {
        FetchedModel {
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
