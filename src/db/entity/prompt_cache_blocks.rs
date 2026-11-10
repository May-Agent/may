//! The cache block size a prompt-cache route's reads imply, for a provider
//! that publishes none.

/// The greatest common divisor of the nonzero cache reads observed on one
/// route (session, provider, model).
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Session the route belongs to.
    pub session_id: String,
    /// Provider of the route.
    pub provider_id: String,
    /// Model of the route.
    pub model: String,
    /// The inferred block size in tokens, never zero.
    pub block_tokens: i64,
}
