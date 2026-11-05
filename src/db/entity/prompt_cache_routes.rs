//! The latest request on each prompt-cache route of a session, which the next
//! request on that route is measured against.

/// What the latest request on one route (session, provider, model) left in
/// the provider's prompt cache.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Session the route belongs to.
    pub session_id: String,
    /// Provider of the route.
    pub provider_id: String,
    /// Model of the route.
    pub model: String,
    /// When the request's usage arrived, Unix milliseconds.
    pub observed_at_ms: i64,
    /// Tokens the request read from the cache.
    pub cache_read_tokens: i64,
    /// Tokens the request wrote to the cache, when the provider reports it.
    pub cache_write_tokens: Option<i64>,
    /// The request's prompt-prefix hash chain, `rmp_serde` encoded.
    pub chain: Vec<u8>,
}
