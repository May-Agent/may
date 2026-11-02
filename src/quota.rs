//! Upstream quota-reset parsing.
//!
//! Provider failures reach the turn loop as plain text (`AgentError::Llm`):
//! subscription and free-tier backends announce inside the error message
//! when their quota lifts again. Each matcher here recognizes one of the
//! announcement formats observed so far; a newly observed format gets a new
//! matcher.
//!
//! Collected formats:
//! - Codex/`ChatGPT`: `try again at Nov 4th, 2026 1:40 AM` — a full date in
//!   the local zone.
//! - `Claude`: `resets 2am (America/New_York)` — a wall-clock time in a
//!   named IANA zone.
//! - Free tier: `will reset in 2 minutes (at 04:11 UTC)` — a relative delay
//!   plus a parenthesized UTC instant.
//! - aither providers' `Retry-After` rendering: `(retry after 30s)`.

use std::sync::OnceLock;
use std::time::Duration;

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use regex::Regex;

/// The instant an upstream error message says its quota resets, when the
/// message declares one in a recognized format and that instant is still in
/// the future. `now` anchors relative delays and day-free wall times.
pub fn reset_at(message: &str, now: Timestamp) -> Option<Timestamp> {
    relative(message, now)
        .or_else(|| retry_after(message, now))
        .or_else(|| try_again_at(message))
        .or_else(|| resets_in_zone(message, now))
        .or_else(|| at_utc(message, now))
        .filter(|until| *until > now)
}

fn captures<'a>(
    cell: &OnceLock<Regex>,
    pattern: &str,
    text: &'a str,
) -> Option<regex::Captures<'a>> {
    cell.get_or_init(|| Regex::new(pattern).expect("quota-reset pattern must compile"))
        .captures(text)
}

/// `reset(s) in N <unit>` — the delay the message declares.
fn relative(message: &str, now: Timestamp) -> Option<Timestamp> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let caps = captures(
        &RE,
        r"(?i)\breset\w*\s+in\s+(\d+)\s*(second|minute|hour|day)s?\b",
        message,
    )?;
    let count: u64 = caps[1].parse().ok()?;
    let unit = caps[2].to_ascii_lowercase();
    let seconds = match unit.as_str() {
        "second" => count,
        "minute" => count.checked_mul(60)?,
        "hour" => count.checked_mul(3_600)?,
        "day" => count.checked_mul(86_400)?,
        _ => return None,
    };
    now.checked_add(Duration::from_secs(seconds)).ok()
}

/// aither's `Retry-After` rendering: `(retry after 30s)`.
fn retry_after(message: &str, now: Timestamp) -> Option<Timestamp> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let caps = captures(&RE, r"(?i)\bretry after\s+(\d+)\s*s\b", message)?;
    now.checked_add(Duration::from_secs(caps[1].parse().ok()?))
        .ok()
}

/// Codex/`ChatGPT`: `try again at Nov 4th, 2026 1:40 AM` — the message
/// leaves its zone unstated, so it is read in the local zone, matching how
/// the announcing CLI renders it.
fn try_again_at(message: &str) -> Option<Timestamp> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let caps = captures(
        &RE,
        r"(?i)\btry again at\s+([A-Za-z]+\s+\d{1,2})(?:st|nd|rd|th)?\s*(,\s*\d{4}\s+\d{1,2}:\d{2}\s*[AP]M)\b",
        message,
    )?;
    let date_time = format!("{}{}", &caps[1], &caps[2]);
    for format in ["%B %d, %Y %I:%M %p", "%b %d, %Y %I:%M %p"] {
        if let Ok(date_time) = DateTime::strptime(format, &date_time) {
            return TimeZone::system()
                .to_ambiguous_zoned(date_time)
                .compatible()
                .ok()
                .map(|zoned| zoned.timestamp());
        }
    }
    None
}

/// `Claude`: `resets 2am (America/New_York)` — the next occurrence of that
/// wall time in the named zone.
fn resets_in_zone(message: &str, now: Timestamp) -> Option<Timestamp> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let caps = captures(
        &RE,
        r"(?i)\bresets?\s+(?:at\s+)?(\d{1,2})(?::(\d{2}))?\s*([ap])\.?m\.?\s*\(([A-Za-z][A-Za-z0-9_+\-/]*)\)",
        message,
    )?;
    let hour12: i8 = caps[1].parse().ok()?;
    let minute: i8 = caps.get(2).map_or(0, |m| m.as_str().parse().unwrap_or(0));
    let hour = hour12 % 12 + i8::from(caps[3].eq_ignore_ascii_case("p")) * 12;
    let zone = if caps[4].eq_ignore_ascii_case("utc") {
        TimeZone::UTC
    } else {
        TimeZone::get(&caps[4]).ok()?
    };
    next_wall_time(&zone, hour, minute, now)
}

