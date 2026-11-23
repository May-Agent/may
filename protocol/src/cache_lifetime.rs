//! How long a provider keeps the main conversation's prompt cache warm.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Config spelling of [`CacheLifetime::Permanent`].
const PERMANENT: &str = "permanent";

/// Default cache lifetime of a provider: one hour.
const DEFAULT_LIFETIME: Duration = Duration::from_hours(1);

/// The default cache lifetime of a provider's main conversation.
///
/// Written in `config.toml` as a duration (`"1h"`, `"5m"`, `"90s"`) or the
/// word `permanent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheLifetime {
    /// The cache expires after this long without a hit.
    Limited(Duration),
    /// The provider never expires the cache.
    Permanent,
}

impl Default for CacheLifetime {
    fn default() -> Self {
        Self::Limited(DEFAULT_LIFETIME)
    }
}

impl CacheLifetime {
    /// Whether this is the one-hour default, which a config file omits.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Whether a cache last touched `gap` ago has expired.
    #[must_use]
    pub fn expired_after(&self, gap: Duration) -> bool {
        match self {
            Self::Limited(lifetime) => gap >= *lifetime,
            Self::Permanent => false,
        }
    }
}

impl From<CacheLifetime> for aither_agent::context::CacheLifetime {
    fn from(value: CacheLifetime) -> Self {
        match value {
            CacheLifetime::Limited(lifetime) => Self::Limited(lifetime),
            CacheLifetime::Permanent => Self::Permanent,
        }
    }
}

/// The Claude breakpoint retention whose tier covers `lifetime`: the
/// five-minute tier for a lifetime it alone cannot cover, otherwise the
/// one-hour tier, which is the longest Claude offers.
#[must_use]
pub const fn claude_cache_ttl(
    lifetime: aither_agent::context::CacheLifetime,
) -> aither_core::llm::model::ClaudePromptCacheTtl {
    use aither_core::llm::model::ClaudePromptCacheTtl;
    match lifetime {
        aither_agent::context::CacheLifetime::Limited(lifetime) if lifetime.as_secs() <= 300 => {
            ClaudePromptCacheTtl::FiveMinutes
        }
        _ => ClaudePromptCacheTtl::OneHour,
    }
}

impl CacheLifetime {
    /// The Claude breakpoint retention covering this lifetime.
    #[must_use]
    pub fn claude_ttl(&self) -> aither_core::llm::model::ClaudePromptCacheTtl {
        claude_cache_ttl((*self).into())
    }
}

/// A cache lifetime text that is neither `permanent` nor a duration.
#[derive(Debug, thiserror::Error)]
#[error("invalid cache lifetime '{text}': expected a duration such as '1h' or 'permanent'")]
pub struct CacheLifetimeParseError {
    text: String,
}

impl FromStr for CacheLifetime {
    type Err = CacheLifetimeParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if text.eq_ignore_ascii_case(PERMANENT) {
            return Ok(Self::Permanent);
        }
        humantime::parse_duration(text)
            .map(Self::Limited)
            .map_err(|_| CacheLifetimeParseError {
                text: text.to_string(),
            })
    }
}

impl fmt::Display for CacheLifetime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limited(lifetime) => humantime::format_duration(*lifetime).fmt(formatter),
            Self::Permanent => formatter.write_str(PERMANENT),
        }
    }
}

impl Serialize for CacheLifetime {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CacheLifetime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations_and_permanent() {
        assert_eq!(
            "30m".parse::<CacheLifetime>().unwrap(),
            CacheLifetime::Limited(Duration::from_mins(30))
        );
        assert_eq!(
            "permanent".parse::<CacheLifetime>().unwrap(),
            CacheLifetime::Permanent
        );
        assert!("soon".parse::<CacheLifetime>().is_err());
    }

    #[test]
    fn display_round_trips_through_parse() {
        for lifetime in [
            CacheLifetime::default(),
            CacheLifetime::Permanent,
            CacheLifetime::Limited(Duration::from_mins(90)),
        ] {
            assert_eq!(
                lifetime.to_string().parse::<CacheLifetime>().unwrap(),
                lifetime
            );
        }
    }

    #[test]
    fn a_claude_ttl_is_the_tier_covering_the_lifetime() {
        use aither_agent::context::CacheLifetime as AitherLifetime;
        use aither_core::llm::model::ClaudePromptCacheTtl;

        for (lifetime, ttl) in [
            (
                AitherLifetime::Limited(Duration::from_mins(5)),
                ClaudePromptCacheTtl::FiveMinutes,
            ),
            (
                AitherLifetime::Limited(Duration::from_secs(60)),
                ClaudePromptCacheTtl::FiveMinutes,
            ),
            (
                AitherLifetime::Limited(Duration::from_mins(30)),
                ClaudePromptCacheTtl::OneHour,
            ),
            (
                AitherLifetime::Limited(Duration::from_hours(24)),
                ClaudePromptCacheTtl::OneHour,
            ),
            (AitherLifetime::Permanent, ClaudePromptCacheTtl::OneHour),
        ] {
            assert_eq!(claude_cache_ttl(lifetime), ttl, "{lifetime:?}");
        }
        assert_eq!(
            CacheLifetime::Limited(Duration::from_mins(5)).claude_ttl(),
            ClaudePromptCacheTtl::FiveMinutes
        );
    }
}
