use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use aither_cloud::{CloudEmbedder, gemini::Gemini, openai::OpenAI};
use aither_core::EmbeddingModel;
use aither_search::SearchEmbeddingModel;

use crate::config::{LoadBalanceStrategy, MayConfig, ProviderInstance, ProviderType};
use crate::models::{ProviderModelFetchResult, fetch_models_by_provider};

#[derive(Clone)]
struct EmbeddingCandidate {
    provider_id: String,
    embedder: CloudEmbedder,
}

#[derive(Clone)]
pub struct MayEmbeddingModel {
    selector: String,
    model_id: String,
    dimension: usize,
    strategy: LoadBalanceStrategy,
    candidates: Arc<[EmbeddingCandidate]>,
    round_robin_counter: Arc<AtomicUsize>,
}

impl std::fmt::Debug for MayEmbeddingModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MayEmbeddingModel")
            .field("selector", &self.selector)
            .field("model_id", &self.model_id)
            .field("dimension", &self.dimension)
            .field("strategy", &self.strategy)
            .field("provider_ids", &self.provider_ids())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub enum EmbeddingModelResolution {
    Disabled,
    Failed(EmbeddingProviderError),
    Ready(MayEmbeddingModel),
}

impl EmbeddingModelResolution {
    #[must_use]
    pub fn from_result(result: Result<Option<MayEmbeddingModel>, EmbeddingProviderError>) -> Self {
        match result {
            Ok(Some(model)) => Self::Ready(model),
            Ok(None) => Self::Disabled,
            Err(error) => Self::Failed(error),
        }
    }

    #[must_use]
    pub const fn ready(&self) -> Option<&MayEmbeddingModel> {
        match self {
            Self::Ready(model) => Some(model),
            Self::Disabled | Self::Failed(_) => None,
        }
    }

    #[must_use]
    pub fn clone_ready(&self) -> Option<MayEmbeddingModel> {
        self.ready().cloned()
    }

    pub fn require_ready(
        &self,
        disabled_message: &'static str,
    ) -> Result<MayEmbeddingModel, EmbeddingModelRequireError> {
        match self {
            Self::Disabled => Err(EmbeddingModelRequireError::Disabled(disabled_message)),
            Self::Failed(error) => Err(EmbeddingModelRequireError::Resolution(error.clone())),
            Self::Ready(model) => Ok(model.clone()),
        }
    }
}

impl MayEmbeddingModel {
    #[must_use]
    pub fn selector(&self) -> &str {
        &self.selector
    }

    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    #[must_use]
    pub fn provider_ids(&self) -> Vec<&str> {
        self.candidates
            .iter()
            .map(|candidate| candidate.provider_id.as_str())
            .collect()
    }

    fn start_index(&self) -> usize {
        match self.strategy {
            LoadBalanceStrategy::FailoverOnly => 0,
            LoadBalanceStrategy::RoundRobin => {
                self.round_robin_counter.fetch_add(1, Ordering::Relaxed) % self.candidates.len()
            }
            LoadBalanceStrategy::Random => fastrand::usize(..self.candidates.len()),
        }
    }
}

impl EmbeddingModel for MayEmbeddingModel {
    fn dim(&self) -> usize {
        self.dimension
    }

    fn embed(
        &self,
        text: &str,
    ) -> impl std::future::Future<Output = aither_core::Result<Vec<f32>>> + Send {
        let model_id = self.model_id.clone();
        let text = text.to_string();
        let candidates = self.candidates.clone();
        let start_index = self.start_index();

        async move {
            let mut failures = Vec::new();
            for offset in 0..candidates.len() {
                let index = (start_index + offset) % candidates.len();
                let candidate = &candidates[index];
                match candidate.embedder.embed(text.as_str()).await {
                    Ok(embedding) => return Ok(embedding),
                    Err(error) => failures.push(format!("{}: {}", candidate.provider_id, error)),
                }
            }

            Err(EmbeddingRequestError {
                model_id,
                details: failures.join(" | "),
            }
            .into())
        }
    }
}

