//! Provider selection and LLM client construction for `MayAgent`.
//!
//! Resolves an explicit `<provider_id>::<model_id>` selector into one
//! concrete provider instance, then builds the `CloudProvider` client
//! with the right base URL, API key, and per-provider quirks
//! (`OpenRouter`, `DeepSeek`, Ollama, `LMStudio`, Copilot, custom
//! OpenAI-compatible proxies, etc.). Also exposes the thin
//! [`RequestParameterizedModel`] wrapper that pins a [`Parameters`]
//! struct onto every inference request routed through the wrapped
//! model.

use std::collections::HashSet;

use aither_cloud::{
    CloudProvider, claude::Claude, codex::Codex, copilot::Copilot, gemini::Gemini, openai::OpenAI,
};
use aither_core::LanguageModel;
use aither_core::llm::{
    Event as LlmEvent, LLMRequest,
    model::{Parameters, Profile as ModelProfile},
};
use futures_lite::Stream;

use super::may::MayAgentError;
use crate::config::{MayConfig, ProviderInstance, ProviderType};
use crate::copilot::clear_copilot_credentials;
use crate::embedding_provider::{EmbeddingModelResolution, resolve_embedding_model};
use crate::model_selector::ModelSelector;

const ERR_NO_MODELS: &str = include_str!("../texts/agent_err_no_models.txt");

pub(super) fn err_no_provider(model: &str) -> String {
    format!("No provider available for model '{model}'. Use /config to add providers.")
}

pub(super) fn err_api_key_required(provider: &str) -> String {
    format!("API key required for provider '{provider}'")
}

