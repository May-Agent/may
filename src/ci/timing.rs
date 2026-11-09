//! Job durations: the baseline a job is compared with, the statistics `ci
//! stats` reports, and the two thresholds that make a job abnormally long.

use std::time::Duration;

use crate::config::CiConfig;

/// The most recent successful base-branch runs a baseline is taken over.
pub const BASELINE_RUNS: usize = 10;

/// The two thresholds, as read from `[ci]` at the moment they are used.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// Any job running longer than this is abnormal.
    pub max_job: Duration,
    /// A job running longer than this multiple of its baseline is abnormal.
    pub baseline_multiple: f64,
}

impl From<CiConfig> for Limits {
    fn from(config: CiConfig) -> Self {
        Self {
            max_job: Duration::from_secs(config.max_job_minutes * 60),
            baseline_multiple: config.baseline_multiple,
        }
    }
}

/// Which threshold a job ran past.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Exceeded {
    /// Past the absolute limit.
    Limit {
        /// The limit.
        limit: Duration,
    },
    /// Past a multiple of the job's baseline.
    Baseline {
        /// The baseline median.
        median: Duration,
        /// The multiple that was exceeded.
        multiple: f64,
    },
}

/// The thresholds `duration` runs past; empty for a normal job. A job with no
/// baseline is judged by the absolute limit alone.
#[must_use]
pub fn judge(duration: Duration, baseline: Option<Duration>, limits: Limits) -> Vec<Exceeded> {
    let mut exceeded = Vec::new();
    if duration > limits.max_job {
        exceeded.push(Exceeded::Limit {
            limit: limits.max_job,
        });
    }
    if let Some(median) = baseline
        && duration.as_secs_f64() > median.as_secs_f64() * limits.baseline_multiple
    {
        exceeded.push(Exceeded::Baseline {
            median,
            multiple: limits.baseline_multiple,
        });
    }
    exceeded
}

/// The median of `values`, the mean of the middle two when there is an even
/// number of them.
#[must_use]
pub fn median(values: &[u64]) -> Option<u64> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    match sorted.len() {
        0 => None,
        n if n % 2 == 1 => Some(sorted[middle]),
        _ => Some(u64::midpoint(sorted[middle - 1], sorted[middle])),
    }
}

/// The nearest-rank 90th percentile of `values`.
#[must_use]
pub fn p90(values: &[u64]) -> Option<u64> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let last = sorted.len().checked_sub(1)?;
    // Nearest rank: ceil(0.9 * n) as a 1-based index.
    let rank = (sorted.len() * 9).div_ceil(10).max(1);
    sorted.get(rank - 1).or_else(|| sorted.get(last)).copied()
}

/// Whether durations, oldest first, are getting slower, faster or staying
/// level: the median of the newer half against the median of the older half,
/// level within 10%.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trend {
    /// Too few runs to tell.
    Unknown,
    /// The newer runs are at least 10% slower.
    Slower,
    /// The newer runs are at least 10% faster.
    Faster,
    /// Within 10%.
    Level,
}

/// The trend of `oldest_first`.
#[must_use]
pub fn trend(oldest_first: &[u64]) -> Trend {
    if oldest_first.len() < 4 {
        return Trend::Unknown;
    }
    let (older, newer) = oldest_first.split_at(oldest_first.len() / 2);
    let (Some(older), Some(newer)) = (median(older), median(newer)) else {
        return Trend::Unknown;
    };
    if newer * 10 >= older * 11 {
        Trend::Slower
    } else if newer * 11 <= older * 10 {
        Trend::Faster
    } else {
        Trend::Level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = 60;

    fn limits() -> Limits {
        CiConfig::default().into()
    }

    #[test]
    fn a_job_past_the_absolute_limit_is_abnormal_without_a_baseline() {
        let found = judge(Duration::from_secs(41 * MINUTE), None, limits());
        assert_eq!(
            found,
            [Exceeded::Limit {
                limit: Duration::from_secs(40 * MINUTE)
            }]
        );
        assert_eq!(judge(Duration::from_secs(40 * MINUTE), None, limits()), []);
    }

    #[test]
    fn a_job_past_a_multiple_of_its_baseline_is_abnormal_under_the_limit() {
        let baseline = Some(Duration::from_secs(5 * MINUTE));
        let found = judge(Duration::from_secs(11 * MINUTE), baseline, limits());
        assert_eq!(
            found,
            [Exceeded::Baseline {
                median: Duration::from_secs(5 * MINUTE),
                multiple: 2.0
            }]
        );
        assert_eq!(
            judge(Duration::from_secs(10 * MINUTE), baseline, limits()),
            []
        );
    }

    #[test]
    fn a_job_past_both_thresholds_reports_both() {
        let baseline = Some(Duration::from_secs(10 * MINUTE));
        let found = judge(Duration::from_secs(45 * MINUTE), baseline, limits());
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn the_thresholds_follow_the_config() {
        let tight = Limits::from(CiConfig {
            max_job_minutes: 1,
            baseline_multiple: 1.5,
        });
        assert_eq!(judge(Duration::from_secs(90), None, tight).len(), 1);
        let baseline = Some(Duration::from_secs(30));
        assert_eq!(judge(Duration::from_secs(50), baseline, tight).len(), 1);
    }

    #[test]
    fn median_and_p90_of_a_run_history() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[5, 1, 3]), Some(3));
        assert_eq!(median(&[4, 1, 3, 2]), Some(2));
        let runs: Vec<u64> = (1..=10).collect();
        assert_eq!(p90(&runs), Some(9));
        assert_eq!(p90(&[7]), Some(7));
    }

    #[test]
    fn trend_compares_the_newer_half_with_the_older_half() {
        assert_eq!(trend(&[10, 10, 10]), Trend::Unknown);
        assert_eq!(trend(&[10, 10, 20, 20]), Trend::Slower);
        assert_eq!(trend(&[20, 20, 10, 10]), Trend::Faster);
        assert_eq!(trend(&[10, 10, 10, 10]), Trend::Level);
    }
}
