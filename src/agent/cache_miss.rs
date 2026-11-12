//! Prompt-cache misses and their causes.
//!
//! A provider request is a cache miss when it reads less from the prompt
//! cache than the previous request on the same route (session, provider and
//! model) left there. What the previous request left is what it read plus
//! what it wrote; a provider that does not report writes is credited with
//! its prompt rounded down to the route's cache block, since it caches whole
//! blocks only. Both are amounts the provider itself reported on its own
//! increments, so any shortfall is cache the provider no longer served.
//!
//! The cause comes from the two requests' prompt-prefix hash chains (where
//! the prefix first changed), the gap between them against the provider's
//! cache lifetime, and what aither and May report happened in between.

use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;
use std::time::Duration;

use aither_agent::context::{PrefixBlock, PrefixChain, PrefixRewrites};
use aither_core::llm::{Role, Usage};

use crate::cache_lifetime::CacheLifetime;

/// What one request on a route left in the provider's prompt cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheObservation {
    /// When the request's usage arrived, Unix milliseconds.
    pub at_ms: i64,
    /// Tokens the request read from the cache.
    pub cache_read_tokens: u64,
    /// Tokens the request wrote to the cache, when the provider reports it.
    pub cache_write_tokens: Option<u64>,
    /// Tokens in the request's prompt, `0` when the provider reports none.
    pub prompt_tokens: u64,
    /// The hash chain of the request's prompt prefix.
    pub chain: PrefixChain,
}

impl CacheObservation {
    /// The observation of a request whose usage arrived at `at_ms`.
    #[must_use]
    pub fn new(at_ms: i64, usage: &Usage, chain: PrefixChain) -> Self {
        Self {
            at_ms,
            cache_read_tokens: usage.cache_read_tokens.map_or(0, u64::from),
            cache_write_tokens: usage.cache_write_tokens.map(u64::from),
            prompt_tokens: usage.prompt_tokens.map_or(0, u64::from),
            chain,
        }
    }

    /// Tokens this request left cached for the next request on its route.
    ///
    /// A provider that reports no writes cached its prompt in whole blocks
    /// of `block` tokens, so it is credited with the prompt rounded down to
    /// a block, and the next request is held to at least what this one read.
    /// With no known block size nothing is credited beyond what was read.
    #[must_use]
    pub fn cached_tokens(&self, block: Option<NonZeroU64>) -> u64 {
        self.cache_write_tokens.map_or_else(
            || {
                let whole_blocks =
                    block.map_or(0, |block| self.prompt_tokens / block.get() * block.get());
                whole_blocks.max(self.cache_read_tokens)
            },
            |written| self.cache_read_tokens + written,
        )
    }
}

/// The cache block size a route's reads imply, `read` folded into `block`.
///
/// It is the greatest common divisor of the nonzero reads observed so far. A
/// provider serves the cache in whole blocks, so every read is a multiple of its block
/// size and the divisor converges on it.
#[must_use]
pub fn observed_block(block: Option<NonZeroU64>, read: u64) -> Option<NonZeroU64> {
    let Some(read) = NonZeroU64::new(read) else {
        return block;
    };
    let Some(block) = block else {
        return Some(read);
    };
    let (mut larger, mut smaller) = (read.get().max(block.get()), read.get().min(block.get()));
    while smaller != 0 {
        (larger, smaller) = (smaller, larger % smaller);
    }
    NonZeroU64::new(larger)
}

/// What happened between the previous request on a route and this one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SincePrevious {
    /// What rewrote the prompt prefix, as aither reported it.
    pub rewrites: PrefixRewrites,
    /// This request came from a new agent: the session's agent was rebuilt
    /// or the daemon restarted.
    pub agent_rebuilt: bool,
    /// The session's previous request went to another route.
    pub model_switched: bool,
}

