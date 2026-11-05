//! Per-session provider-usage aggregation for `/usage`: one row per model
//! plus session-level API time, wall-clock duration and cost totals, and the
//! session's prompt-cache misses by cause.

use super::cache_miss::CacheMissCause;
use super::wall_clock::{Interval, IntervalSet, WallClockSplit};
use crate::db::SessionIntervals;
use crate::db::entity::{cache_misses, request_logs};

/// How many of the newest cache misses `/usage` lists one by one.
const RECENT_CACHE_MISSES: usize = 10;

/// One model's aggregated usage inside a session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsageRow {
    /// Model id as the provider reported it.
    pub model: String,
    /// Requests made to the model.
    pub requests: u64,
    /// Uncached prompt tokens.
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

/// The cache misses of one cause inside a session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CacheMissCauseRow {
    /// Why those requests missed.
    pub cause: CacheMissCause,
    /// How many requests missed for it.
    pub misses: u64,
    /// Cached tokens they did not read.
    pub missed_tokens: u64,
}

/// One recorded cache miss.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CacheMissRow {
    /// When the request's usage arrived (ISO 8601).
    pub timestamp: String,
    /// Model the request targeted.
    pub model: String,
    /// Why it missed.
    pub cause: CacheMissCause,
    /// Cached tokens it did not read.
    pub missed_tokens: u64,
    /// Time since the previous request on its route, milliseconds.
    pub gap_ms: u64,
    /// Kind of the first prefix block it changed: `tools` or a message role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diverged_kind: Option<String>,
    /// Message index of that block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diverged_index: Option<u64>,
}

impl CacheMissRow {
    fn from_model(row: &cache_misses::Model) -> Self {
        Self {
            timestamp: row.timestamp.clone(),
            model: row.model.clone(),
            cause: stored_cause(row),
            missed_tokens: row.missed_tokens.max(0).cast_unsigned(),
            gap_ms: row.gap_ms.max(0).cast_unsigned(),
            diverged_kind: row.diverged_kind.clone(),
            diverged_index: row.diverged_index.map(|index| index.max(0).cast_unsigned()),
        }
    }
}

/// A stored miss's cause; only May writes the column.
fn stored_cause(row: &cache_misses::Model) -> CacheMissCause {
    row.cause
        .parse()
        .unwrap_or_else(|error| panic!("cache_misses row {}: {error}", row.id))
}

