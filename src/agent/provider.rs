//! Provider selection and LLM client construction for `MayAgent`.
//!
//! Resolves an explicit `<provider_id>::<model_id>` selector into one
//! concrete provider instance, then builds the `CloudProvider` client
//! with the right base URL, API key, and per-provider quirks
//! (`OpenRouter`, `DeepSeek`, Ollama, `LMStudio`, Copilot, custom
//! OpenAI-compatible proxies, etc.). Also exposes [`ModelSlot`], a model
//! paired with the [`Parameters`] aither sends its requests with.

use std::collections::HashSet;

use aither_cloud::{
    CloudProvider, claude::Claude, codex::Codex, copilot::Copilot, gemini::Gemini, openai::OpenAI,
};
use aither_core::LanguageModel;
use std::pin::Pin;

use aither_core::llm::{
    Classify, Event as LlmEvent, Failure, GenerateError, LLMRequest, TokenCounting,
    model::{Ability, Parameters, Profile as ModelProfile, ReasoningEffort},
};
use futures_lite::{Stream, StreamExt as _};

use super::may::MayAgentError;
use crate::config::{
    MayConfig, ModelTier, NativeToolsConfig, ProviderApi, ProviderInstance, ProviderType,
};
use crate::copilot::clear_copilot_credentials;
use crate::model_selector::ModelSelector;
use crate::secrets::SecretRedactor;

const ERR_NO_MODELS: &str = include_str!("../texts/agent_err_no_models.txt");

pub(super) fn err_no_provider(model: &str) -> String {
    format!("No provider available for model '{model}'. Use /config to add providers.")
}

pub(super) fn err_api_key_required(provider: &str) -> String {
    format!("API key required for provider '{provider}'")
}

/// Parses a session `provider::model` selector string.
pub(super) fn parse_model_selector(selector: &str) -> Result<ModelSelector, MayAgentError> {
    selector.parse::<ModelSelector>().map_err(|error| {
        MayAgentError::MissingApiKey(format!("Invalid model selector '{selector}': {error}"))
    })
}

fn resolve_model_selector(
    may_config: &MayConfig,
    selector: &ModelSelector,
) -> Result<ProviderInstance, MayAgentError> {
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

    Ok(provider_instance)
}

/// Why a [`RedactedModel`] request failed.
#[derive(Debug, thiserror::Error)]
pub enum RedactedError<E> {
    /// The stored secrets could not be read; the request was not sent.
    #[error(transparent)]
    Secrets(crate::secrets::SecretError),
    /// The inner model failed.
    #[error(transparent)]
    Model(E),
}

impl<E: Classify> Classify for RedactedError<E> {
    fn failure(&self) -> Failure {
        match self {
            Self::Secrets(_) => Failure::permanent(),
            Self::Model(error) => error.failure(),
        }
    }
}

impl<E> RedactedError<E> {
    /// Wraps the inner failure of a token count.
    fn of_count(error: GenerateError<E>) -> GenerateError<Self> {
        match error {
            GenerateError::Provider(error) => GenerateError::Provider(Self::Model(error)),
            GenerateError::Parse { source, response } => GenerateError::Parse { source, response },
        }
    }
}

/// A [`LanguageModel`] that replaces every stored secret value in a request
/// with the secret's name before the request reaches the inner model.
///
/// A request is never sent when the stored secrets cannot be read: it fails
/// with [`RedactedError::Secrets`] instead.
///
/// The secrets are read when each request is sent, so one added mid-session
/// applies from its next request. `respond` and the endpoint token count
/// redact the request they send; every other entry point of the trait
/// (tool-carrying and structured requests) is defined in terms of `respond`,
/// so no request can bypass the redaction.
#[derive(Debug, Clone)]
pub struct RedactedModel<M> {
    inner: M,
    redactor: SecretRedactor,
    /// Replaces the context length of the inner model's profile.
    context_window: Option<u32>,
}

impl<M> RedactedModel<M> {
    /// Wraps `inner`, redacting the secrets `redactor` reads.
    pub const fn new(inner: M, redactor: SecretRedactor) -> Self {
        Self {
            inner,
            redactor,
            context_window: None,
        }
    }

    /// Reports `window`, when known, as the model's context length instead
    /// of the inner model's own.
    #[must_use]
    pub const fn with_context_window(mut self, window: Option<u32>) -> Self {
        self.context_window = window;
        self
    }
}

