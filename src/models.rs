//! Model fetching from provider APIs.

use aither_cloud::{
    ClaudeProvider, CloudModelProvider, CopilotProvider, GeminiProvider, OpenAIProvider,
};
use aither_core::llm::LanguageModelProvider;

use crate::config::{MayConfig, ProviderInstance, ProviderType};

/// Fetched model info.
#[derive(Debug, Clone)]
pub struct FetchedModel {
    /// Model ID.
    pub id: String,
    /// Provider ID that can serve this model.
    pub provider_id: String,
}

/// Fetch models from all configured providers.
///
/// This calls each provider's /models endpoint to get the real list.
pub async fn fetch_all_models(config: &MayConfig) -> Vec<FetchedModel> {
    let mut models = Vec::new();

    for (id, instance) in &config.provider {
        if !instance.is_valid(id) {
            continue;
        }

        match fetch_models_for_provider(id, instance).await {
            Ok(provider_models) => {
                for model_id in provider_models {
                    models.push(FetchedModel {
                        id: model_id,
                        provider_id: id.clone(),
                    });
                }
            }
            Err(e) => {
                tracing::warn!("Failed to fetch models from {}: {}", id, e);
            }
        }
    }

    // Sort and dedupe
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    models
}

/// Fetch models from a single provider.
async fn fetch_models_for_provider(
    id: &str,
    instance: &ProviderInstance,
) -> Result<Vec<String>, String> {
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
    Ok(profiles.into_iter().map(|p| p.name).collect())
}
