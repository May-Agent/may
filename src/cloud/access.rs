//! The model provider a fronting daemon pushes to its cloud home.
//!
//! A cloud session's turns run on the codespace, so its daemon needs the
//! session model's provider. The fronting daemon pushes the provider's
//! configuration over the authenticated forward when the codespace comes
//! up and before each message: an API key goes as it is configured, while
//! an OAuth provider gets only a short-lived access token. The refresh
//! token never leaves this machine; the fronting daemon refreshes it and
//! pushes the new access token, so the two daemons never refresh the same
//! token.
//!
//! On the cloud home the access token lives in an [`AccessCell`] beside the
//! provider's configuration, never in a file. Each push replaces the token
//! in the cell its models already read, so the session's idle agent picks
//! the new token up without being rebuilt.

use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

use crate::config::{ProviderInstance, ProviderType};

/// A short-lived access token of an OAuth provider, without what refreshes
/// it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccessToken {
    /// A GitHub Copilot session token and the API endpoint it is for.
    Copilot {
        /// The session token.
        token: String,
        /// The API endpoint the token is for.
        api_endpoint: String,
    },
    /// A `ChatGPT` access token and the claims requests carry.
    Chatgpt {
        /// The bearer token.
        access_token: String,
        /// The `OpenID` ID token, a source of the account claim.
        id_token: Option<String>,
        /// The `ChatGPT-Account-Id` the requests carry.
        account_id: Option<String>,
        /// The compute-residency constraint.
        residency: Option<String>,
        /// When the access token expires, in seconds since the Unix epoch.
        expires_at: Option<u64>,
    },
    /// An xAI Grok access token.
    Grok {
        /// The bearer token.
        access_token: String,
    },
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::Copilot { .. } => "Copilot",
            Self::Chatgpt { .. } => "Chatgpt",
            Self::Grok { .. } => "Grok",
        };
        f.debug_struct("AccessToken")
            .field("kind", &kind)
            .finish_non_exhaustive()
    }
}

impl AccessToken {
    /// The access part of `credentials`; the refresh token stays behind.
    #[must_use]
    pub fn of_codex(credentials: &aither_codex::auth::CodexCredentials) -> Self {
        Self::Chatgpt {
            access_token: credentials.access_token.clone(),
            id_token: credentials.id_token.clone(),
            account_id: credentials.account_id.clone(),
            residency: credentials.residency.clone(),
            expires_at: credentials.expires_at,
        }
    }

    /// The provider type the token is for.
    #[must_use]
    pub const fn provider_type(&self) -> ProviderType {
        match self {
            Self::Copilot { .. } => ProviderType::Copilot,
            Self::Chatgpt { .. } => ProviderType::Chatgpt,
            Self::Grok { .. } => ProviderType::Grok,
        }
    }
}

/// The access token a cloud home's OAuth provider uses, replaced in place by
/// each push. Clones share the token.
#[derive(Clone)]
pub struct AccessCell(Arc<ArcSwap<AccessToken>>);

impl std::fmt::Debug for AccessCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AccessCell").finish_non_exhaustive()
    }
}

impl AccessCell {
    /// A cell holding `token`.
    #[must_use]
    pub fn holding(token: AccessToken) -> Self {
        Self(Arc::new(ArcSwap::from_pointee(token)))
    }

    /// The token the latest push brought.
    #[must_use]
    pub fn current(&self) -> Arc<AccessToken> {
        self.0.load_full()
    }

    /// Replaces the token every model reading this cell uses.
    pub fn replace(&self, token: AccessToken) {
        self.0.store(Arc::new(token));
    }
}

/// The body of `PUT /api/cloud-home/providers/{id}`: a provider's
/// configuration and, for an OAuth provider, its access token.
#[derive(Debug, Clone, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPush {
    /// The provider's configuration as `config.toml` holds it, its API key
    /// included.
    #[schema(value_type = serde_json::Value)]
    pub instance: ProviderInstance,
    /// The OAuth provider's access token; `None` for an API key provider.
    #[schema(value_type = Option<serde_json::Value>)]
    pub access: Option<AccessToken>,
}

/// Why this machine's credential of an OAuth provider is not available.
#[derive(Debug, thiserror::Error)]
pub enum AccessError {
    /// The provider is not signed in on this machine.
    #[error("{0} is not signed in on this machine; sign in with /login")]
    NotSignedIn(&'static str),
    /// GitHub Copilot's session token could not be had.
    #[error("GitHub Copilot: {0}")]
    Copilot(#[from] crate::copilot::CopilotTokenError),
    /// The `ChatGPT` credential could not be read or refreshed.
    #[error("ChatGPT: {0}")]
    Chatgpt(#[from] crate::chatgpt::ChatgptAuthError),
    /// The Grok credential could not be read or refreshed.
    #[error("xAI Grok: {0}")]
    Grok(#[from] crate::grok::GrokAuthError),
}

/// How long a `ChatGPT` access token must still be valid to be pushed: the
/// margin the model listing refreshes it with.
const CHATGPT_EXPIRY_MARGIN: std::time::Duration = std::time::Duration::from_secs(60);

/// This machine's access token of a provider of type `provider_type`,
/// refreshed here when it is about to expire; `None` for a provider that
/// takes an API key.
///
/// # Errors
/// Fails when the provider is not signed in or its token cannot be
/// refreshed.
pub async fn local_access(provider_type: ProviderType) -> Result<Option<AccessToken>, AccessError> {
    match provider_type {
        ProviderType::Copilot => {
            let credentials = crate::copilot::get_copilot_credentials()
                .await?
                .ok_or(AccessError::NotSignedIn("GitHub Copilot"))?;
            Ok(Some(AccessToken::Copilot {
                token: credentials.token,
                api_endpoint: credentials.api_endpoint,
            }))
        }
        ProviderType::Chatgpt => {
            let mut credentials = crate::chatgpt::load_credentials()
                .await?
                .ok_or(AccessError::NotSignedIn("ChatGPT"))?;
            if credentials.is_expired(CHATGPT_EXPIRY_MARGIN) {
                credentials = aither_codex::auth::refresh_tokens(&credentials.refresh_token)
                    .await
                    .map_err(crate::chatgpt::ChatgptAuthError::Codex)?;
                crate::chatgpt::save_credentials(&credentials).await?;
            }
            Ok(Some(AccessToken::of_codex(&credentials)))
        }
        ProviderType::Grok => {
            let access_token = crate::grok::get_access_token()
                .await?
                .ok_or(AccessError::NotSignedIn("xAI Grok"))?;
            Ok(Some(AccessToken::Grok { access_token }))
        }
        ProviderType::Anthropic
        | ProviderType::OpenAI
        | ProviderType::Google
        | ProviderType::OpenRouter
        | ProviderType::DeepSeek
        | ProviderType::Ollama
        | ProviderType::LMStudio
        | ProviderType::Groq
        | ProviderType::Together
        | ProviderType::Mistral
        | ProviderType::XAI
        | ProviderType::Custom => Ok(None),
    }
}