impl<M> LanguageModel for RedactedModel<M>
where
    M: LanguageModel,
{
    type Error = RedactedError<M::Error>;

    fn respond(
        &self,
        mut request: LLMRequest,
    ) -> impl Stream<Item = Result<LlmEvent, Self::Error>> + Send {
        // Boxed here: `flatten` would otherwise embed the provider's
        // multi-KB response state machine by value in every future that
        // awaits this stream.
        Box::pin(
            futures_lite::stream::once_future(async move {
                let events: Pin<Box<dyn Stream<Item = Result<LlmEvent, Self::Error>> + Send + '_>> =
                    match self.redactor.current().await {
                        Ok(redactions) => {
                            redactions.redact_request(&mut request);
                            Box::pin(
                                self.inner
                                    .respond(request)
                                    .map(|event| event.map_err(RedactedError::Model)),
                            )
                        }
                        Err(error) => Box::pin(futures_lite::stream::once(Err(
                            RedactedError::Secrets(error),
                        ))),
                    };
                events
            })
            .flatten(),
        )
    }

    async fn profile(&self) -> Result<ModelProfile, Self::Error> {
        let mut profile = self.inner.profile().await.map_err(RedactedError::Model)?;
        if let Some(window) = self.context_window {
            profile.context_length = window;
        }
        Ok(profile)
    }

    fn token_counting(&self) -> TokenCounting {
        self.inner.token_counting()
    }

    /// Counts the redacted request: an endpoint count sends it to the
    /// provider.
    fn count_tokens(
        &self,
        request: &LLMRequest,
    ) -> impl Future<Output = Result<Option<u32>, GenerateError<Self::Error>>> + Send {
        let mut request = request.clone();
        async move {
            let redactions = self
                .redactor
                .current()
                .await
                .map_err(|error| GenerateError::Provider(RedactedError::Secrets(error)))?;
            redactions.redact_request(&mut request);
            self.inner
                .count_tokens(&request)
                .await
                .map_err(RedactedError::of_count)
        }
    }

    /// Counts model output, which holds no secret to redact.
    fn count_text(&self, text: &str) -> Option<u32> {
        self.inner.count_text(text)
    }
}

/// The model every provider client is built as: a [`CloudProvider`] behind
/// secret redaction. [`build_cloud_provider_from_instance`] is the only
/// constructor, so no request leaves May without passing it.
pub type ProviderLlm = RedactedModel<CloudProvider>;

/// A model and the [`Parameters`] of the requests sent to it. aither takes
/// the parameters next to the model and adds the fields it owns on top.
#[derive(Debug, Clone)]
pub struct ModelSlot<M> {
    /// The model.
    pub llm: M,
    /// The parameters its requests carry.
    pub parameters: Parameters,
}

impl<M> ModelSlot<M> {
    /// Pairs `llm` with the `parameters` of its requests.
    pub const fn new(llm: M, parameters: Parameters) -> Self {
        Self { llm, parameters }
    }
}

impl<M: LanguageModel> ModelSlot<M> {
    /// Streams the model's answer to a standalone request over `messages`
    /// (a side question outside any agent) carrying this slot's parameters.
    pub fn respond(
        &self,
        messages: Vec<aither_core::llm::Message>,
    ) -> impl Stream<Item = Result<LlmEvent, M::Error>> + Send {
        self.llm
            .respond(LLMRequest::new(messages).with_parameters(self.parameters.clone()))
    }

    /// Generates a `T` from `messages` through the model's structured output,
    /// carrying this slot's parameters.
    pub fn generate<T: schemars::JsonSchema + serde::de::DeserializeOwned + 'static>(
        &self,
        messages: Vec<aither_core::llm::Message>,
    ) -> impl Future<Output = Result<T, aither_core::llm::GenerateError<M::Error>>> + Send {
        self.llm
            .generate(LLMRequest::new(messages).with_parameters(self.parameters.clone()))
    }
}

/// [`request_parameters_for`] for a session's `selector`, looking the provider
/// instance up in `config`.
pub(super) fn session_request_parameters(
    config: &MayConfig,
    selector: &ModelSelector,
    reasoning_effort: Option<ReasoningEffort>,
) -> Parameters {
    let provider_id = selector.provider_id();
    let provider_type = config
        .provider
        .get(provider_id)
        .map(|instance| instance.resolve_type(provider_id));
    let mut parameters = request_parameters_for(
        provider_id,
        config.provider.get(provider_id),
        selector.model_id(),
        reasoning_effort,
    );
    if reasoning_text_requested(provider_type, selector.model_id(), reasoning_effort) {
        parameters = parameters.include_reasoning(true);
    }
    parameters
}