fn resolve_model_selector(
    may_config: &MayConfig,
    selector: &str,
) -> Result<(ModelSelector, ProviderInstance), MayAgentError> {
    let selector = selector.parse::<ModelSelector>().map_err(|error| {
        MayAgentError::MissingApiKey(format!("Invalid model selector '{selector}': {error}"))
    })?;

    let provider_id = selector.provider_id();
    let Some(provider_instance) = may_config.provider.get(provider_id).cloned() else {
        return Err(MayAgentError::MissingApiKey(err_no_provider(
            selector.model_id(),
        )));
    };
    if !provider_instance.is_valid(provider_id) {
        return Err(MayAgentError::MissingApiKey(err_no_provider(
            selector.model_id(),
        )));
    }
    if !provider_instance.can_serve_model(provider_id, selector.model_id()) {
        return Err(MayAgentError::Llm(format!(
            "Provider '{provider_id}' cannot serve configured model '{}'",
            selector.model_id()
        )));
    }

    Ok((selector, provider_instance))
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

/// Number of runnable provider candidates for `selector`.
#[must_use]
pub fn provider_candidate_count(config: &MayConfig, selector: &str) -> usize {
    usize::from(resolve_model_selector(config, selector).is_ok())
}

/// The concrete runtime produced by [`resolve_provider_runtime`].
#[derive(Debug)]
pub struct ResolvedProviderRuntime {
    /// Resolved provider identifier.
    pub provider_id: String,
    /// Configured LLM client for the provider.
    pub llm: CloudProvider,
    /// Embedding model resolution used by retrieval tools.
    pub search_embedder: EmbeddingModelResolution,
}

/// Resolve a model selector against `may_config` into a usable provider
/// runtime, trying fallback candidates in order.
///
/// # Errors
///
/// Returns [`MayAgentError`] when no configured provider can serve the
/// selector or all candidates fail.
pub async fn resolve_provider_runtime(
    may_config: &MayConfig,
    selector: &str,
    _provider_attempt: usize,
) -> Result<ResolvedProviderRuntime, MayAgentError> {
    let (selector, provider_instance) = resolve_model_selector(may_config, selector)?;
    let provider_id = selector.provider_id().to_string();
    let llm =
        build_cloud_provider_from_instance(&provider_id, &provider_instance, selector.model_id())
            .await?;

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

/// Tier-resolved models for the `ask` terminal command.
///
/// `None` for a tier means no distinct model is configured (or it failed to
/// initialize) and the session model should answer that tier.
#[derive(Debug, Clone, Default)]
pub struct AskTierModels {
    pub flagship: Option<CloudProvider>,
    pub balanced: Option<CloudProvider>,
    pub fast: Option<CloudProvider>,
}

/// Resolves per-tier models for the `ask` command from the configured tier
/// defaults, skipping tiers that resolve to the session's own model.
pub async fn resolve_ask_tier_models(
    config: &MayConfig,
    session_provider_id: &str,
    session_model_id: &str,
) -> AskTierModels {
    let mut models = AskTierModels::default();
    for (tier, slot) in [
        (crate::config::ModelTier::Advanced, &mut models.flagship),
        (crate::config::ModelTier::Balanced, &mut models.balanced),
        (crate::config::ModelTier::Fast, &mut models.fast),
    ] {
        let Some(selector) = config.default_model_for_tier(tier) else {
            continue;
        };
        if selector.provider_id() == session_provider_id && selector.model_id() == session_model_id
        {
            continue;
        }
        let Some(instance) = config.get_provider(selector.provider_id()) else {
            tracing::warn!(
                tier = ?tier,
                provider_id = selector.provider_id(),
                "ask tier model references an unknown provider; tier falls back to session model"
            );
            continue;
        };
        match build_cloud_provider_from_instance(
            selector.provider_id(),
            instance,
            selector.model_id(),
        )
        .await
        {
            Ok(llm) => *slot = Some(llm),
            Err(error) => {
                tracing::warn!(
                    tier = ?tier,
                    provider_id = selector.provider_id(),
                    model = selector.model_id(),
                    error = %error,
                    "failed to initialize ask tier model; tier falls back to session model"
                );
            }
        }
    }
    models
}

/// Builds a fast+balanced provider chain for background memory extraction.
///
/// # Errors
///
/// Returns [`MayAgentError`] when a configured selector cannot be resolved
/// into a provider, and [`MayAgentError::MissingApiKey`] when a candidate
/// provider is not authenticated.
///
/// # Panics
///
/// Panics when a configured model selector references a provider that is not
/// in the provider map — config validation is expected to reject that
/// earlier.
pub async fn build_fast_provider_chain(
    config: &MayConfig,
) -> Result<Vec<crate::memory::MemoryExtractionProvider>, MayAgentError> {
    let mut selectors = Vec::new();
    let mut seen = HashSet::new();
    for tier in [
        crate::config::ModelTier::Fast,
        crate::config::ModelTier::Balanced,
    ] {
        for selector in config.models.models_for_tier(tier) {
            if seen.insert(selector.clone()) {
                selectors.push(selector.clone());
            }
        }
    }

    if selectors.is_empty() {
        return Err(MayAgentError::MissingApiKey(
            ERR_NO_MODELS.trim().to_string(),
        ));
    }
    let first_model = selectors[0].model_id().to_string();

    let mut built = Vec::new();
    let mut last_error: Option<MayAgentError> = None;

    for selector in selectors {
        let provider_id = selector.provider_id().to_string();
        let Some(instance) = config.get_provider(&provider_id) else {
            panic!("configured model selector referenced unknown provider '{provider_id}'");
        };

        match build_cloud_provider_from_instance(&provider_id, instance, selector.model_id()).await
        {
            Ok(llm) => built.push(crate::memory::MemoryExtractionProvider {
                provider_id: provider_id.clone(),
                model: selector.model_id().to_string(),
                llm,
            }),
            Err(error) => {
                tracing::warn!(
                    provider_id = %provider_id,
                    model = selector.model_id(),
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
        ProviderType::OpenAI
        | ProviderType::Groq
        | ProviderType::Together
        | ProviderType::Mistral
        | ProviderType::XAI => {
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
        ProviderType::Chatgpt => {
            let Some(credentials) = crate::chatgpt::load_credentials().await? else {
                return Err(MayAgentError::MissingApiKey(
                    "ChatGPT not authenticated. Use /config to set up OAuth.".to_string(),
                ));
            };
            let mut client = Codex::new(credentials, model).on_refresh(|rotated| {
                // Persist rotated credentials in the background; the refresh
                // token is single-use so losing it would force re-login.
                let rotated = rotated.clone();
                executor_core::spawn(async move {
                    if let Err(e) = crate::chatgpt::save_credentials(&rotated).await {
                        tracing::warn!("failed to persist rotated ChatGPT tokens: {e}");
                    }
                })
                .detach();
            });
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            client.into()
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

fn is_copilot_unauthorized(err: &crate::copilot::CopilotTokenError) -> bool {
    matches!(
        err,
        crate::copilot::CopilotTokenError::Exchange(error)
            if error.status() == Some(401)
    )
}
