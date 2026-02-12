//! Model fetching from provider APIs.

use aither_cloud::{
    ClaudeProvider, CloudModelProvider, CopilotProvider, GeminiProvider, OpenAIProvider,
};
use aither_core::llm::{LanguageModelProvider, model::Ability};

use crate::config::{MayConfig, ProviderInstance, ProviderType};

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
pub async fn fetch_models_by_provider(config: &MayConfig) -> Vec<ProviderModelFetchResult> {
    let mut results = Vec::new();

    for (id, instance) in &config.provider {
        if !instance.is_valid(id) {
            results.push(ProviderModelFetchResult {
                provider_id: id.clone(),
                models: Vec::new(),
                error: Some("provider is disabled or missing required credentials".to_string()),
            });
            continue;
        }

        match fetch_models_for_provider(id, instance).await {
            Ok(mut provider_models) => {
                provider_models.sort_by(|a, b| a.id.cmp(&b.id));
                provider_models.dedup_by(|a, b| a.id == b.id);
                results.push(ProviderModelFetchResult {
                    provider_id: id.clone(),
                    models: provider_models,
                    error: None,
                });
            }
            Err(e) => {
                tracing::warn!("Failed to fetch models from {}: {}", id, e);
                results.push(ProviderModelFetchResult {
                    provider_id: id.clone(),
                    models: Vec::new(),
                    error: Some(e),
                });
            }
        }
    }

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
async fn fetch_models_for_provider(
    id: &str,
    instance: &ProviderInstance,
) -> Result<Vec<FetchedModel>, String> {
    let ptype = instance.resolve_type(id);
    let api_key = instance.api_key.as_deref().unwrap_or("");
    let base_url = instance.effective_base_url(id);

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
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "Copilot not authenticated".to_string())?;
            CopilotProvider::new(credentials.token)
                .base_url(credentials.api_endpoint)
                .into()
        }
        // OpenAI-compatible providers
        ProviderType::OpenAI
        | ProviderType::OpenRouter
        | ProviderType::DeepSeek
        | ProviderType::Groq
        | ProviderType::Together
        | ProviderType::Mistral
        | ProviderType::XAI
        | ProviderType::Ollama
        | ProviderType::Custom => {
            let mut p = OpenAIProvider::new(api_key);
            if let Some(url) = base_url {
                p = p.base_url(url);
            }
            p.into()
        }
    };

    let profiles = provider.list_models().await.map_err(|e| e.to_string())?;
    Ok(profiles
        .into_iter()
        .map(|p| FetchedModel {
            id: p.name,
            nickname: p.description,
            context_window: Some(p.context_length),
            abilities: p
                .abilities
                .into_iter()
                .map(ability_to_label)
                .collect::<Vec<_>>(),
            provider_id: id.to_string(),
        })
        .collect())
}

fn ability_to_label(ability: Ability) -> String {
    match ability {
        Ability::ToolUse => "tool_use".to_string(),
        Ability::Vision => "vision".to_string(),
        Ability::Audio => "audio".to_string(),
        Ability::Video => "video".to_string(),
        Ability::WebSearch => "web_search".to_string(),
        Ability::Pdf => "pdf".to_string(),
        Ability::CodeExecution => "code_execution".to_string(),
        Ability::Reasoning => "reasoning".to_string(),
        Ability::ImageGeneration => "image_generation".to_string(),
    }
}
