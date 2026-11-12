//! The client a session's model requests go through: a provider client
//! built from configuration, or, on a cloud home, one that follows the
//! access token the fronting daemon pushes.

use std::sync::Arc;

use aither_cloud::{CloudError, CloudProvider, codex::Codex, copilot::Copilot, openai::OpenAI};
use aither_core::LanguageModel;
use aither_core::llm::{
    Event as LlmEvent, GenerateError, LLMRequest, TokenCounting, model::Profile as ModelProfile,
};
use arc_swap::ArcSwapOption;
use futures_lite::{Stream, StreamExt as _};

use crate::cloud::access::{AccessCell, AccessToken};

/// A session model's client.
#[derive(Clone)]
pub enum SessionModel {
    /// A client built once from the provider's configuration.
    Built(CloudProvider),
    /// A cloud home's OAuth provider, following the pushed access token.
    Pushed(PushedModel),
}

impl std::fmt::Debug for SessionModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Built(provider) => f.debug_tuple("Built").field(provider).finish(),
            Self::Pushed(model) => f.debug_tuple("Pushed").field(model).finish(),
        }
    }
}

impl From<CloudProvider> for SessionModel {
    fn from(provider: CloudProvider) -> Self {
        Self::Built(provider)
    }
}

impl SessionModel {
    /// The client of the request about to be sent.
    fn client(&self) -> CloudProvider {
        match self {
            Self::Built(provider) => provider.clone(),
            Self::Pushed(model) => model.client(),
        }
    }
}

impl LanguageModel for SessionModel {
    type Error = CloudError;

    fn respond(
        &self,
        request: LLMRequest,
    ) -> impl Stream<Item = Result<LlmEvent, Self::Error>> + Send {
        // The client is owned by the stream, so a push during the request
        // changes only the next one.
        let client = self.client();
        async_stream::stream! {
            let mut events = std::pin::pin!(client.respond(request));
            while let Some(event) = events.next().await {
                yield event;
            }
        }
    }

    fn token_counting(&self) -> TokenCounting {
        self.client().token_counting()
    }

    async fn count_tokens(
        &self,
        request: &LLMRequest,
    ) -> Result<Option<u32>, GenerateError<Self::Error>> {
        self.client().count_tokens(request).await
    }

    fn count_text(&self, text: &str) -> Option<u32> {
        self.client().count_text(text)
    }

    async fn profile(&self) -> Result<ModelProfile, Self::Error> {
        self.client().profile().await
    }
}

/// A model of a cloud home's OAuth provider: each request uses the access
/// token the fronting daemon pushed last. It never refreshes the token
/// itself; that is the fronting daemon's.
#[derive(Clone)]
pub struct PushedModel {
    access: AccessCell,
    base_url: Option<String>,
    model: String,
    /// The client of the token last used, rebuilt when a push replaces it.
    built: Arc<ArcSwapOption<(Arc<AccessToken>, CloudProvider)>>,
}

impl std::fmt::Debug for PushedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushedModel")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl PushedModel {
    /// `model` of the provider whose token `access` holds, at `base_url`
    /// when the configuration overrides it.
    pub(crate) fn new(access: AccessCell, base_url: Option<&str>, model: &str) -> Self {
        Self {
            access,
            base_url: base_url.map(str::to_owned),
            model: model.to_owned(),
            built: Arc::new(ArcSwapOption::empty()),
        }
    }

    fn client(&self) -> CloudProvider {
        let token = self.access.current();
        if let Some(built) = self.built.load_full()
            && Arc::ptr_eq(&built.0, &token)
        {
            return built.1.clone();
        }
        let client = self.build(&token);
        self.built.store(Some(Arc::new((token, client.clone()))));
        client
    }

    fn build(&self, token: &AccessToken) -> CloudProvider {
        match token {
            AccessToken::Copilot {
                token,
                api_endpoint,
            } => Copilot::new(token.clone())
                .with_base_url(api_endpoint.clone())
                .with_model(&self.model)
                .into(),
            AccessToken::Chatgpt {
                access_token,
                id_token,
                account_id,
                residency,
                expires_at,
            } => {
                // No refresh token: an expired token fails the request
                // rather than refreshing here.
                let credentials = aither_codex::auth::CodexCredentials {
                    access_token: access_token.clone(),
                    refresh_token: String::new(),
                    id_token: id_token.clone(),
                    account_id: account_id.clone(),
                    residency: residency.clone(),
                    expires_at: *expires_at,
                    earliest_refresh_at: None,
                };
                let mut client = Codex::new(credentials, &self.model);
                if let Some(url) = &self.base_url {
                    client = client.with_base_url(url);
                }
                client.into()
            }
            AccessToken::Grok { access_token } => {
                let mut client = OpenAI::new(access_token).with_model(&self.model);
                if let Some(url) = &self.base_url {
                    client = client.with_base_url(url);
                }
                client.into()
            }
        }
    }
}