/// Why a request read less from the prompt cache than its route had cached.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CacheMissCause {
    /// The tool definitions changed, so nothing after them matched.
    ToolsChanged,
    /// A compaction replaced the conversation.
    Compaction,
    /// The gap since the previous request outlasted the cache lifetime.
    Expired,
    /// A new agent sent a different prefix.
    AgentRebuilt,
    /// The prefix changed with no compaction or rebuild behind it.
    PrefixChanged,
    /// The prefix was unchanged; the session had used another model since.
    ModelSwitched,
    /// The prefix was unchanged and still within its lifetime, yet the
    /// provider did not serve it.
    ProviderDropped,
}

impl CacheMissCause {
    /// The cause's stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolsChanged => "tools_changed",
            Self::Compaction => "compaction",
            Self::Expired => "expired",
            Self::AgentRebuilt => "agent_rebuilt",
            Self::PrefixChanged => "prefix_changed",
            Self::ModelSwitched => "model_switched",
            Self::ProviderDropped => "provider_dropped",
        }
    }
}

impl fmt::Display for CacheMissCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A stored cause name that names no cause.
#[derive(Debug, thiserror::Error)]
#[error("unknown cache miss cause '{0}'")]
pub struct UnknownCacheMissCause(String);

impl FromStr for CacheMissCause {
    type Err = UnknownCacheMissCause;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(match text {
            "tools_changed" => Self::ToolsChanged,
            "compaction" => Self::Compaction,
            "expired" => Self::Expired,
            "agent_rebuilt" => Self::AgentRebuilt,
            "prefix_changed" => Self::PrefixChanged,
            "model_switched" => Self::ModelSwitched,
            "provider_dropped" => Self::ProviderDropped,
            other => return Err(UnknownCacheMissCause(other.to_string())),
        })
    }
}

/// One request that read less from the prompt cache than its route had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheMiss {
    /// Why the cache missed.
    pub cause: CacheMissCause,
    /// Tokens the route had cached that this request did not read.
    pub missed_tokens: u64,
    /// Tokens this request did read from the cache.
    pub read_tokens: u64,
    /// Time since the previous request on the route.
    pub gap: Duration,
    /// The first block of the previous request's cached prefix this request
    /// changed, `None` when it repeated all of it.
    pub diverged: Option<PrefixBlock>,
    /// What happened since the previous request on the route.
    pub since: SincePrevious,
}

/// The miss `current` is against `previous`, the route's request before it,
/// or `None` when it read at least everything `previous` left cached.
///
/// `block` is the route's cache block size, which sizes what a provider that
/// reports no writes left cached.
#[must_use]
pub fn detect_cache_miss(
    previous: &CacheObservation,
    current: &CacheObservation,
    since: SincePrevious,
    lifetime: CacheLifetime,
    block: Option<NonZeroU64>,
) -> Option<CacheMiss> {
    let missed_tokens = previous
        .cached_tokens(block)
        .checked_sub(current.cache_read_tokens)
        .filter(|&missed| missed > 0)?;
    let gap = Duration::from_millis(
        u64::try_from(current.at_ms.saturating_sub(previous.at_ms)).unwrap_or(0),
    );
    let expired = lifetime.expired_after(gap);
    let diverged = current.chain.first_divergence(&previous.chain);
    let cause = match diverged {
        Some(PrefixBlock::Tools) => CacheMissCause::ToolsChanged,
        _ if since.rewrites.compacted => CacheMissCause::Compaction,
        _ if expired => CacheMissCause::Expired,
        Some(PrefixBlock::Message { .. }) if since.agent_rebuilt => CacheMissCause::AgentRebuilt,
        Some(PrefixBlock::Message { .. }) => CacheMissCause::PrefixChanged,
        None if since.model_switched => CacheMissCause::ModelSwitched,
        None => CacheMissCause::ProviderDropped,
    };
    Some(CacheMiss {
        cause,
        missed_tokens,
        read_tokens: current.cache_read_tokens,
        gap,
        diverged,
        since,
    })
}

/// The stored kind of a prefix block: `tools`, or the role of its message.
#[must_use]
pub const fn block_kind(block: PrefixBlock) -> &'static str {
    match block {
        PrefixBlock::Tools => "tools",
        PrefixBlock::Message { role, .. } => match role {
            Role::System => "system",
            Role::Configuration => "configuration",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        },
    }
}