/// Whether a session's requests should ask the provider to stream its
/// reasoning text, so the UI can show the thinking block.
///
/// An explicit effort asks for it on every provider. A model that reasons by
/// default gets it too — except on Anthropic, where asking alone would turn
/// adaptive thinking on for models whose default is off.
pub fn reasoning_text_requested(
    provider_type: Option<ProviderType>,
    model_id: &str,
    effort: Option<ReasoningEffort>,
) -> bool {
    if matches!(effort, Some(ReasoningEffort::None)) {
        return false;
    }
    if effort.is_some() {
        return true;
    }
    provider_type.is_some_and(|t| t != ProviderType::Anthropic)
        && aither_models::lookup(model_id).is_some_and(|info| info.has_ability(Ability::Reasoning))
}

/// Computes the [`Parameters`] a model should run with: `reasoning_effort`
/// plus the provider-hosted web tools `instance`'s `native_tools` enables
/// for the provider type `provider_id` resolves to, gated by the model's own
/// abilities.
///
/// The prompt cache is aither's: it sets the key and breakpoints itself.
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
            request_parameters.native_tools.claude.web_search = model_has_web_search;
            request_parameters.native_tools.claude.web_fetch = native.web_fetch;
        }
        Some(ProviderType::Google) => {
            request_parameters.websearch = model_has_web_search;
            request_parameters.native_tools.gemini.url_context = native.web_fetch;
        }
        Some(ProviderType::OpenAI) => {
            request_parameters.websearch = model_has_web_search;
        }
        Some(ProviderType::Chatgpt) => {
            // Hosted search is included in the subscription. Codex slugs are
            // absent from aither-models, so the Ability::WebSearch gate would
            // always read false here.
            request_parameters.websearch = true;
        }
        Some(ProviderType::Custom) => {
            // A custom endpoint's support is decided by the backend — config
            // validation already guarantees this arm only when the provider
            // speaks the Responses API.
            request_parameters.websearch = native.web_search;
        }
        _ => {}
    }
    request_parameters
}

/// Provider candidates for `selector` in failover order: the provider the
/// selector names first, then every configured generic proxy able to serve
/// the model, ordered by provider id for a stable retry sequence.
fn provider_candidates(
    config: &MayConfig,
    selector: &ModelSelector,
) -> Vec<(String, ProviderInstance)> {
    let mut candidates: Vec<(String, ProviderInstance)> = resolve_model_selector(config, selector)
        .map(|instance| (selector.provider_id().to_string(), instance))
        .into_iter()
        .collect();
    let mut proxies = config
        .provider
        .iter()
        .filter(|(id, instance)| {
            id.as_str() != selector.provider_id()
                && instance.is_valid(id)
                && instance.is_generic_model_proxy(id)
                && instance.can_serve_model(id, selector.model_id())
        })
        .map(|(id, instance)| (id.clone(), instance.clone()))
        .collect::<Vec<_>>();
    proxies.sort_by(|left, right| left.0.cmp(&right.0));
    candidates.extend(proxies);
    candidates
}

/// Number of runnable provider candidates for `selector`.
#[must_use]
pub fn provider_candidate_count(config: &MayConfig, selector: &str) -> usize {
    parse_model_selector(selector)
        .map_or(0, |selector| provider_candidates(config, &selector).len())
}

