use std::sync::Arc;

use aither_cloud::{CloudEmbedder, gemini::Gemini, openai::OpenAI};
use aither_core::EmbeddingModel;
use aither_search::SearchEmbeddingModel;

use crate::config::{MayConfig, ProviderInstance, ProviderType};

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
    candidates: Arc<[EmbeddingCandidate]>,
}

impl std::fmt::Debug for MayEmbeddingModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MayEmbeddingModel")
            .field("selector", &self.selector)
            .field("model_id", &self.model_id)
            .field("dimension", &self.dimension)
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

        async move {
            let mut failures = Vec::new();
            for candidate in candidates.iter() {
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
    #[error("no enabled provider supports embedding selector '{model_id}'")]
    NoSupportingProvider { model_id: String },
    #[error("embedding probe failed for selector '{model_id}': {details}")]
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
    let Some(selector) = config.models.embedding_model().cloned() else {
        return Ok(None);
    };
    let provider_id = selector.provider_id().to_string();
    let Some(instance) = config.provider.get(&provider_id) else {
        panic!("configured embedding selector referenced unknown provider '{provider_id}'");
    };
    if !instance.is_valid(&provider_id) {
        return Err(EmbeddingProviderError::NoSupportingProvider {
            model_id: selector.to_string(),
        });
    }
    if !instance.can_serve_model(&provider_id, selector.model_id()) {
        return Err(EmbeddingProviderError::NoSupportingProvider {
            model_id: selector.to_string(),
        });
    }

    let embedder = instantiate_embedding_provider(
        &provider_id,
        instance,
        selector.model_id(),
        config.models.embedding_dimensions,
    )?;
    match embedding_support(instance.resolve_type(&provider_id)) {
        EmbeddingSupport::Unsupported => {
            return Err(EmbeddingProviderError::NoSupportingProvider {
                model_id: selector.to_string(),
            });
        }
        EmbeddingSupport::Trusted => {}
        EmbeddingSupport::RequiresProbe => {
            probe_embedding_provider(&provider_id, &embedder)
                .await
                .map_err(|details| EmbeddingProviderError::ProbeFailed {
                    model_id: selector.to_string(),
                    details,
                })?;
        }
    }

    let dimension = embedder.dim();
    tracing::debug!(
        embedding_selector = selector.to_string(),
        provider_id = provider_id.as_str(),
        "resolved embedding provider for semantic search"
    );

    Ok(Some(MayEmbeddingModel {
        selector: selector.to_string(),
        model_id: selector.model_id().to_string(),
        dimension,
        candidates: Arc::from(vec![EmbeddingCandidate {
            provider_id,
            embedder,
        }]),
    }))
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
    use super::{EmbeddingProviderError, EmbeddingSupport, embedding_support};
    use crate::config::{MayConfig, ProviderInstance, ProviderType};
    use crate::model_selector::ModelSelector;

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
    fn embedding_model_uses_explicit_selector() {
        let mut config = MayConfig::default();
        config.provider.insert(
            "ollama".to_string(),
            ProviderInstance::new(ProviderType::Ollama),
        );
        config.models.embedding_model = Some(ModelSelector::new("ollama", "qwen3-embedding:0.6b"));

        assert_eq!(
            config.models.embedding_model().unwrap().to_string(),
            "ollama::qwen3-embedding:0.6b"
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
