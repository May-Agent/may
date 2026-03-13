use aither_cloud::{CloudEmbedder, gemini::Gemini, openai::OpenAI};

use crate::config::{MayConfig, ProviderInstance, ProviderType};

#[derive(Debug, Clone)]
pub struct ResolvedEmbeddingProvider {
    pub provider_id: String,
    pub embedder: CloudEmbedder,
}

pub fn resolve_embedding_provider(
    config: &MayConfig,
    preferred_provider_id: Option<&str>,
) -> anyhow::Result<ResolvedEmbeddingProvider> {
    let mut candidate_ids = Vec::new();
    if let Some(preferred) = preferred_provider_id
        && !preferred.trim().is_empty()
    {
        candidate_ids.push(preferred.trim().to_string());
    }
    let mut configured_ids = config.provider.keys().cloned().collect::<Vec<_>>();
    configured_ids.sort();
    for id in configured_ids {
        if !candidate_ids.iter().any(|candidate| candidate == &id) {
            candidate_ids.push(id);
        }
    }

    for provider_id in candidate_ids {
        let Some(instance) = config.provider.get(&provider_id) else {
            continue;
        };
        if !instance.is_valid(&provider_id) {
            continue;
        }
        if let Some(embedder) = instantiate_embedding_provider(&provider_id, instance)? {
            return Ok(ResolvedEmbeddingProvider {
                provider_id,
                embedder,
            });
        }
    }

    Err(anyhow::anyhow!(
        "no embedding-capable provider is configured; add an OpenAI-compatible or Google provider"
    ))
}

fn instantiate_embedding_provider(
    provider_id: &str,
    instance: &ProviderInstance,
) -> anyhow::Result<Option<CloudEmbedder>> {
    let provider_type = instance.resolve_type(provider_id);
    let api_key = instance.api_key.as_deref().unwrap_or("");
    let base_url = instance.effective_base_url(provider_id);

    let embedder = match provider_type {
        ProviderType::Google => {
            let mut client = Gemini::new(api_key);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            Some(CloudEmbedder::from(client))
        }
        ProviderType::OpenAI
        | ProviderType::OpenRouter
        | ProviderType::DeepSeek
        | ProviderType::Ollama
        | ProviderType::Groq
        | ProviderType::Together
        | ProviderType::Mistral
        | ProviderType::XAI
        | ProviderType::Custom => {
            let mut client = OpenAI::new(api_key);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            Some(CloudEmbedder::from(client))
        }
        ProviderType::Anthropic | ProviderType::Copilot => None,
    };

    Ok(embedder)
}

#[cfg(test)]
mod tests {
    use super::resolve_embedding_provider;
    use crate::config::{MayConfig, ProviderInstance, ProviderType};

    #[test]
    fn preferred_embedding_provider_wins_when_supported() {
        let mut config = MayConfig::default();
        config.provider.insert(
            "google".to_string(),
            ProviderInstance::new(ProviderType::Google).with_api_key("g-key"),
        );
        config.provider.insert(
            "openai".to_string(),
            ProviderInstance::new(ProviderType::OpenAI).with_api_key("o-key"),
        );

        let resolved = resolve_embedding_provider(&config, Some("google"))
            .expect("preferred supported provider must resolve");
        assert_eq!(resolved.provider_id, "google");
    }

    #[test]
    fn unsupported_providers_are_skipped() {
        let mut config = MayConfig::default();
        config.provider.insert(
            "anthropic".to_string(),
            ProviderInstance::new(ProviderType::Anthropic).with_api_key("a-key"),
        );
        config.provider.insert(
            "openai".to_string(),
            ProviderInstance::new(ProviderType::OpenAI).with_api_key("o-key"),
        );

        let resolved = resolve_embedding_provider(&config, Some("anthropic"))
            .expect("resolver should skip unsupported preferred provider");
        assert_eq!(resolved.provider_id, "openai");
    }
}