/// Fold the session's request log into a usage report.
#[must_use]
pub fn assemble_usage_report(
    session_id: &str,
    rows: &[request_logs::Model],
    misses: &[cache_misses::Model],
    intervals: &SessionIntervals,
    created_at_ms: Option<i64>,
    now_ms: i64,
) -> SessionUsageReport {
    let mut models: Vec<ModelUsageRow> = Vec::new();
    let model_row = |model: &str| ModelUsageRow {
        model: model.to_string(),
        requests: 0,
        prompt_tokens: 0,
        completion_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        api_ms: 0,
        cache_hit_rate: 0.0,
        cache_misses: 0,
        cache_miss_tokens: 0,
        cost_usd: None,
    };
    for row in rows {
        let index = models
            .iter()
            .position(|model| model.model == row.model)
            .unwrap_or_else(|| {
                models.push(model_row(&row.model));
                models.len() - 1
            });
        let entry = &mut models[index];
        entry.requests += 1;
        entry.prompt_tokens += u64::try_from(row.prompt_tokens.unwrap_or(0)).unwrap_or_default();
        entry.completion_tokens +=
            u64::try_from(row.completion_tokens.unwrap_or(0)).unwrap_or_default();
        entry.cache_read_tokens +=
            u64::try_from(row.cache_read_tokens.unwrap_or(0)).unwrap_or_default();
        entry.cache_write_tokens +=
            u64::try_from(row.cache_write_tokens.unwrap_or(0)).unwrap_or_default();
        entry.api_ms += row.elapsed_ms.max(0).cast_unsigned();
    }

    let mut cache_miss_causes: Vec<CacheMissCauseRow> = Vec::new();
    for miss in misses {
        let missed_tokens = miss.missed_tokens.max(0).cast_unsigned();
        let index = models
            .iter()
            .position(|model| model.model == miss.model)
            .unwrap_or_else(|| {
                models.push(model_row(&miss.model));
                models.len() - 1
            });
        models[index].cache_misses += 1;
        models[index].cache_miss_tokens += missed_tokens;
        let cause = stored_cause(miss);
        let index = cache_miss_causes
            .iter()
            .position(|row| row.cause == cause)
            .unwrap_or_else(|| {
                cache_miss_causes.push(CacheMissCauseRow {
                    cause,
                    misses: 0,
                    missed_tokens: 0,
                });
                cache_miss_causes.len() - 1
            });
        cache_miss_causes[index].misses += 1;
        cache_miss_causes[index].missed_tokens += missed_tokens;
    }
    cache_miss_causes.sort_by_key(|row| std::cmp::Reverse(row.missed_tokens));
    let recent_cache_misses = misses
        .iter()
        .rev()
        .take(RECENT_CACHE_MISSES)
        .map(CacheMissRow::from_model)
        .collect();

    let mut api_ms = 0;
    let mut total_cost: Option<f64> = None;
    for entry in &mut models {
        api_ms += entry.api_ms;
        let input = entry.prompt_tokens + entry.cache_read_tokens;
        entry.cache_hit_rate = if input > 0 {
            entry.cache_read_tokens as f64 / input as f64
        } else {
            0.0
        };
        entry.cost_usd = cost_usd(entry);
        if let Some(cost) = entry.cost_usd {
            total_cost = Some(total_cost.unwrap_or(0.0) + cost);
        }
    }
    models.sort_by_key(|model| std::cmp::Reverse(model.prompt_tokens + model.cache_read_tokens));

    // The session clock starts at creation; without a creation time it
    // starts at the first request.
    // A request is logged when it ends, so it ran for `elapsed_ms` before.
    let requests: Vec<Interval> = rows
        .iter()
        .filter_map(|row| {
            let end = row.timestamp.parse::<jiff::Timestamp>().ok()?;
            Some(Interval::ending_at(
                end.as_millisecond(),
                row.elapsed_ms.max(0).cast_unsigned(),
            ))
        })
        .collect();
    let start_ms = created_at_ms.or_else(|| requests.iter().map(|span| span.start_ms()).min());
    let wall = start_ms.map_or_else(
        || Interval::ending_at(now_ms, 0),
        |start| Interval::clamped(start, now_ms),
    );
    let split = WallClockSplit::attribute(
        wall,
        &IntervalSet::new(requests),
        &intervals.tools,
        &intervals.user_waits,
    );

    SessionUsageReport {
        session_id: session_id.to_string(),
        api_ms,
        wall_ms: wall.len_ms(),
        split,
        total_cost_usd: total_cost,
        models,
        cache_miss_causes,
        recent_cache_misses,
    }
}

/// The known cost of one aggregated row: input and output rates plus the
/// cache rates the provider charges, `None` when the model is unknown.
fn cost_usd(row: &ModelUsageRow) -> Option<f64> {
    // Logged model ids may carry the provider prefix (`relay::o4-mini-high`);
    // the catalog keys on the bare model id.
    let bare = row.model.rsplit("::").next().unwrap_or(&row.model);
    let pricing = aither_models::lookup(bare).map(aither_models::ModelEntry::pricing)?;
    let mut cost = (row.completion_tokens as f64).mul_add(
        pricing.output_per_token(),
        row.prompt_tokens as f64 * pricing.input_per_token(),
    );
    if let Some(rate) = pricing.cache_read_per_token() {
        cost = (row.cache_read_tokens as f64).mul_add(rate, cost);
    }
    if let Some(rate) = pricing.cache_write_per_token() {
        cost = (row.cache_write_tokens as f64).mul_add(rate, cost);
    }
    Some(cost)
}