impl SearchEmbeddingModel for MayEmbeddingModel {
    fn selector(&self) -> &str {
        self.selector()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddingSupport {
    Unsupported,
    Trusted,
    RequiresProbe,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum EmbeddingProviderError {
    #[error("semantic search requires a global models.embedding_model to be configured")]
    MissingModel,
    #[error(
        "embedding provider '{provider_id}' requires global models.embedding_dimensions for model '{model_id}'"
    )]
    MissingDimensions {
        provider_id: String,
        model_id: String,
    },
    #[error("no enabled provider reports support for embedding model '{model_id}'")]
    NoSupportingProvider { model_id: String },
    #[error(
        "embedding model '{model_id}' resolved incompatible dimensions across providers: {dimensions}"
    )]
    DimensionMismatch {
        model_id: String,
        dimensions: String,
    },
    #[error("embedding probe failed for every provider serving '{model_id}': {details}")]
    ProbeFailed { model_id: String, details: String },
}

#[derive(Debug, thiserror::Error)]
pub enum EmbeddingModelRequireError {
    #[error("{0}")]
    Disabled(&'static str),
    #[error("{0}")]
    Resolution(EmbeddingProviderError),
}

#[derive(Debug, thiserror::Error)]
#[error("embedding request failed for model '{model_id}' across providers: {details}")]
struct EmbeddingRequestError {
    model_id: String,
    details: String,
}

pub async fn resolve_embedding_model(
    config: &MayConfig,
) -> Result<Option<MayEmbeddingModel>, EmbeddingProviderError> {
    let Some(raw_selector) = config.models.embedding_model().map(ToOwned::to_owned) else {
        return Ok(None);
    };
    let selector = parse_embedding_selector(config, raw_selector);

    let provider_models = fetch_models_by_provider(config).await;
    let supported_provider_ids = supported_provider_ids(&provider_models, &selector);
    if supported_provider_ids.is_empty() {
        return Err(EmbeddingProviderError::NoSupportingProvider {
            model_id: selector.selector.clone(),
        });
    }

    let mut candidates = Vec::new();
    let mut failures = Vec::new();
    for provider_id in supported_provider_ids {
        let Some(instance) = config.provider.get(&provider_id) else {
            panic!("provider model fetch referenced unknown provider '{provider_id}'");
        };
        match instantiate_embedding_provider(
            &provider_id,
            instance,
            &selector.model_id,
            config.models.embedding_dimensions,
        ) {
            Ok(embedder) => match embedding_support(instance.resolve_type(&provider_id)) {
                EmbeddingSupport::Unsupported => {
                    failures.push(format!(
                        "{provider_id}: provider does not support embeddings"
                    ));
                }
                EmbeddingSupport::Trusted => {
                    candidates.push(EmbeddingCandidate {
                        provider_id,
                        embedder,
                    });
                }
                EmbeddingSupport::RequiresProbe => {
                    match probe_embedding_provider(&provider_id, &embedder).await {
                        Ok(()) => candidates.push(EmbeddingCandidate {
                            provider_id,
                            embedder,
                        }),
                        Err(error) => failures.push(error),
                    }
                }
            },
            Err(error) => failures.push(error.to_string()),
        }
    }

    if candidates.is_empty() {
        return Err(EmbeddingProviderError::ProbeFailed {
            model_id: selector.selector,
            details: failures.join(" | "),
        });
    }

    let mut dimensions = candidates
        .iter()
        .map(|candidate| (candidate.provider_id.as_str(), candidate.embedder.dim()))
        .collect::<Vec<_>>();
    dimensions.sort_by(|a, b| a.0.cmp(b.0));
    let first_dimension = dimensions[0].1;
    if dimensions
        .iter()
        .any(|(_, dimension)| *dimension != first_dimension)
    {
        return Err(EmbeddingProviderError::DimensionMismatch {
            model_id: selector.selector,
            dimensions: dimensions
                .into_iter()
                .map(|(provider_id, dimension)| format!("{provider_id}:{dimension}"))
                .collect::<Vec<_>>()
                .join(", "),
        });
    }

    candidates.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
    tracing::debug!(
        embedding_selector = selector.selector.as_str(),
        provider_ids = %candidates
            .iter()
            .map(|candidate| candidate.provider_id.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        "resolved embedding providers for semantic search"
    );

    Ok(Some(MayEmbeddingModel {
        selector: selector.selector,
        model_id: selector.model_id,
        dimension: first_dimension,
        strategy: config.preferences.load_balance_strategy,
        candidates: Arc::from(candidates),
        round_robin_counter: Arc::new(AtomicUsize::new(0)),
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EmbeddingSelector {
    selector: String,
    provider_id: Option<String>,
    model_id: String,
}

fn parse_embedding_selector(config: &MayConfig, selector: String) -> EmbeddingSelector {
    let trimmed = selector.trim();
    if let Some((provider_id, model_id)) = trimmed.split_once('@')
        && config.provider.contains_key(provider_id)
        && !model_id.trim().is_empty()
    {
        return EmbeddingSelector {
            selector: trimmed.to_string(),
            provider_id: Some(provider_id.to_string()),
            model_id: model_id.trim().to_string(),
        };
    }

    EmbeddingSelector {
        selector: trimmed.to_string(),
        provider_id: None,
        model_id: trimmed.to_string(),
    }
}

fn supported_provider_ids(
    fetch_results: &[ProviderModelFetchResult],
    selector: &EmbeddingSelector,
) -> Vec<String> {
    let mut provider_ids = fetch_results
        .iter()
        .filter(|entry| entry.error.is_none())
        .filter(|entry| {
            entry
                .models
                .iter()
                .any(|model| model.id == selector.model_id)
        })
        .map(|entry| entry.provider_id.clone())
        .collect::<Vec<_>>();
    if let Some(provider_id) = selector.provider_id.as_deref() {
        provider_ids.retain(|candidate| candidate == provider_id);
    }
    provider_ids.sort();
    provider_ids.dedup();
    provider_ids
}

fn embedding_support(provider_type: ProviderType) -> EmbeddingSupport {
    match provider_type {
        ProviderType::Anthropic | ProviderType::Copilot => EmbeddingSupport::Unsupported,
        ProviderType::OpenAI | ProviderType::Google => EmbeddingSupport::Trusted,
        ProviderType::OpenRouter
        | ProviderType::DeepSeek
        | ProviderType::Ollama
        | ProviderType::LMStudio
        | ProviderType::Groq
        | ProviderType::Together
        | ProviderType::Mistral
        | ProviderType::XAI
        | ProviderType::Custom => EmbeddingSupport::RequiresProbe,
    }
}

fn instantiate_embedding_provider(
    provider_id: &str,
    instance: &ProviderInstance,
    embedding_model: &str,
    embedding_dimensions: Option<usize>,
) -> Result<CloudEmbedder, EmbeddingProviderError> {
    let provider_type = instance.resolve_type(provider_id);
    let api_key = instance.api_key.as_deref().unwrap_or("");
    let base_url = instance.effective_base_url(provider_id);

    match provider_type {
        ProviderType::Google => {
            let mut client = Gemini::new(api_key);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            let embedding_dimensions =
                embedding_dimensions.ok_or_else(|| EmbeddingProviderError::MissingDimensions {
                    provider_id: provider_id.to_string(),
                    model_id: embedding_model.to_string(),
                })?;
            client = client.with_embedding_model(embedding_model, embedding_dimensions);
            Ok(CloudEmbedder::from(client))
        }
        ProviderType::OpenAI
        | ProviderType::OpenRouter
        | ProviderType::DeepSeek
        | ProviderType::Ollama
        | ProviderType::LMStudio
        | ProviderType::Groq
        | ProviderType::Together
        | ProviderType::Mistral
        | ProviderType::XAI
        | ProviderType::Custom => {
            let mut client = OpenAI::new(api_key);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            client = client.with_embedding_model(embedding_model);
            if let Some(embedding_dimensions) = embedding_dimensions {
                client = client.with_embedding_dimensions(embedding_dimensions);
            }
            Ok(CloudEmbedder::from(client))
        }
        ProviderType::Anthropic | ProviderType::Copilot => {
            Err(EmbeddingProviderError::NoSupportingProvider {
                model_id: embedding_model.to_string(),
            })
        }
    }
}

async fn probe_embedding_provider(
    provider_id: &str,
    embedder: &CloudEmbedder,
) -> Result<(), String> {
    embedder
        .embed("may embedding probe")
        .await
        .map(|_| ())
        .map_err(|error| format!("{provider_id}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{
        EmbeddingProviderError, EmbeddingSupport, embedding_support, supported_provider_ids,
    };
    use crate::config::{MayConfig, ProviderInstance, ProviderType};
    use crate::models::ProviderModelFetchResult;

    #[test]
    fn canonical_embedding_providers_are_trusted() {
        assert_eq!(
            embedding_support(ProviderType::OpenAI),
            EmbeddingSupport::Trusted
        );
        assert_eq!(
            embedding_support(ProviderType::Google),
            EmbeddingSupport::Trusted
        );
    }

    #[test]
    fn unsupported_providers_are_rejected() {
        assert_eq!(
            embedding_support(ProviderType::Anthropic),
            EmbeddingSupport::Unsupported
        );
        assert_eq!(
            embedding_support(ProviderType::Copilot),
            EmbeddingSupport::Unsupported
        );
    }

    #[test]
    fn openai_compatible_proxies_require_probe() {
        assert_eq!(
            embedding_support(ProviderType::Custom),
            EmbeddingSupport::RequiresProbe
        );
        assert_eq!(
            embedding_support(ProviderType::OpenRouter),
            EmbeddingSupport::RequiresProbe
        );
        assert_eq!(
            embedding_support(ProviderType::LMStudio),
            EmbeddingSupport::RequiresProbe
        );
    }

    #[test]
    fn embedding_model_is_global_config() {
        let config = MayConfig::default();
        assert!(config.models.embedding_model().is_none());
    }

    #[test]
    fn supported_provider_ids_require_exact_model_match() {
        let results = vec![
            ProviderModelFetchResult {
                provider_id: "ollama".to_string(),
                models: vec![
                    crate::models::FetchedModel {
                        id: "qwen3-embedding:0.6b".to_string(),
                        nickname: "qwen3-embedding:0.6b".to_string(),
                        context_window: None,
                        abilities: Vec::new(),
                        provider_id: "ollama".to_string(),
                    },
                    crate::models::FetchedModel {
                        id: "qwen3-embedding:4b".to_string(),
                        nickname: "qwen3-embedding:4b".to_string(),
                        context_window: None,
                        abilities: Vec::new(),
                        provider_id: "ollama".to_string(),
                    },
                ],
                error: None,
            },
            ProviderModelFetchResult {
                provider_id: "custom".to_string(),
                models: vec![crate::models::FetchedModel {
                    id: "qwen3-embedding:4b".to_string(),
                    nickname: "qwen3-embedding:4b".to_string(),
                    context_window: None,
                    abilities: Vec::new(),
                    provider_id: "custom".to_string(),
                }],
                error: None,
            },
        ];

        assert_eq!(
            supported_provider_ids(
                &results,
                &super::EmbeddingSelector {
                    selector: "qwen3-embedding:0.6b".to_string(),
                    provider_id: None,
                    model_id: "qwen3-embedding:0.6b".to_string(),
                }
            ),
            vec!["ollama".to_string()]
        );
    }

    #[test]
    fn provider_prefixed_embedding_selector_is_opt_in() {
        let mut config = MayConfig::default();
        config.provider.insert(
            "ollama".to_string(),
            ProviderInstance::new(ProviderType::Ollama),
        );

        assert_eq!(
            super::parse_embedding_selector(&config, "ollama@qwen3-embedding:0.6b".to_string()),
            super::EmbeddingSelector {
                selector: "ollama@qwen3-embedding:0.6b".to_string(),
                provider_id: Some("ollama".to_string()),
                model_id: "qwen3-embedding:0.6b".to_string(),
            }
        );
        assert_eq!(
            super::parse_embedding_selector(&config, "qwen3-embedding:0.6b".to_string()),
            super::EmbeddingSelector {
                selector: "qwen3-embedding:0.6b".to_string(),
                provider_id: None,
                model_id: "qwen3-embedding:0.6b".to_string(),
            }
        );
    }

    #[test]
    fn google_embedding_dimensions_are_global() {
        let error = super::instantiate_embedding_provider(
            "google",
            &ProviderInstance::new(ProviderType::Google).with_api_key("g-key"),
            "text-embedding-004",
            None,
        )
        .expect_err("google embeddings must require global dimensions");

        assert!(matches!(
            error,
            EmbeddingProviderError::MissingDimensions { .. }
        ));
    }
}
