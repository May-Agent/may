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
    model::{Ability, Parameters, Profile as ModelProfile, ReasoningEffort},
};
use futures_lite::Stream;

use super::may::MayAgentError;
use crate::config::{MayConfig, ModelTier, NativeToolsConfig, ProviderInstance, ProviderType};
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
#[derive(Debug, Clone)]
pub struct RequestParameterizedModel<M> {
    inner: M,
    parameters: Parameters,
}

impl<M> RequestParameterizedModel<M> {
    /// Wraps `inner`, pinning `parameters` onto every request it serves.
    pub const fn new(inner: M, parameters: Parameters) -> Self {
        Self { inner, parameters }
    }

    /// The [`Parameters`] pinned onto every request routed through this model.
    #[cfg(test)]
    const fn parameters(&self) -> &Parameters {
        &self.parameters
    }
}

/// Computes the [`Parameters`] a model should run with: `reasoning_effort`
/// plus the provider-hosted web tools `instance`'s `native_tools` enables
/// for the provider type `provider_id` resolves to, gated by the model's own
/// abilities.
///
/// `instance` is `None` when `provider_id` is absent from the config.
pub(super) fn request_parameters_for(
    provider_id: &str,
    instance: Option<&ProviderInstance>,
    model_id: &str,
    reasoning_effort: Option<ReasoningEffort>,
) -> Parameters {
    let mut request_parameters = Parameters::default();
    if let Some(reasoning_effort) = reasoning_effort {
        request_parameters = request_parameters.reasoning_effort(reasoning_effort);
    }

    // Enable provider-hosted web tools only for the resolved provider type:
    // each provider crate reads its own `native_tools` section, and the
    // portable `websearch` flag reaches OpenAI/Gemini only.
    let native = instance.map_or_else(NativeToolsConfig::default, |instance| {
        instance.native_tools.clone()
    });
    let provider_type = instance.map(|instance| instance.resolve_type(provider_id));
    let model_has_web_search =
        aither_models::lookup(model_id).is_some_and(|info| info.has_ability(Ability::WebSearch));
    match provider_type {
        Some(ProviderType::Anthropic) => {
            request_parameters.native_tools.claude.web_search =
                native.web_search && model_has_web_search;
            request_parameters.native_tools.claude.web_fetch = native.web_fetch;
        }
        Some(ProviderType::Google) => {
            request_parameters.websearch = native.web_search && model_has_web_search;
            request_parameters.native_tools.gemini.url_context = native.web_fetch;
        }
        Some(ProviderType::OpenAI) => {
            request_parameters.websearch = native.web_search && model_has_web_search;
        }
        Some(ProviderType::Chatgpt) => {
            // Hosted search is included in the subscription (no per-call
            // billing), so default it on unless native_tools was configured
            // explicitly. The backend's own catalog decides support — the
            // aither-models db has no entries for codex slugs, so the
            // Ability::WebSearch gate would always read false here.
            request_parameters.websearch = native.web_search || native.is_empty();
        }
        _ => {}
    }
    request_parameters
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

/// Tier-resolved models for the `ask` terminal command, each carrying the
/// [`Parameters`] of its own provider instance and model.
///
/// `None` for a tier means no distinct model is configured and the session
/// model should answer that tier.
#[derive(Debug, Clone, Default)]
pub struct AskTierModels {
    /// Model for the flagship (`Advanced`) tier.
    pub flagship: Option<RequestParameterizedModel<CloudProvider>>,
    /// Model for the balanced tier.
    pub balanced: Option<RequestParameterizedModel<CloudProvider>>,
    /// Model for the fast tier.
    pub fast: Option<RequestParameterizedModel<CloudProvider>>,
}

/// Resolves per-tier models for the `ask` command from the configured tier
/// defaults, building them concurrently and skipping tiers that resolve to
/// the session's own model.
///
/// # Errors
///
/// Returns [`MayAgentError`] when a configured tier selector references a
/// provider missing from `config` or its model fails to build.
pub async fn resolve_ask_tier_models(
    config: &MayConfig,
    session_provider_id: &str,
    session_model_id: &str,
) -> Result<AskTierModels, MayAgentError> {
    let (flagship, balanced, fast) = futures_util::future::try_join3(
        resolve_ask_tier_model(
            config,
            ModelTier::Advanced,
            session_provider_id,
            session_model_id,
        ),
        resolve_ask_tier_model(
            config,
            ModelTier::Balanced,
            session_provider_id,
            session_model_id,
        ),
        resolve_ask_tier_model(
            config,
            ModelTier::Fast,
            session_provider_id,
            session_model_id,
        ),
    )
    .await?;
    Ok(AskTierModels {
        flagship,
        balanced,
        fast,
    })
}

/// Builds the model configured for `tier` wrapped in the [`Parameters`] of
/// its own provider instance. `None` means the session model answers the
/// tier: nothing is configured for it, or it resolves to the session's own
/// model.
async fn resolve_ask_tier_model(
    config: &MayConfig,
    tier: ModelTier,
    session_provider_id: &str,
    session_model_id: &str,
) -> Result<Option<RequestParameterizedModel<CloudProvider>>, MayAgentError> {
    let Some(selector) = config.default_model_for_tier(tier) else {
        return Ok(None);
    };
    let provider_id = selector.provider_id();
    if provider_id == session_provider_id && selector.model_id() == session_model_id {
        return Ok(None);
    }
    let Some(instance) = config.get_provider(provider_id) else {
        return Err(MayAgentError::MissingApiKey(format!(
            "ask {tier:?} tier model '{selector}' references provider '{provider_id}', \
             which is not configured"
        )));
    };
    let llm = build_cloud_provider_from_instance(provider_id, instance, selector.model_id())
        .await
        .map_err(|error| {
            MayAgentError::Llm(format!(
                "failed to build ask {tier:?} tier model '{selector}': {error}"
            ))
        })?;
    Ok(Some(RequestParameterizedModel::new(
        llm,
        request_parameters_for(provider_id, Some(instance), selector.model_id(), None),
    )))
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

#[cfg(test)]
mod tests {
    use aither_core::llm::model::ReasoningEffort;

    use super::*;
    use crate::config::MayConfig;

    #[test]
    fn request_parameters_for_google_session() {
        let mut instance = ProviderInstance::new(ProviderType::Google).with_api_key("key");
        instance.native_tools.web_search = true;
        instance.native_tools.web_fetch = true;

        let parameters = request_parameters_for(
            "google",
            Some(&instance),
            "gemini-3-flash-preview",
            Some(ReasoningEffort::High),
        );

        assert!(parameters.websearch);
        assert!(parameters.native_tools.gemini.url_context);
        assert!(!parameters.native_tools.claude.web_search);
        assert!(!parameters.native_tools.claude.web_fetch);
        assert_eq!(parameters.reasoning_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn request_parameters_for_chatgpt_defaults_web_search_on() {
        let instance = ProviderInstance::new(ProviderType::Chatgpt);
        let parameters = request_parameters_for("chatgpt", Some(&instance), "gpt-5", None);
        assert!(parameters.websearch);

        // An explicitly configured `native_tools` section drops the default.
        let mut fetch_only = ProviderInstance::new(ProviderType::Chatgpt);
        fetch_only.native_tools.web_fetch = true;
        let parameters = request_parameters_for("chatgpt", Some(&fetch_only), "gpt-5", None);
        assert!(!parameters.websearch);
    }

    #[test]
    fn request_parameters_for_anthropic_gates_on_model_ability() {
        let mut instance = ProviderInstance::new(ProviderType::Anthropic).with_api_key("key");
        instance.native_tools.web_search = true;
        instance.native_tools.web_fetch = true;

        let parameters = request_parameters_for(
            "anthropic",
            Some(&instance),
            "claude-sonnet-4-5-20250929",
            None,
        );
        assert!(parameters.native_tools.claude.web_search);
        assert!(parameters.native_tools.claude.web_fetch);
        assert!(!parameters.websearch);

        // A model without the WebSearch ability keeps the flag off even when
        // the provider instance enables it.
        let parameters = request_parameters_for(
            "anthropic",
            Some(&instance),
            "claude-haiku-4-5-20251001",
            None,
        );
        assert!(!parameters.native_tools.claude.web_search);
        assert!(parameters.native_tools.claude.web_fetch);
    }

    #[tokio::test]
    async fn ask_tier_models_carry_their_own_providers_parameters() {
        let mut config = MayConfig::default();

        // Session provider: Google with hosted web tools enabled.
        let mut google = ProviderInstance::new(ProviderType::Google).with_api_key("google-key");
        google.native_tools.web_search = true;
        google.native_tools.web_fetch = true;
        config.upsert_provider("google", google);

        // Fast tier provider: a separate Anthropic instance.
        let mut anthropic =
            ProviderInstance::new(ProviderType::Anthropic).with_api_key("anthropic-key");
        anthropic.native_tools.web_search = true;
        anthropic.native_tools.web_fetch = true;
        config.upsert_provider("anthropic", anthropic);

        config
            .models
            .add_model(ModelTier::Fast, "claude-sonnet-4-5-20250929", "anthropic");
        // The balanced tier resolving to the session's own model is skipped.
        config
            .models
            .add_model(ModelTier::Balanced, "gemini-3-flash-preview", "google");

        let models = resolve_ask_tier_models(&config, "google", "gemini-3-flash-preview")
            .await
            .expect("tier models resolve");

        let fast = models.fast.expect("fast tier model is built");
        let parameters = fast.parameters();
        // None of the Google session's flags reach the Claude tier model: it
        // carries only Claude's own `native_tools` for its instance, and no
        // reasoning effort.
        assert!(parameters.native_tools.claude.web_search);
        assert!(parameters.native_tools.claude.web_fetch);
        assert!(!parameters.websearch);
        assert!(!parameters.native_tools.gemini.url_context);
        assert!(parameters.reasoning_effort.is_none());

        assert!(models.balanced.is_none());
        assert!(models.flagship.is_none());
    }

    #[tokio::test]
    async fn ask_tier_model_with_unknown_provider_fails() {
        let mut config = MayConfig::default();
        config.upsert_provider(
            "google",
            ProviderInstance::new(ProviderType::Google).with_api_key("google-key"),
        );
        config
            .models
            .add_model(ModelTier::Fast, "some-model", "ghost");

        let error = resolve_ask_tier_models(&config, "google", "gemini-3-flash-preview")
            .await
            .expect_err("an unknown tier provider must fail resolution");

        let message = error.to_string();
        assert!(message.contains("Fast"), "{message}");
        assert!(message.contains("ghost"), "{message}");
        assert!(message.contains("some-model"), "{message}");
    }
}