/// The stored message index of a prefix block, `None` for the tools.
#[must_use]
pub const fn block_index(block: PrefixBlock) -> Option<usize> {
    match block {
        PrefixBlock::Tools => None,
        PrefixBlock::Message { index, .. } => Some(index),
    }
}

#[cfg(test)]
mod tests {
    use aither_core::llm::Message;
    use aither_core::llm::tool::ToolDefinition;

    use super::*;

    const HOUR_MS: i64 = 3_600_000;

    fn tool(name: &'static str) -> ToolDefinition {
        ToolDefinition::from_parts(
            name.into(),
            "runs a command".into(),
            serde_json::json!({"type": "object"}),
        )
        .expect("an object schema is valid")
    }

    fn conversation(turns: usize) -> Vec<Message> {
        let mut messages = vec![Message::system("you are may")];
        for turn in 0..turns {
            messages.push(Message::user(format!("question {turn}")));
            messages.push(Message::assistant(format!("answer {turn}")));
        }
        messages.push(Message::system("live tail"));
        messages
    }

    fn observation(at_ms: i64, read: u32, write: u32, chain: PrefixChain) -> CacheObservation {
        CacheObservation::new(
            at_ms,
            &Usage::new(read + write + 100, 10).with_cache_tokens(read, write),
            chain,
        )
    }

    fn one_hour() -> CacheLifetime {
        CacheLifetime::Limited(Duration::from_hours(1))
    }

    /// A session whose prefix only grows reads everything the previous
    /// request left cached, request after request: no miss.
    #[test]
    fn a_session_whose_prefix_never_changes_records_no_miss() {
        let tools = [tool("terminal")];
        let mut previous = observation(0, 0, 4_000, PrefixChain::of(&tools, &conversation(1)));
        for turn in 2..8_u32 {
            let current = observation(
                i64::from(turn) * 60_000,
                u32::try_from(previous.cached_tokens(None)).expect("fits"),
                4_000,
                PrefixChain::of(&tools, &conversation(turn as usize)),
            );
            assert_eq!(
                detect_cache_miss(
                    &previous,
                    &current,
                    SincePrevious::default(),
                    one_hour(),
                    None
                ),
                None,
                "turn {turn}"
            );
            previous = current;
        }
    }

    #[test]
    fn a_gap_past_the_cache_lifetime_is_an_expiry() {
        let tools = [tool("terminal")];
        let previous = observation(0, 8_000, 500, PrefixChain::of(&tools, &conversation(2)));
        let current = observation(
            HOUR_MS + 1,
            0,
            9_000,
            PrefixChain::of(&tools, &conversation(3)),
        );
        let miss = detect_cache_miss(
            &previous,
            &current,
            SincePrevious::default(),
            one_hour(),
            None,
        )
        .expect("a miss");
        assert_eq!(miss.cause, CacheMissCause::Expired);
        assert_eq!(miss.missed_tokens, 8_500);
        assert_eq!(miss.diverged, None);
        assert_eq!(miss.gap, Duration::from_millis(3_600_001));
    }

    #[test]
    fn a_permanent_cache_never_expires() {
        let tools = [tool("terminal")];
        let previous = observation(0, 8_000, 0, PrefixChain::of(&tools, &conversation(2)));
        let current = observation(
            100 * HOUR_MS,
            0,
            8_000,
            PrefixChain::of(&tools, &conversation(3)),
        );
        let miss = detect_cache_miss(
            &previous,
            &current,
            SincePrevious::default(),
            CacheLifetime::Permanent,
            None,
        )
        .expect("a miss");
        assert_eq!(miss.cause, CacheMissCause::ProviderDropped);
    }

