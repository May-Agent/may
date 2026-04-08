//! Model fetching from provider APIs.

use aither_cloud::{ClaudeProvider, CloudModelProvider, CopilotProvider, GeminiProvider};
use aither_core::llm::{LanguageModelProvider, model::Ability};
use aither_models::lookup as lookup_model_info;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;

use crate::config::{MayConfig, ProviderInstance, ProviderType};

/// Error type for model fetching operations.
#[derive(Debug, thiserror::Error)]
pub enum ModelFetchError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Copilot not authenticated")]
    CopilotNotAuthenticated,
    #[error("Copilot auth error: {0}")]
    CopilotAuth(#[from] crate::copilot::CopilotTokenError),
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
                model.provider_id = entry.provider_id.clone();
                model
            })
        })
        .collect::<Vec<_>>();

    // Keep provider-model pairs (do not dedupe only by model ID).
    models.sort_by(|a, b| a.id.cmp(&b.id).then(a.provider_id.cmp(&b.provider_id)));
    models.dedup_by(|a, b| a.id == b.id && a.provider_id == b.provider_id);
    models
}

/// Fetch models from a single provider.
pub(crate) async fn fetch_models_for_provider_instance(
    id: &str,
    instance: &ProviderInstance,
) -> Result<Vec<FetchedModel>, ModelFetchError> {
    fetch_models_for_provider(id, instance).await
}

async fn fetch_models_for_provider(
    id: &str,
    instance: &ProviderInstance,
) -> Result<Vec<FetchedModel>, ModelFetchError> {
    let ptype = instance.resolve_type(id);
    let api_key = instance.api_key.as_deref().unwrap_or("");
    let base_url = instance.effective_base_url(id);

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
        ProviderType::OpenAI
        | ProviderType::OpenRouter
        | ProviderType::DeepSeek
        | ProviderType::Groq
        | ProviderType::Together
        | ProviderType::Mistral
        | ProviderType::XAI
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
            let nickname = aither_models::lookup(&model_id)
                .map(|info| info.id().to_string())
                .or_else(|| {
                    if provider_description.is_empty() {
                        None
                    } else {
                        Some(provider_description.to_string())
                    }
                })
                .unwrap_or_else(|| model_id.clone());

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
                .clone()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| aither_models::lookup(&model_id).map(|info| info.id().to_string()))
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
