//! Prompt-cache observations and the misses they reveal.
//!
//! Each provider request's prompt prefix and cache usage is measured against
//! the latest request on the same route (session, provider, model), which
//! `prompt_cache_routes` keeps across agent rebuilds and daemon restarts. A
//! request that reads less than its route had cached is appended to
//! `cache_misses` with its cause.

use aither_agent::context::{PrefixChain, PromptPrefix};
use aither_core::llm::Usage;

use super::{
    ChatDb, ChatDbError,
    entity::{cache_misses, prompt_cache_routes},
    query, storage_error, to_i64_from_u64,
};
use crate::agent::cache_miss::{
    CacheMiss, CacheObservation, SincePrevious, block_index, block_kind, detect_cache_miss,
};
use crate::cache_lifetime::CacheLifetime;

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
        let miss = match routes.iter().find(|route| is_this_route(route)) {
            Some(route) => {
                let previous = stored_observation(route)?;
                let since = SincePrevious {
                    rewrites: request.prefix.rewrites,
                    agent_rebuilt: request.prefix.first_request,
                    model_switched: latest.is_some_and(|latest| !is_this_route(latest)),
                };
                detect_cache_miss(&previous, &current, since, request.lifetime)
            }
            None => None,
        };

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
                "INSERT INTO cache_misses (session_id, timestamp, model, provider_id, cause, \
                 missed_tokens, read_tokens, gap_ms, diverged_kind, diverged_index, compacted, \
                 reassembled, agent_rebuilt, model_switched) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
            "INSERT INTO prompt_cache_routes (session_id, provider_id, model, observed_at_ms, \
             cache_read_tokens, cache_write_tokens, chain) VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (session_id, provider_id, model) DO UPDATE SET \
             observed_at_ms = excluded.observed_at_ms, \
             cache_read_tokens = excluded.cache_read_tokens, \
             cache_write_tokens = excluded.cache_write_tokens, chain = excluded.chain",
            request.session_id,
            request.provider_id,
            request.model,
            current.at_ms,
            to_i64_from_u64(current.cache_read_tokens, "cache_read_tokens")?,
            current
                .cache_write_tokens
                .map(|tokens| to_i64_from_u64(tokens, "cache_write_tokens"))
                .transpose()?,
            chain.as_slice(),
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(miss)
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
        chain,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aither_agent::context::PrefixRewrites;
    use aither_core::llm::Message;
    use aither_core::llm::tool::ToolDefinition;
    use tempfile::tempdir;

    use super::*;
    use crate::agent::cache_miss::CacheMissCause;
    use crate::agent::{SessionUsageReport, assemble_usage_report};
    use crate::db::SessionIntervals;

    const SESSION: &str = "session-cache";
    const START_MS: i64 = 1_700_000_000_000;

    fn tool(name: &'static str) -> ToolDefinition {
        ToolDefinition::from_parts(
            name.into(),
            "runs a command".into(),
            serde_json::json!({"type": "object"}),
        )
        .expect("an object schema is valid")
    }

    /// A session's requests on one route, as the turn runner reports them.
    struct Session {
        db: ChatDb,
        tools: Vec<ToolDefinition>,
        messages: Vec<Message>,
        at_ms: i64,
        cached: u32,
        rewrites: PrefixRewrites,
        first_request: bool,
        model: &'static str,
    }

    impl Session {
        async fn open(dir: &tempfile::TempDir) -> Self {
            let db = ChatDb::open(&dir.path().join("history.db"))
                .await
                .expect("db opens");
            db.create_session(SESSION, Some("Cache"), Some("opus"))
                .await
                .expect("session is created");
            Self {
                db,
                tools: vec![tool("terminal")],
                messages: vec![Message::system("you are may"), Message::user("start")],
                at_ms: START_MS,
                cached: 0,
                rewrites: PrefixRewrites::default(),
                first_request: true,
                model: "opus",
            }
        }

        /// Sends one request `after` the previous one that reads `read`
        /// tokens from the cache and writes 1000 more.
        async fn request(&mut self, after: Duration, read: u32) -> Option<CacheMiss> {
            self.at_ms += i64::try_from(after.as_millis()).expect("fits");
            let mut messages = self.messages.clone();
            messages.push(Message::system("live tail"));
            let usage = Usage::new(read + 1_000 + 200, 50).with_cache_tokens(read, 1_000);
            let miss = self
                .db
                .observe_prompt_cache(PromptCacheRequest {
                    session_id: SESSION,
                    provider_id: "anthropic",
                    model: self.model,
                    at: jiff::Timestamp::from_millisecond(self.at_ms).expect("in range"),
                    usage: &usage,
                    prefix: PromptPrefix {
                        chain: PrefixChain::of(&self.tools, &messages),
                        rewrites: std::mem::take(&mut self.rewrites),
                        first_request: std::mem::replace(&mut self.first_request, false),
                    },
                    lifetime: CacheLifetime::Limited(Duration::from_mins(5)),
                })
                .await
                .expect("the request is observed");
            self.cached = read + 1_000;
            self.messages.push(Message::assistant("done"));
            self.messages.push(Message::user("next"));
            miss
        }

        /// A request that reads everything the route had cached.
        async fn warm_request(&mut self) -> Option<CacheMiss> {
            self.request(Duration::from_secs(30), self.cached).await
        }

        async fn report(&self) -> SessionUsageReport {
            let misses = self
                .db
                .cache_misses_for_session(SESSION)
                .await
                .expect("misses load");
            assemble_usage_report(
                SESSION,
                &[],
                &misses,
                &SessionIntervals::default(),
                Some(START_MS),
                self.at_ms,
            )
        }
    }

    /// Each cause produces exactly one record, and the warm requests around
    /// it produce none.
    async fn one_miss_named(session: &mut Session, miss: Option<CacheMiss>, cause: CacheMissCause) {
        assert_eq!(miss.map(|miss| miss.cause), Some(cause));
        assert_eq!(
            session.warm_request().await,
            None,
            "the next request is warm"
        );
        let report = session.report().await;
        assert_eq!(report.cache_miss_causes.len(), 1, "{report:?}");
        assert_eq!(report.cache_miss_causes[0].cause, cause);
        assert_eq!(report.cache_miss_causes[0].misses, 1);
        assert_eq!(report.recent_cache_misses.len(), 1);
        assert_eq!(report.recent_cache_misses[0].cause, cause);
        assert_eq!(report.models[0].cache_misses, 1);
    }

    #[tokio::test]
    async fn a_session_whose_prefix_never_changes_records_no_miss() {
        let dir = tempdir().expect("temp dir");
        let mut session = Session::open(&dir).await;
        for _ in 0..10 {
            assert_eq!(session.warm_request().await, None);
        }
        let report = session.report().await;
        assert!(report.cache_miss_causes.is_empty());
        assert!(report.recent_cache_misses.is_empty());
    }

    #[tokio::test]
    async fn an_expired_cache_lifetime_records_one_expiry() {
        let dir = tempdir().expect("temp dir");
        let mut session = Session::open(&dir).await;
        session.warm_request().await;
        session.warm_request().await;
        let miss = session.request(Duration::from_mins(6), 0).await;
        assert_eq!(miss.as_ref().map(|miss| miss.missed_tokens), Some(2_000));
        one_miss_named(&mut session, miss, CacheMissCause::Expired).await;
    }

    #[tokio::test]
    async fn a_compaction_records_one_compaction() {
        let dir = tempdir().expect("temp dir");
        let mut session = Session::open(&dir).await;
        for _ in 0..4 {
            session.warm_request().await;
        }
        session.messages = vec![Message::system("you are may"), Message::user("handoff")];
        session.rewrites.compacted = true;
        let miss = session.request(Duration::from_secs(30), 300).await;
        one_miss_named(&mut session, miss, CacheMissCause::Compaction).await;
    }

    #[tokio::test]
    async fn a_changed_tool_set_records_one_tool_change() {
        let dir = tempdir().expect("temp dir");
        let mut session = Session::open(&dir).await;
        session.warm_request().await;
        session.warm_request().await;
        session.tools.push(tool("monitor"));
        let miss = session.request(Duration::from_secs(30), 0).await;
        one_miss_named(&mut session, miss, CacheMissCause::ToolsChanged).await;
    }

    /// The route outlives the agent: a rebuilt agent's changed system
    /// prompt is measured against what its predecessor cached.
    #[tokio::test]
    async fn a_rebuilt_agent_is_measured_against_its_predecessor() {
        let dir = tempdir().expect("temp dir");
        let mut session = Session::open(&dir).await;
        session.warm_request().await;
        session.warm_request().await;
        session.first_request = true;
        session.messages[0] = Message::system("you are may, rebuilt");
        let miss = session.request(Duration::from_secs(30), 0).await;
        let stored = session
            .db
            .cache_misses_for_session(SESSION)
            .await
            .expect("misses load");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].diverged_kind.as_deref(), Some("system"));
        assert_eq!(stored[0].diverged_index, Some(0));
        assert!(stored[0].agent_rebuilt);
        one_miss_named(&mut session, miss, CacheMissCause::AgentRebuilt).await;
    }

    /// Another model's request in between is no miss on either route, and
    /// is named when the first route later misses.
    #[tokio::test]
    async fn a_model_switch_is_carried_by_the_next_miss() {
        let dir = tempdir().expect("temp dir");
        let mut session = Session::open(&dir).await;
        session.warm_request().await;
        session.warm_request().await;
        let cached = session.cached;
        session.model = "sonnet";
        assert_eq!(session.request(Duration::from_secs(10), 0).await, None);
        session.model = "opus";
        session.cached = cached;
        let miss = session.request(Duration::from_secs(10), 0).await;
        assert!(miss.as_ref().is_some_and(|miss| miss.since.model_switched));
        one_miss_named(&mut session, miss, CacheMissCause::ModelSwitched).await;
    }
}
