//! Per-session provider-usage aggregation for `/usage`: one row per model
//! plus session-level API time, wall-clock duration and cost totals.

use crate::db::entity::request_logs;

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
    /// Total known cost in USD; `None` when no model's pricing is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    /// Per-model breakdown, most input first.
    pub models: Vec<ModelUsageRow>,
}

/// Fold the session's request log into a usage report.
#[must_use]
pub fn assemble_usage_report(
    session_id: &str,
    rows: &[request_logs::Model],
    created_at_ms: Option<i64>,
    now_ms: i64,
) -> SessionUsageReport {
    let mut models: Vec<ModelUsageRow> = Vec::new();
    for row in rows {
        let index = models
            .iter()
            .position(|model| model.model == row.model)
            .unwrap_or_else(|| {
                models.push(ModelUsageRow {
                    model: row.model.clone(),
                    requests: 0,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    api_ms: 0,
                    cache_hit_rate: 0.0,
                    cost_usd: None,
                });
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
    let start_ms = created_at_ms.or_else(|| {
        rows.iter()
            .filter_map(|row| {
                row.timestamp
                    .parse::<jiff::Timestamp>()
                    .ok()
                    .map(jiff::Timestamp::as_millisecond)
            })
            .min()
    });
    let wall_ms = start_ms.map_or(0, |start| (now_ms - start).max(0).cast_unsigned());

    SessionUsageReport {
        session_id: session_id.to_string(),
        api_ms,
        wall_ms,
        total_cost_usd: total_cost,
        models,
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
