//! The per-model usage report a session's `/usage` assembles.

use crate::cache_miss::{CacheMissCauseRow, CacheMissRow};
use crate::wall_clock::WallClockSplit;

/// One model's aggregated usage inside a session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsageRow {
    /// Model id as the provider reported it.
    pub model: String,
    /// Requests made to the model.
    pub requests: u64,
    /// Input tokens, cached ones included.
    pub prompt_tokens: u64,
    /// Completion tokens.
    pub completion_tokens: u64,
    /// Input tokens served from the provider cache.
    pub cache_read_tokens: u64,
    /// Tokens written to the provider cache.
    pub cache_write_tokens: u64,
    /// Provider wall time across the requests, milliseconds.
    pub api_ms: u64,
    /// Share of input tokens served from cache.
    pub cache_hit_rate: f64,
    /// Requests that read less from the prompt cache than the model's
    /// previous request had cached.
    pub cache_misses: u64,
    /// Cached tokens those requests did not read.
    pub cache_miss_tokens: u64,
    /// Cost in USD when the model's pricing is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// How many of `requests` only refreshed the prompt cache while a
    /// background command ran; their cost is part of the row's totals.
    #[serde(default)]
    pub keep_warm_requests: u64,
}

/// Aggregated provider usage of one session, answer of `/usage`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsageReport {
    /// Session the report covers.
    pub session_id: String,
    /// Sum of provider request wall times, milliseconds.
    pub api_ms: u64,
    /// Session age, milliseconds since it was created.
    pub wall_ms: u64,
    /// Where the wall clock went; its parts sum to `wall_ms`.
    pub split: WallClockSplit,
    /// Total known cost in USD; `None` when no model's pricing is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    /// Per-model breakdown, most input first.
    pub models: Vec<ModelUsageRow>,
    /// Cache misses by cause, most missed tokens first.
    pub cache_miss_causes: Vec<CacheMissCauseRow>,
    /// The newest cache misses, newest first.
    pub recent_cache_misses: Vec<CacheMissRow>,
}
