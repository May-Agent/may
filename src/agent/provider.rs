//! Provider selection and LLM client construction for `MayAgent`.
//!
//! Picks the best provider candidate for a given model id, validates
//! generic provider proxies can actually serve the model via `/models`,
//! and builds concrete `CloudProvider` clients with the right base URL,
//! API key, and per-provider quirks (OpenRouter, DeepSeek, Ollama,
//! LMStudio, Copilot, custom OpenAI-compatible proxies, etc.). Also
//! exposes the thin [`RequestParameterizedModel`] wrapper that pins a
//! `Parameters` struct onto every inference request routed through the
//! wrapped model.

use std::collections::HashSet;

use aither_cloud::{
    CloudProvider, claude::Claude, copilot::Copilot, gemini::Gemini, openai::OpenAI,
};
use aither_core::LanguageModel;
use aither_core::llm::{
    Event as LlmEvent, LLMRequest,
    model::{Parameters, Profile as ModelProfile},
};
use futures_lite::Stream;

use super::may::MayAgentError;
use crate::config::{MatchMode, MayConfig, ProviderInstance, ProviderType, model_ids_match};
use crate::copilot::clear_copilot_credentials;
use crate::embedding_provider::{EmbeddingModelResolution, resolve_embedding_model};

const ERR_NO_MODELS: &str = include_str!("../texts/agent_err_no_models.txt");

pub(super) fn err_no_provider(model: &str) -> String {
    format!("No provider available for model '{model}'. Use /config to add providers.")
}

pub(super) fn err_api_key_required(provider: &str) -> String {
    format!("API key required for provider '{provider}'")
}

fn ordered_provider_ids_for_model(config: &MayConfig, model: &str) -> Vec<String> {
    let strict = config.preferences.strict_model_matching;
    let mapped_provider = config
        .models
        .provider_for_model(model, strict)
        .map(str::to_string);

    let mut explicit = Vec::new();
    let mut generic = Vec::new();

    for (id, instance) in &config.provider {
        if !instance.can_serve_model(id, model) {
            continue;
        }
        if instance.is_generic_model_proxy(id) {
            generic.push(id.clone());
        } else {
            explicit.push(id.clone());
        }
    }

    explicit.sort_unstable();
    generic.sort_unstable();

    let mut ordered = explicit;
    ordered.extend(generic);

    if let Some(mapped) = mapped_provider
        && let Some(index) = ordered.iter().position(|id| id == &mapped)
    {
        let preferred = ordered.remove(index);
        ordered.insert(0, preferred);
    }

    ordered
}

/// A single `(model, provider_id)` candidate that `resolve_provider_runtime`
/// can try in order until one successfully initializes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderCandidate {
    pub model: String,
    pub provider_id: String,
}

/// Wraps a base [`LanguageModel`] so every request carries a pinned
/// [`Parameters`] struct (temperature, max tokens, reasoning effort, ...).
#[derive(Clone)]
pub(super) struct RequestParameterizedModel<M> {
    inner: M,
    parameters: Parameters,
}

impl<M> RequestParameterizedModel<M> {
    pub(super) const fn new(inner: M, parameters: Parameters) -> Self {
        Self { inner, parameters }
    }
}

impl<M> LanguageModel for RequestParameterizedModel<M>
where
    M: LanguageModel + Clone,
{
    type Error = M::Error;

    fn respond(
        &self,
        request: LLMRequest,
    ) -> impl Stream<Item = Result<LlmEvent, Self::Error>> + Send {
        self.inner
            .respond(request.with_parameters(self.parameters.clone()))
    }

    async fn profile(&self) -> ModelProfile {
        self.inner.profile().await
    }
}

/// Returns every provider candidate capable of serving `model`,
/// ordered by priority (explicit provider first, then proxy providers).
pub fn provider_candidates_for_model(config: &MayConfig, model: &str) -> Vec<ProviderCandidate> {
    ordered_provider_ids_for_model(config, model)
        .into_iter()
        .map(|provider_id| ProviderCandidate {
            model: model.to_string(),
            provider_id,
        })
        .collect()
}

/// Returns deduped candidates across several model tiers in priority order.
pub fn provider_candidates_for_tiers(
    config: &MayConfig,
    tiers: &[crate::config::ModelTier],
) -> Vec<ProviderCandidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::<(String, String)>::new();

    for tier in tiers {
        for model in config.models.models_for_tier(*tier) {
            for candidate in provider_candidates_for_model(config, model) {
                if seen.insert((candidate.model.clone(), candidate.provider_id.clone())) {
                    candidates.push(candidate);
                }
            }
        }
    }

    candidates
}