    #[test]
    fn a_compaction_names_the_compaction() {
        let tools = [tool("terminal")];
        let previous = observation(0, 90_000, 2_000, PrefixChain::of(&tools, &conversation(30)));
        let compacted = vec![
            Message::system("you are may"),
            Message::user("handoff"),
            Message::system("live tail"),
        ];
        let current = observation(60_000, 1_000, 3_000, PrefixChain::of(&tools, &compacted));
        let since = SincePrevious {
            rewrites: PrefixRewrites {
                compacted: true,
                reassembled: false,
            },
            ..SincePrevious::default()
        };
        let miss = detect_cache_miss(&previous, &current, since, one_hour(), None).expect("a miss");
        assert_eq!(miss.cause, CacheMissCause::Compaction);
        assert_eq!(miss.missed_tokens, 91_000);
        assert_eq!(
            miss.diverged,
            Some(PrefixBlock::Message {
                index: 1,
                role: Role::User
            })
        );
    }

    #[test]
    fn a_changed_tool_set_names_the_tools() {
        let previous = observation(
            0,
            20_000,
            500,
            PrefixChain::of(&[tool("terminal")], &conversation(4)),
        );
        let current = observation(
            30_000,
            0,
            21_000,
            PrefixChain::of(&[tool("terminal"), tool("monitor")], &conversation(5)),
        );
        let miss = detect_cache_miss(
            &previous,
            &current,
            SincePrevious::default(),
            one_hour(),
            None,
        )
        .expect("a miss");
        assert_eq!(miss.cause, CacheMissCause::ToolsChanged);
        assert_eq!(miss.diverged, Some(PrefixBlock::Tools));
    }

    #[test]
    fn a_rebuilt_agent_with_a_new_system_prompt_names_the_rebuild() {
        let tools = [tool("terminal")];
        let previous = observation(0, 20_000, 500, PrefixChain::of(&tools, &conversation(4)));
        let mut rebuilt = conversation(5);
        rebuilt[0] = Message::system("you are may, rebuilt");
        let current = observation(30_000, 0, 21_000, PrefixChain::of(&tools, &rebuilt));
        let since = SincePrevious {
            agent_rebuilt: true,
            ..SincePrevious::default()
        };
        let miss = detect_cache_miss(&previous, &current, since, one_hour(), None).expect("a miss");
        assert_eq!(miss.cause, CacheMissCause::AgentRebuilt);
        let since = SincePrevious::default();
        let miss = detect_cache_miss(&previous, &current, since, one_hour(), None).expect("a miss");
        assert_eq!(miss.cause, CacheMissCause::PrefixChanged);
    }

    fn block(tokens: u64) -> Option<NonZeroU64> {
        NonZeroU64::new(tokens)
    }

    /// A request on a provider that reports no cache writes.
    fn unwritten(at_ms: i64, prompt: u32, read: u32, turns: usize) -> CacheObservation {
        let mut usage = Usage::new(prompt, 10);
        usage.cache_read_tokens = Some(read);
        CacheObservation::new(
            at_ms,
            &usage,
            PrefixChain::of(&[tool("terminal")], &conversation(turns)),
        )
    }

    /// A provider that reports no writes is held to its prompt rounded down
    /// to its block, so each of two consecutive requests that read nothing
    /// records the loss.
    #[test]
    fn consecutive_zero_read_requests_without_write_counts_each_record_a_miss() {
        let first = unwritten(0, 30_000, 0, 2);
        let second = unwritten(1_000, 31_000, 0, 3);
        let third = unwritten(2_000, 32_000, 0, 4);
        let since = SincePrevious::default();
        let blocks = block(64);
        let miss = detect_cache_miss(&first, &second, since, one_hour(), blocks).expect("a miss");
        assert_eq!(miss.missed_tokens, 29_952);
        let miss = detect_cache_miss(&second, &third, since, one_hour(), blocks).expect("a miss");
        assert_eq!(miss.missed_tokens, 30_976);
        assert_eq!(miss.cause, CacheMissCause::ProviderDropped);
    }