/// `at 04:11 UTC` — the next occurrence of the UTC wall time.
fn at_utc(message: &str, now: Timestamp) -> Option<Timestamp> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let caps = captures(&RE, r"(?i)\bat\s+(\d{1,2}):(\d{2})\s*UTC\b", message)?;
    next_wall_time(
        &TimeZone::UTC,
        caps[1].parse().ok()?,
        caps[2].parse().ok()?,
        now,
    )
}

/// The next instant `zone`'s wall clock reads `hour`:`minute`, today or
/// tomorrow. Ambiguous or skipped wall times resolve by jiff's `compatible`
/// strategy, matching how the announced boundary itself rolls over.
fn next_wall_time(zone: &TimeZone, hour: i8, minute: i8, now: Timestamp) -> Option<Timestamp> {
    let today = now.to_zoned(zone.clone()).date();
    for date in [today, today.tomorrow().ok()?] {
        let Ok(zoned) = zone
            .to_ambiguous_zoned(date.at(hour, minute, 0, 0))
            .compatible()
        else {
            continue;
        };
        if zoned.timestamp() > now {
            return Some(zoned.timestamp());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use jiff::civil::DateTime;
    use jiff::tz::TimeZone;

    use super::reset_at;

    fn now() -> Timestamp {
        "2026-11-01T00:00:00Z".parse().expect("timestamp")
    }

    #[test]
    fn codex_try_again_at_reads_a_local_instant() {
        let message = "You've hit your usage limit. Visit \
                       https://chatgpt.com/settings/usage to purchase more credits \
                       or try again at Nov 4th, 2026 1:40 AM.";
        let parsed = reset_at(message, now()).expect("reset instant");
        let expected = TimeZone::system()
            .to_ambiguous_zoned(
                DateTime::strptime("%b %d, %Y %I:%M %p", "Nov 4, 2026 1:40 AM").expect("date"),
            )
            .compatible()
            .expect("unambiguous local wall time")
            .timestamp();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn claude_resets_in_a_named_zone() {
        // US DST ended Nov 1, so on Nov 2, 2am in America/New_York is 07:00Z.
        let parsed = reset_at(
            "You've hit your weekly limit \u{b7} resets 2am (America/New_York)",
            "2026-11-02T00:00:00Z".parse().expect("now"),
        )
        .expect("reset instant");
        assert_eq!(parsed, "2026-11-02T07:00:00Z".parse::<Timestamp>().unwrap());
    }

    #[test]
    fn free_tier_prefers_the_relative_delay() {
        let parsed = reset_at(
            "Reached free model rate limit. Please switch to a different model. \
             Your limit will reset in 2 minutes (at 04:11 UTC). Send a message to retry",
            "2026-10-30T04:08:30Z".parse().expect("now"),
        )
        .expect("reset instant");
        assert_eq!(parsed, "2026-10-30T04:10:30Z".parse::<Timestamp>().unwrap());
    }

    #[test]
    fn utc_clock_falls_back_to_the_parenthesized_instant() {
        let parsed = reset_at(
            "will reset (at 04:11 UTC)",
            "2026-10-30T04:12:00Z".parse().expect("now"),
        )
        .expect("reset instant");
        assert_eq!(parsed, "2026-10-31T04:11:00Z".parse::<Timestamp>().unwrap());
    }

    #[test]
    fn retry_after_seconds() {
        let parsed =
            reset_at("Rate limit exceeded: x (retry after 42s)", now()).expect("reset instant");
        assert_eq!(parsed, "2026-11-01T00:00:42Z".parse::<Timestamp>().unwrap());
    }

    #[test]
    fn unrecognized_and_stale_messages_do_not_parse() {
        assert_eq!(reset_at("boom", now()), None);
        assert_eq!(
            reset_at("resets 2am", now()),
            None,
            "a zone-less wall time is ambiguous"
        );
        assert_eq!(
            reset_at("try again at Jan 4th, 2020 1:40 AM", now()),
            None,
            "a reset in the past is not held for"
        );
    }
}