/// Number of provider candidates that can serve `model`.
#[must_use]
pub fn provider_candidate_count(config: &MayConfig, model: &str) -> usize {
    provider_candidates_for_model(config, model).len()
}

/// The concrete runtime produced by [`resolve_provider_runtime`].
pub struct ResolvedProviderRuntime {
    pub provider_id: String,
    pub llm: CloudProvider,
    pub search_embedder: EmbeddingModelResolution,
}

/// Walks provider candidates for `model` (starting at `provider_attempt`
/// modulo candidate count) and returns the first candidate that passes
/// generic-provider model validation and successfully initializes a
/// `CloudProvider` client.
pub async fn resolve_provider_runtime(
    may_config: &MayConfig,
    model: &str,
    provider_attempt: usize,
) -> Result<ResolvedProviderRuntime, MayAgentError> {
    let provider_candidates = provider_candidates_for_model(may_config, model);
    let candidate_count = provider_candidates.len();
    if candidate_count == 0 {
        return Err(MayAgentError::MissingApiKey(err_no_provider(model)));
    }
    let start_index = provider_attempt % candidate_count;
    let mut last_provider_error: Option<MayAgentError> = None;

    let mut selected_provider: Option<(String, CloudProvider)> = None;
    for offset in 0..candidate_count {
        let index = (start_index + offset) % candidate_count;
        let provider_id = provider_candidates[index].provider_id.clone();
        let Some(provider_instance) = may_config.provider.get(&provider_id).cloned() else {
            continue;
        };

        if let Err(error) = validate_generic_provider_model_support(
            may_config,
            &provider_id,
            &provider_instance,
            model,
        )
        .await
        {
            tracing::warn!(
                model,
                provider_id,
                error = %error,
                "provider candidate rejected during model support validation"
            );
            last_provider_error = Some(error);
            continue;
        }

        match build_cloud_provider_from_instance(&provider_id, &provider_instance, model).await {
            Ok(llm) => {
                selected_provider = Some((provider_id, llm));
                break;
            }
            Err(error) => {
                tracing::warn!(
                    model,
                    provider_id,
                    error = %error,
                    "failed to initialize provider candidate for model"
                );
                last_provider_error = Some(error);
            }
        }
    }

    let (provider_id, llm) = if let Some(selected) = selected_provider {
        selected
    } else {
        return Err(last_provider_error
            .unwrap_or_else(|| MayAgentError::MissingApiKey(err_no_provider(model))));
    };

    let search_embedder =
        EmbeddingModelResolution::from_result(resolve_embedding_model(may_config).await);
    match &search_embedder {
        EmbeddingModelResolution::Ready(resolved) => {
            tracing::debug!(
                embedding_model = resolved.model_id(),
                provider_ids = %resolved.provider_ids().join(", "),
                "resolved embedding provider for semantic search"
            );
        }
        EmbeddingModelResolution::Disabled => {}
        EmbeddingModelResolution::Failed(error) => {
            tracing::debug!(
                error = %error,
                "semantic search embedder resolution failed"
            );
        }
    }

    Ok(ResolvedProviderRuntime {
        provider_id,
        llm,
        search_embedder,
    })
}

async fn validate_generic_provider_model_support(
    config: &MayConfig,
    provider_id: &str,
    instance: &ProviderInstance,
    model: &str,
) -> Result<(), MayAgentError> {
    if !instance.is_generic_model_proxy(provider_id) {
        return Ok(());
    }

    let fetched = crate::models::fetch_models_for_provider_instance(provider_id, instance)
        .await
        .map_err(|error| {
            MayAgentError::Llm(format!(
                "Failed to validate model '{model}' for provider '{provider_id}' via /models: {error}"
            ))
        })?;

    let mode = MatchMode::from_strict_flag(config.preferences.strict_model_matching);
    let has_match = fetched
        .iter()
        .any(|candidate| model_ids_match(candidate.id.as_str(), model, mode));

    if !has_match {
        return Err(MayAgentError::Llm(format!(
            "Provider '{provider_id}' does not advertise model '{model}' in /models"
        )));
    }

    Ok(())
}