    /// Live session: each request read the previous prompt up to a 64-token
    /// block, which is no miss.
    #[test]
    fn a_shortfall_within_one_block_records_no_miss() {
        let live = [
            (12_009, 11_968),
            (12_244, 12_224),
            (12_451, 12_544),
            (13_842, 13_824),
            (14_140, 14_080),
        ];
        for (previous_prompt, next_read) in live {
            let previous = unwritten(0, previous_prompt, 0, 2);
            let current = unwritten(1_000, previous_prompt + 1_200, next_read, 3);
            assert_eq!(
                detect_cache_miss(
                    &previous,
                    &current,
                    SincePrevious::default(),
                    one_hour(),
                    block(64)
                ),
                None,
                "prompt {previous_prompt}"
            );
        }
    }

    /// A read a whole block or more under the credit is a real drop.
    #[test]
    fn a_real_drop_still_records_provider_dropped() {
        let previous = unwritten(0, 14_140, 0, 2);
        let current = unwritten(1_000, 14_500, 12_800, 3);
        let miss = detect_cache_miss(
            &previous,
            &current,
            SincePrevious::default(),
            one_hour(),
            block(64),
        )
        .expect("a miss");
        assert_eq!(miss.cause, CacheMissCause::ProviderDropped);
        assert_eq!(miss.missed_tokens, 14_080 - 12_800);
    }

    /// `OpenAI` documents 128-token increments: a 100-token shortfall is a
    /// miss at 64 but not at 128.
    #[test]
    fn the_block_size_sets_the_credit() {
        let previous = unwritten(0, 12_000, 0, 2);
        let current = unwritten(1_000, 12_100, 11_904, 3);
        let since = SincePrevious::default();
        assert_eq!(
            detect_cache_miss(&previous, &current, since, one_hour(), block(128)),
            None
        );
        let miss = detect_cache_miss(&previous, &current, since, one_hour(), block(64))
            .expect("a miss at 64");
        assert_eq!(miss.missed_tokens, 11_968 - 11_904);
    }

    /// With no known block nothing is credited beyond what was read.
    #[test]
    fn an_unknown_block_credits_only_the_read() {
        assert_eq!(unwritten(0, 12_000, 0, 2).cached_tokens(None), 0);
        assert_eq!(unwritten(0, 12_000, 11_968, 2).cached_tokens(None), 11_968);
    }

    #[test]
    fn the_observed_block_is_the_gcd_of_the_nonzero_reads() {
        assert_eq!(observed_block(None, 0), None);
        assert_eq!(observed_block(None, 11_968), block(11_968));
        assert_eq!(observed_block(block(11_968), 0), block(11_968));
        let mut inferred = None;
        for read in [11_968, 12_224, 12_544, 13_824, 14_080] {
            inferred = observed_block(inferred, read);
        }
        assert_eq!(inferred, block(64));
    }

    /// A request that read all of the previous prompt is no miss, with or
    /// without write counts.
    #[test]
    fn a_provider_without_write_counts_that_reads_the_prompt_records_no_miss() {
        let tools = [tool("terminal")];
        let usage = |prompt: u32, read: u32| {
            let mut usage = Usage::new(prompt, 10);
            usage.cache_read_tokens = Some(read);
            usage
        };
        let previous = CacheObservation::new(
            0,
            &usage(30_000, 0),
            PrefixChain::of(&tools, &conversation(2)),
        );
        let current = CacheObservation::new(
            1_000,
            &usage(31_000, 30_000),
            PrefixChain::of(&tools, &conversation(3)),
        );
        assert_eq!(
            detect_cache_miss(
                &previous,
                &current,
                SincePrevious::default(),
                one_hour(),
                None
            ),
            None
        );
    }

    #[test]
    fn causes_round_trip_through_their_stored_names() {
        for cause in [
            CacheMissCause::ToolsChanged,
            CacheMissCause::Compaction,
            CacheMissCause::Expired,
            CacheMissCause::AgentRebuilt,
            CacheMissCause::PrefixChanged,
            CacheMissCause::ModelSwitched,
            CacheMissCause::ProviderDropped,
        ] {
            assert_eq!(cause.as_str().parse::<CacheMissCause>().unwrap(), cause);
        }
        assert!("soon".parse::<CacheMissCause>().is_err());
    }
}
