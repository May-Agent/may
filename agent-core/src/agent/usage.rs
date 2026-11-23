//! Per-session provider-usage aggregation for `/usage`: one row per model
//! plus session-level API time, wall-clock duration and cost totals, and the
//! session's prompt-cache misses by cause.

use super::cache_miss::CacheMissCause;
use crate::db::SessionIntervals;
use crate::db::entity::{cache_misses, request_logs};
use may_protocol::cache_miss::{CacheMissCauseRow, CacheMissRow};
use may_protocol::session_usage::{ModelUsageRow, SessionUsageReport};
use may_protocol::wall_clock::{Interval, IntervalSet, WallClockSplit};

/// How many of the newest cache misses `/usage` lists one by one.
const RECENT_CACHE_MISSES: usize = 10;

/// A stored miss's cause; only May writes the column.
/// The row form a stored `cache_misses` row reports.
fn cache_miss_row_from(row: &cache_misses::Model) -> CacheMissRow {
    CacheMissRow {
        timestamp: row.timestamp.clone(),
        model: row.model.clone(),
        cause: stored_cause(row),
        missed_tokens: row.missed_tokens.max(0).cast_unsigned(),
        gap_ms: row.gap_ms.max(0).cast_unsigned(),
        diverged_kind: row.diverged_kind.clone(),
        diverged_index: row.diverged_index.map(|index| index.max(0).cast_unsigned()),
    }
}

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
        keep_warm_requests: 0,
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
        entry.keep_warm_requests += u64::from(row.keep_warm);
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
        .map(cache_miss_row_from)
        .collect();

    let mut api_ms = 0;
    let mut total_cost: Option<f64> = None;
    for entry in &mut models {
        api_ms += entry.api_ms;
        entry.cache_hit_rate = if entry.prompt_tokens > 0 {
            entry.cache_read_tokens as f64 / entry.prompt_tokens as f64
        } else {
            0.0
        };
        entry.cost_usd = cost_usd(entry);
        if let Some(cost) = entry.cost_usd {
            total_cost = Some(total_cost.unwrap_or(0.0) + cost);
        }
    }
    models.sort_by_key(|model| std::cmp::Reverse(model.prompt_tokens));

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
    // The prompt counts every input token, cached ones included; a cached
    // token is billed at its cache rate when the model has one, otherwise as
    // plain input.
    let mut uncached = row.prompt_tokens;
    let mut cost = (row.completion_tokens as f64) * pricing.output_per_token();
    if let Some(rate) = pricing.cache_read_per_token() {
        uncached = uncached.saturating_sub(row.cache_read_tokens);
        cost = (row.cache_read_tokens as f64).mul_add(rate, cost);
    }
    if let Some(rate) = pricing.cache_write_per_token() {
        uncached = uncached.saturating_sub(row.cache_write_tokens);
        cost = (row.cache_write_tokens as f64).mul_add(rate, cost);
    }
    Some((uncached as f64).mul_add(pricing.input_per_token(), cost))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = "gpt-5";

    fn log(prompt: i32, cache_read: i32, cache_write: i32) -> request_logs::Model {
        request_logs::Model {
            id: 0,
            session_id: "s".to_string(),
            timestamp: "2026-11-06T00:00:00Z".to_string(),
            model: MODEL.to_string(),
            provider_id: "openai".to_string(),
            prompt_tokens: Some(prompt),
            completion_tokens: Some(0),
            total_tokens: Some(prompt),
            cache_read_tokens: Some(cache_read),
            cache_write_tokens: Some(cache_write),
            context_window: None,
            elapsed_ms: 1,
            ok: true,
            error: None,
            keep_warm: false,
        }
    }

    fn report(rows: &[request_logs::Model]) -> ModelUsageRow {
        let mut report =
            assemble_usage_report("s", rows, &[], &SessionIntervals::default(), None, 1_000);
        report.models.remove(0)
    }

    #[test]
    fn hit_rate_is_cache_read_over_the_whole_prompt() {
        let row = report(&[log(92_946, 82_176, 0)]);
        assert!((row.cache_hit_rate - 82_176.0 / 92_946.0).abs() < 1e-9);
    }

    #[test]
    fn cost_bills_the_cached_part_of_the_prompt_once() {
        let pricing = aither_models::lookup(MODEL)
            .map(aither_models::ModelEntry::pricing)
            .expect("the catalog prices the test model");
        let cache_rate = pricing
            .cache_read_per_token()
            .expect("the test model has a cache-read rate");
        let row = report(&[log(1_000, 800, 0)]);
        let expected = 800.0_f64.mul_add(cache_rate, 200.0 * pricing.input_per_token());
        assert!((row.cost_usd.expect("priced") - expected).abs() < 1e-12);
    }
}