/// Builds a fast+balanced provider chain for background memory extraction.
pub async fn build_fast_provider_chain(
    config: &MayConfig,
) -> Result<Vec<crate::memory::MemoryExtractionProvider>, MayAgentError> {
    let candidates = provider_candidates_for_tiers(
        config,
        &[
            crate::config::ModelTier::Fast,
            crate::config::ModelTier::Balanced,
        ],
    );

    if candidates.is_empty() {
        return Err(MayAgentError::MissingApiKey(
            ERR_NO_MODELS.trim().to_string(),
        ));
    }
    let first_model = candidates[0].model.clone();

    let mut built = Vec::new();
    let mut last_error: Option<MayAgentError> = None;

    for candidate in candidates {
        let Some(instance) = config.get_provider(&candidate.provider_id) else {
            continue;
        };

        match build_cloud_provider_from_instance(&candidate.provider_id, instance, &candidate.model)
            .await
        {
            Ok(llm) => built.push(crate::memory::MemoryExtractionProvider {
                provider_id: candidate.provider_id.clone(),
                model: candidate.model.clone(),
                llm,
            }),
            Err(error) => {
                tracing::warn!(
                    provider_id = %candidate.provider_id,
                    model = %candidate.model,
                    error = %error,
                    "failed to initialize memory extraction provider candidate"
                );
                last_error = Some(error);
            }
        }
    }

    if built.is_empty() {
        return Err(last_error
            .unwrap_or_else(|| MayAgentError::MissingApiKey(err_no_provider(&first_model))));
    }

    Ok(built)
}

/// Build a `CloudProvider` from a `ProviderInstance` directly.
pub(super) async fn build_cloud_provider_from_instance(
    id: &str,
    instance: &ProviderInstance,
    model: &str,
) -> Result<CloudProvider, MayAgentError> {
    let provider_type = instance.resolve_type(id);
    let api_key = instance.api_key.as_deref().unwrap_or("");
    let base_url = instance.effective_base_url(id);

    if provider_type.requires_api_key() && api_key.is_empty() {
        return Err(MayAgentError::MissingApiKey(err_api_key_required(id)));
    }

    let llm: CloudProvider = match provider_type {
        ProviderType::Anthropic => {
            let mut client = Claude::new(api_key).with_model(model);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            client.into()
        }
        ProviderType::Google => {
            let mut client = Gemini::new(api_key).with_text_model(model);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            client.into()
        }
        ProviderType::OpenAI => {
            let mut client = OpenAI::new(api_key).with_model(model);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            client.into()
        }
        ProviderType::OpenRouter => OpenAI::openrouter(api_key).with_model(model).into(),
        ProviderType::DeepSeek => OpenAI::deepseek(api_key).with_model(model).into(),
        ProviderType::Copilot => {
            let credentials = match crate::copilot::get_copilot_credentials().await {
                Ok(Some(credentials)) => credentials,
                Ok(None) => {
                    return Err(MayAgentError::MissingApiKey(
                        "GitHub Copilot not authenticated. Use /config to set up OAuth."
                            .to_string(),
                    ));
                }
                Err(e) => {
                    if is_copilot_unauthorized(&e)
                        && let Err(clear_err) = clear_copilot_credentials().await
                    {
                        tracing::warn!(
                            "Failed to clear invalid Copilot credentials: {}",
                            clear_err
                        );
                    }
                    return Err(MayAgentError::CopilotToken(e));
                }
            };

            Copilot::new(credentials.token)
                .with_base_url(credentials.api_endpoint)
                .with_oauth_token(credentials.oauth_token)
                .with_model(model)
                .into()
        }
        ProviderType::Ollama => {
            let url = base_url.unwrap_or("http://localhost:11434/v1");
            OpenAI::new(api_key)
                .with_base_url(url)
                .with_model(model)
                .into()
        }
        ProviderType::LMStudio => {
            let url = base_url.unwrap_or("http://127.0.0.1:1234/v1");
            OpenAI::new(api_key)
                .with_base_url(url)
                .with_model(model)
                .with_chat_completions_api()
                .into()
        }
        ProviderType::Groq | ProviderType::Together | ProviderType::Mistral | ProviderType::XAI => {
            let mut client = OpenAI::new(api_key).with_model(model);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            client.into()
        }
        ProviderType::Custom => {
            let url = base_url.ok_or_else(|| {
                MayAgentError::MissingApiKey(format!(
                    "Base URL required for custom provider '{id}'"
                ))
            })?;
            OpenAI::new(api_key)
                .with_base_url(url)
                .with_model(model)
                .with_chat_completions_api()
                .into()
        }
    };

    Ok(llm)
}

const fn is_copilot_unauthorized(err: &crate::copilot::CopilotTokenError) -> bool {
    matches!(
        err,
        crate::copilot::CopilotTokenError::Exchange(
            aither_copilot::CopilotError::Http(zenwave::Error::Http { status, .. })
        ) if status.as_u16() == 401
    )
}