/// The concrete runtime produced by [`resolve_provider_runtime`].
#[derive(Debug)]
pub struct ResolvedProviderRuntime {
    /// Resolved provider identifier.
    pub provider_id: String,
    /// Configured LLM client for the provider.
    pub llm: ProviderLlm,
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
    selector: &ModelSelector,
    provider_attempt: usize,
    redactor: &SecretRedactor,
) -> Result<ResolvedProviderRuntime, MayAgentError> {
    let candidates = provider_candidates(may_config, selector);
    let Some((provider_id, provider_instance)) = candidates.get(provider_attempt) else {
        return Err(MayAgentError::MissingApiKey(err_no_provider(
            selector.model_id(),
        )));
    };
    let llm = build_cloud_provider_from_instance(
        provider_id,
        provider_instance,
        selector.model_id(),
        redactor,
    )
    .await?;

    Ok(ResolvedProviderRuntime {
        provider_id: provider_id.clone(),
        llm,
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
    pub flagship: Option<ModelSlot<ProviderLlm>>,
    /// Model for the balanced tier.
    pub balanced: Option<ModelSlot<ProviderLlm>>,
    /// Model for the fast tier.
    pub fast: Option<ModelSlot<ProviderLlm>>,
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
    redactor: &SecretRedactor,
) -> Result<AskTierModels, MayAgentError> {
    let (flagship, balanced, fast) = futures_util::future::try_join3(
        resolve_ask_tier_model(
            config,
            ModelTier::Advanced,
            session_provider_id,
            session_model_id,
            redactor,
        ),
        resolve_ask_tier_model(
            config,
            ModelTier::Balanced,
            session_provider_id,
            session_model_id,
            redactor,
        ),
        resolve_ask_tier_model(
            config,
            ModelTier::Fast,
            session_provider_id,
            session_model_id,
            redactor,
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
    redactor: &SecretRedactor,
) -> Result<Option<ModelSlot<ProviderLlm>>, MayAgentError> {
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
    let llm =
        build_cloud_provider_from_instance(provider_id, instance, selector.model_id(), redactor)
            .await
            .map_err(|error| {
                MayAgentError::Llm(format!(
                    "failed to build ask {tier:?} tier model '{selector}': {error}"
                ))
            })?;
    Ok(Some(ModelSlot::new(
        llm,
        request_parameters_for(provider_id, Some(instance), selector.model_id(), None),
    )))
}

/// Builds the configured compaction model wrapped in the [`Parameters`] of its
/// own provider instance. `None` means compaction runs on the session model:
/// no compaction model is configured, or it is the session's own model.
///
/// # Errors
///
/// Returns [`MayAgentError`] when the selector references a provider missing
/// from `config` or its model fails to build.
pub async fn resolve_compaction_model(
    config: &MayConfig,
    session_provider_id: &str,
    session_model_id: &str,
    redactor: &SecretRedactor,
) -> Result<Option<ModelSlot<ProviderLlm>>, MayAgentError> {
    let Some(selector) = config.models.compaction_model.as_ref() else {
        return Ok(None);
    };
    let provider_id = selector.provider_id();
    if provider_id == session_provider_id && selector.model_id() == session_model_id {
        return Ok(None);
    }
    let Some(instance) = config.get_provider(provider_id) else {
        return Err(MayAgentError::MissingApiKey(format!(
            "compaction model '{selector}' references provider '{provider_id}', \
             which is not configured"
        )));
    };
    let llm =
        build_cloud_provider_from_instance(provider_id, instance, selector.model_id(), redactor)
            .await
            .map_err(|error| {
                MayAgentError::Llm(format!(
                    "failed to build compaction model '{selector}': {error}"
                ))
            })?;
    Ok(Some(ModelSlot::new(
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
    redactor: &SecretRedactor,
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

        match build_cloud_provider_from_instance(
            &provider_id,
            instance,
            selector.model_id(),
            redactor,
        )
        .await
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

/// Build a `CloudProvider` from a `ProviderInstance` directly, behind the
/// secret redaction of `redactor`.
pub(super) async fn build_cloud_provider_from_instance(
    id: &str,
    instance: &ProviderInstance,
    model: &str,
    redactor: &SecretRedactor,
) -> Result<ProviderLlm, MayAgentError> {
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
        ProviderType::Grok => {
            let Some(token) = crate::grok::get_access_token().await? else {
                return Err(MayAgentError::MissingApiKey(
                    "xAI Grok not authenticated. Use /login to sign in.".to_string(),
                ));
            };
            let mut client = OpenAI::new(&token).with_model(model);
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
            let client = OpenAI::new(api_key).with_base_url(url).with_model(model);
            match instance.api {
                Some(ProviderApi::Responses) => client.with_responses_api().into(),
                _ => client.with_chat_completions_api().into(),
            }
        }
    };

    Ok(RedactedModel::new(llm, redactor.clone()))
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
    fn reasoning_text_requested_follows_effort_and_anthropic_guard() {
        // Explicit effort asks for reasoning text on every provider.
        assert!(reasoning_text_requested(
            Some(ProviderType::Anthropic),
            "claude-opus-4-5",
            Some(ReasoningEffort::Low),
        ));
        // Effort "none" never asks — it exists to disable thinking.
        assert!(!reasoning_text_requested(
            Some(ProviderType::OpenAI),
            "gpt-5",
            Some(ReasoningEffort::None),
        ));
        // Without effort, a reasoning-by-default model gets its text on
        // non-Anthropic providers; Anthropic stays silent so asking cannot
        // turn thinking on for models whose default is off.
        assert!(reasoning_text_requested(
            Some(ProviderType::OpenAI),
            "gpt-5",
            None,
        ));
        assert!(!reasoning_text_requested(
            Some(ProviderType::Anthropic),
            "claude-opus-4-5",
            None,
        ));
        // A model without reasoning ability is never asked.
        assert!(!reasoning_text_requested(
            Some(ProviderType::OpenAI),
            "gpt-4o-mini",
            None,
        ));
    }

    #[test]
    fn request_parameters_for_chatgpt_defaults_web_search_on() {
        let instance = ProviderInstance::new(ProviderType::Chatgpt);
        let parameters = request_parameters_for("chatgpt", Some(&instance), "gpt-5", None);
        assert!(parameters.websearch);

        let mut fetch_only = ProviderInstance::new(ProviderType::Chatgpt);
        fetch_only.native_tools.web_fetch = true;
        let parameters = request_parameters_for("chatgpt", Some(&fetch_only), "gpt-5", None);
        assert!(parameters.websearch);
    }

    #[test]
    fn request_parameters_for_custom_responses_honors_web_search() {
        let mut instance =
            ProviderInstance::new(ProviderType::Custom).with_base_url("http://127.0.0.1:8787/v1");
        instance.api = Some(ProviderApi::Responses);
        let parameters = request_parameters_for("relay", Some(&instance), "o4-mini-high", None);
        assert!(!parameters.websearch);

        instance.native_tools.web_search = true;
        let parameters = request_parameters_for("relay", Some(&instance), "o4-mini-high", None);
        assert!(parameters.websearch);
    }

    #[test]
    fn request_parameters_set_no_prompt_cache_key() {
        for (id, provider_type) in [
            ("relay", ProviderType::Custom),
            ("chatgpt", ProviderType::Chatgpt),
            ("openai", ProviderType::OpenAI),
        ] {
            let instance = ProviderInstance::new(provider_type);
            let parameters = request_parameters_for(id, Some(&instance), "gpt-5", None);
            assert!(parameters.cache.openai.is_none(), "{id}");
        }
    }

    #[test]
    fn provider_validation_rejects_native_tools_without_responses_api() {
        let mut instance =
            ProviderInstance::new(ProviderType::Custom).with_base_url("http://127.0.0.1:8787/v1");
        instance.native_tools.web_search = true;
        assert!(instance.validate("relay").is_err());

        instance.api = Some(ProviderApi::Responses);
        assert!(instance.validate("relay").is_ok());

        // `api` on a non-custom provider is rejected as well.
        let mut openai = ProviderInstance::new(ProviderType::OpenAI).with_api_key("key");
        openai.api = Some(ProviderApi::Responses);
        assert!(openai.validate("openai").is_err());
    }

    #[test]
    fn request_parameters_for_anthropic_gates_on_model_ability() {
        let mut instance = ProviderInstance::new(ProviderType::Anthropic).with_api_key("key");
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

        // A model without the WebSearch ability keeps the flag off.
        let parameters = request_parameters_for(
            "anthropic",
            Some(&instance),
            "claude-haiku-4-5-20251001",
            None,
        );
        assert!(!parameters.native_tools.claude.web_search);
        assert!(parameters.native_tools.claude.web_fetch);
    }

    #[test]
    fn session_selector_enables_native_web_search() {
        let mut config = MayConfig::default();
        let instance = ProviderInstance::new(ProviderType::Google).with_api_key("key");
        config.provider.insert("google".to_string(), instance);

        let selector = parse_model_selector("google::gemini-3-flash-preview").unwrap();
        let parameters = session_request_parameters(&config, &selector, None);
        assert!(parameters.websearch);
        assert!(aither_models::lookup(selector.model_id()).is_some());
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

        let models = resolve_ask_tier_models(
            &config,
            "google",
            "gemini-3-flash-preview",
            &SecretRedactor::new(std::path::Path::new("/nonexistent")),
        )
        .await
        .expect("tier models resolve");

        let fast = models.fast.expect("fast tier model is built");
        let parameters = &fast.parameters;
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

        let error = resolve_ask_tier_models(
            &config,
            "google",
            "gemini-3-flash-preview",
            &SecretRedactor::new(std::path::Path::new("/nonexistent")),
        )
        .await
        .expect_err("an unknown tier provider must fail resolution");

        let message = error.to_string();
        assert!(message.contains("Fast"), "{message}");
        assert!(message.contains("ghost"), "{message}");
        assert!(message.contains("some-model"), "{message}");
    }
}
