//! Persisted prompt-cache misses behind `/usage`.

/// One provider request that read less from the prompt cache than its route
/// had cached.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Row id.
    pub id: i32,
    /// Session the request belongs to.
    pub session_id: String,
    /// When the request's usage arrived (ISO 8601).
    pub timestamp: String,
    /// Model id the request targeted.
    pub model: String,
    /// Provider that served the request.
    pub provider_id: String,
    /// Stored name of the [`CacheMissCause`](crate::agent::cache_miss::CacheMissCause).
    pub cause: String,
    /// Tokens the route had cached that the request did not read.
    pub missed_tokens: i64,
    /// Tokens the request did read from the cache.
    pub read_tokens: i64,
    /// Time since the previous request on the route, milliseconds.
    pub gap_ms: i64,
    /// Kind of the first changed prefix block: `tools` or a message role.
    pub diverged_kind: Option<String>,
    /// Message index of the first changed prefix block.
    pub diverged_index: Option<i64>,
    /// A compaction ran since the previous request on the route.
    pub compacted: bool,
    /// The window was reassembled since the previous request on the route.
    pub reassembled: bool,
    /// The request came from a new agent.
    pub agent_rebuilt: bool,
    /// The session used another route since the previous request on this one.
    pub model_switched: bool,
}
