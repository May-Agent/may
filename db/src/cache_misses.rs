//! Prompt-cache observations and the misses they reveal.
//!
//! Each provider request's prompt prefix and cache usage is measured against
//! the latest request on the same route (session, provider, model), which
//! `prompt_cache_routes` keeps across agent rebuilds and daemon restarts. A
//! request that reads less than its route had cached is appended to
//! `cache_misses` with its cause.

use std::num::NonZeroU64;
use std::time::Duration;

use aither_agent::context::{PrefixChain, PromptPrefix};
use aither_core::llm::Usage;

use super::{
    ChatDb, ChatDbError,
    entity::{cache_misses, prompt_cache_routes},
    query, storage_error, to_i64_from_u64,
};
use may_protocol::cache_lifetime::CacheLifetime;
use may_protocol::cache_miss::{
    CacheMiss, CacheObservation, SincePrevious, block_index, block_kind, detect_cache_miss,
    observed_block,
};

/// One provider request's prompt prefix and the usage it reported.
#[derive(Debug)]
pub struct PromptCacheRequest<'a> {
    /// Session the request belongs to.
    pub session_id: &'a str,
    /// Provider that served the request.
    pub provider_id: &'a str,
    /// Model the request targeted.
    pub model: &'a str,
    /// When the request's usage arrived.
    pub at: jiff::Timestamp,
    /// What the provider reported for the request.
    pub usage: &'a Usage,
    /// The request's prompt prefix, as the agent reported it.
    pub prefix: PromptPrefix,
    /// How long the provider keeps the conversation's cache warm.
    pub lifetime: CacheLifetime,
    /// The provider's published cache block size; `None` where it publishes
    /// none and the route's reads imply it.
    pub published_block: Option<NonZeroU64>,
}

impl ChatDb {
    /// Measure a request against the latest one on its route, record the
    /// miss when it read less than the route had cached, and make it the
    /// route's latest request.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or when the route's stored
    /// prefix chain cannot be decoded.
    pub async fn observe_prompt_cache(
        &self,
        request: PromptCacheRequest<'_>,
    ) -> Result<Option<CacheMiss>, ChatDbError> {
        let routes = query!(
            self.db,
            "SELECT * FROM prompt_cache_routes WHERE session_id = ?",
            request.session_id
        )
        .fetch_all::<prompt_cache_routes::Model>()
        .await?;
        let is_this_route = |route: &prompt_cache_routes::Model| {
            route.provider_id == request.provider_id && route.model == request.model
        };
        let latest = routes.iter().max_by_key(|route| route.observed_at_ms);
        let current = CacheObservation::new(
            request.at.as_millisecond(),
            request.usage,
            request.prefix.chain,
        );
        let this_route = routes.iter().find(|route| is_this_route(route));
        let observed = this_route
            .and_then(|route| route.cache_block_tokens)
            .and_then(|tokens| u64::try_from(tokens).ok())
            .and_then(NonZeroU64::new);
        let miss = match this_route {
            Some(route) => {
                let previous = stored_observation(route)?;
                let since = SincePrevious {
                    rewrites: request.prefix.rewrites,
                    agent_rebuilt: request.prefix.first_request,
                    model_switched: latest.is_some_and(|latest| !is_this_route(latest)),
                };
                let block = request.published_block.or(observed);
                detect_cache_miss(&previous, &current, since, request.lifetime, block)
            }
            None => None,
        };
        let inferred = observed_block(observed, current.cache_read_tokens);

        let chain = rmp_serde::to_vec(&current.chain)
            .map_err(|error| storage_error(format!("failed to encode prefix chain: {error}")))?;
        let mut tx = self.db.begin().await?;
        if let Some(miss) = &miss {
            let diverged_index = miss
                .diverged
                .and_then(block_index)
                .map(|index| to_i64_from_u64(index as u64, "diverged_index"))
                .transpose()?;
            query!(
                tx,
                super::db_sql::CACHE_MISS_INSERT,
                request.session_id,
                request.at.to_string(),
                request.model,
                request.provider_id,
                miss.cause.as_str(),
                to_i64_from_u64(miss.missed_tokens, "missed_tokens")?,
                to_i64_from_u64(miss.read_tokens, "read_tokens")?,
                to_i64_from_u64(
                    u64::try_from(miss.gap.as_millis()).unwrap_or(u64::MAX),
                    "gap_ms"
                )?,
                miss.diverged.map(block_kind),
                diverged_index,
                miss.since.rewrites.compacted,
                miss.since.rewrites.reassembled,
                miss.since.agent_rebuilt,
                miss.since.model_switched,
            )
            .execute()
            .await?;
        }
        query!(
            tx,
            super::db_sql::PROMPT_CACHE_ROUTE_UPSERT,
            request.session_id,
            request.provider_id,
            request.model,
            current.at_ms,
            to_i64_from_u64(current.cache_read_tokens, "cache_read_tokens")?,
            current
                .cache_write_tokens
                .map(|tokens| to_i64_from_u64(tokens, "cache_write_tokens"))
                .transpose()?,
            to_i64_from_u64(current.prompt_tokens, "prompt_tokens")?,
            chain.as_slice(),
            inferred
                .map(|block| to_i64_from_u64(block.get(), "cache_block_tokens"))
                .transpose()?,
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(miss)
    }

    /// Whether the prompt cache of `session_id` has gone cold at `now`: its
    /// latest provider request is older than that provider's cache lifetime,
    /// `lifetime_of` giving the lifetime for a provider id. A session that
    /// has made no request has no cache to be cold.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn prompt_cache_is_cold(
        &self,
        session_id: &str,
        now: jiff::Timestamp,
        lifetime_of: impl FnOnce(&str) -> CacheLifetime,
    ) -> Result<bool, ChatDbError> {
        let routes = query!(
            self.db,
            "SELECT * FROM prompt_cache_routes WHERE session_id = ?",
            session_id
        )
        .fetch_all::<prompt_cache_routes::Model>()
        .await?;
        let Some(latest) = routes.iter().max_by_key(|route| route.observed_at_ms) else {
            return Ok(false);
        };
        let gap = Duration::from_millis(
            u64::try_from(now.as_millisecond().saturating_sub(latest.observed_at_ms)).unwrap_or(0),
        );
        Ok(lifetime_of(&latest.provider_id).expired_after(gap))
    }

    /// Every recorded cache miss of one session, oldest first.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn cache_misses_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<cache_misses::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM cache_misses WHERE session_id = ? ORDER BY timestamp ASC, id ASC",
            session_id
        )
        .fetch_all::<cache_misses::Model>()
        .await?)
    }
}

/// The observation a route's stored row describes.
fn stored_observation(route: &prompt_cache_routes::Model) -> Result<CacheObservation, ChatDbError> {
    let chain = rmp_serde::from_slice::<PrefixChain>(&route.chain).map_err(|error| {
        storage_error(format!(
            "failed to decode the prefix chain of session {} on {}/{}: {error}",
            route.session_id, route.provider_id, route.model
        ))
    })?;
    let tokens = |value: i64, field: &str| {
        u64::try_from(value).map_err(|error| {
            storage_error(format!("negative {field} in prompt_cache_routes: {error}"))
        })
    };
    Ok(CacheObservation {
        at_ms: route.observed_at_ms,
        cache_read_tokens: tokens(route.cache_read_tokens, "cache_read_tokens")?,
        cache_write_tokens: route
            .cache_write_tokens
            .map(|value| tokens(value, "cache_write_tokens"))
            .transpose()?,
        prompt_tokens: tokens(route.prompt_tokens, "prompt_tokens")?,
        chain,
    })
}
