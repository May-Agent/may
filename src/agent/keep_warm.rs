//! When an idle agent refreshes its prompt cache.
//!
//! A session whose agent waits on a detached background command lets the
//! provider's cache entry go cold when the command outlives it; the turn the
//! command's completion wakes would then read the whole prefix at full price.
//! The idle worker therefore asks [`refresh_in`] after every wake and every
//! refresh how long to wait before sending one more keep-warm request.

use std::time::Duration;

use aither_agent::CacheKeepAlive;
use aither_sandbox::{JobInfo, JobStatus};

/// How long before its entry goes cold a refresh is sent, at most.
///
/// It is aither's default lead for the refreshes it sends while a run waits, so
/// both paths land in the same window: late enough that a command ending
/// meanwhile saves the request, early enough that a slow request still
/// reaches the provider before the entry expires.
#[must_use]
pub fn refresh_margin() -> Duration {
    CacheKeepAlive::default().send_before_expiry
}

/// How long from now the next refresh is due, `None` when none should be
/// sent.
///
/// `cold_in` is how long the cache entry stays warm (`None` for an entry that
/// is cold already or never expires). A refresh buys nothing unless some
/// running job outlives the entry: it has no deadline or its remaining time
/// is beyond the cold point. When every running job ends first, the turn its
/// completion starts arrives while the cache is still warm.
///
/// The refresh is due [`refresh_margin`] before the entry goes cold, or half
/// a `lifetime` before it for a lifetime shorter than twice the margin, so a
/// short lifetime never makes the refreshes follow each other at once.
#[must_use]
pub fn refresh_in(
    cold_in: Option<Duration>,
    lifetime: Duration,
    jobs: &[JobInfo],
) -> Option<Duration> {
    let cold_in = cold_in?;
    jobs.iter()
        .filter(|job| job.status == JobStatus::Running)
        .any(|job| job.remaining().is_none_or(|remaining| remaining > cold_in))
        .then(|| cold_in.saturating_sub(refresh_margin().min(lifetime / 2)))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use aither_sandbox::{JobPlace, ScriptLanguage, TerminalMode};

    use super::*;

    fn job(status: JobStatus, timeout: Option<Duration>) -> JobInfo {
        JobInfo {
            pid: 1,
            task_id: "amber".to_string(),
            execution_key: String::new(),
            script: "sleep 9999".to_string(),
            language: ScriptLanguage::Shell,
            mode: TerminalMode::Sandboxed,
            started_at: Instant::now(),
            status,
            output_path: None,
            place: JobPlace::Host,
            timeout,
        }
    }

    const LIFETIME: Duration = Duration::from_mins(60);
    const COLD_IN: Duration = LIFETIME;
    /// Half a minute short of an hour.
    const DUE: Duration = Duration::from_secs(59 * 60 + 30);

    #[test]
    fn a_deadline_past_the_cold_point_refreshes_before_it() {
        let jobs = [job(JobStatus::Running, Some(Duration::from_hours(2)))];
        assert_eq!(refresh_in(Some(COLD_IN), LIFETIME, &jobs), Some(DUE));
        assert_eq!(
            refresh_in(Some(refresh_margin()), LIFETIME, &jobs),
            Some(Duration::ZERO),
            "the refresh that fires at the margin sends at once"
        );
    }

    #[test]
    fn deadlines_before_the_cold_point_send_nothing() {
        let jobs = [
            job(JobStatus::Running, Some(Duration::from_mins(10))),
            job(JobStatus::Running, Some(Duration::from_mins(59))),
        ];
        assert_eq!(refresh_in(Some(COLD_IN), LIFETIME, &jobs), None);
    }

    #[test]
    fn a_job_without_a_deadline_refreshes() {
        let jobs = [
            job(JobStatus::Running, Some(Duration::from_mins(5))),
            job(JobStatus::Running, None),
        ];
        assert_eq!(refresh_in(Some(COLD_IN), LIFETIME, &jobs), Some(DUE));
    }

    #[test]
    fn a_finished_job_does_not_count() {
        let jobs = [job(
            JobStatus::Completed {
                exit_code: 0,
                delivery: aither_sandbox::Delivery::Pending,
            },
            None,
        )];
        assert_eq!(refresh_in(Some(COLD_IN), LIFETIME, &jobs), None);
    }

    #[test]
    fn no_jobs_send_nothing() {
        assert_eq!(refresh_in(Some(COLD_IN), LIFETIME, &[]), None);
    }

    #[test]
    fn a_cold_or_permanent_entry_is_left_alone() {
        let jobs = [job(JobStatus::Running, None)];
        assert_eq!(refresh_in(None, LIFETIME, &jobs), None);
    }

    #[test]
    fn a_short_lifetime_is_refreshed_half_way() {
        let jobs = [job(JobStatus::Running, None)];
        let lifetime = Duration::from_secs(40);
        assert_eq!(
            refresh_in(Some(lifetime), lifetime, &jobs),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            refresh_in(Some(Duration::from_secs(20)), lifetime, &jobs),
            Some(Duration::ZERO),
            "the refresh that fires at the half sends at once"
        );
        assert_eq!(
            refresh_in(Some(Duration::from_secs(10)), lifetime, &jobs),
            Some(Duration::ZERO)
        );
    }
}
