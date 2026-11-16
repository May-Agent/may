//! The words CI findings and the `ci` command use, rendered from templates.

use std::fmt;
use std::time::Duration;

use askama::Template;

use super::logs::{TestKind, TestTiming};
use super::timing::{Exceeded, Trend};

/// A duration as `1h 02m`, `12m 03s` or `45s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span(pub Duration);

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        let (hours, minutes, seconds) = (secs / 3600, secs % 3600 / 60, secs % 60);
        if hours > 0 {
            write!(f, "{hours}h {minutes:02}m")
        } else if minutes > 0 {
            write!(f, "{minutes}m {seconds:02}s")
        } else if secs > 0 || self.0.is_zero() {
            write!(f, "{seconds}s")
        } else {
            write!(f, "{}ms", self.0.as_millis())
        }
    }
}

/// A threshold a job ran past, in words.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reason(pub Exceeded);

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Exceeded::Limit { limit } => write!(f, "past the {} limit", Span(limit)),
            Exceeded::Baseline { median, multiple } => write!(
                f,
                "past {multiple}x its baseline of {} (the median of its last successful base-branch runs)",
                Span(median)
            ),
        }
    }
}

/// A step or test and how long it took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timed {
    /// Step or test name.
    pub name: String,
    /// How long it took; for a test still running when the runner warned,
    /// the time it was known to have run.
    pub duration: Span,
}

impl Timed {
    /// A step.
    #[must_use]
    pub fn step(name: &str, duration: Duration) -> Self {
        Self {
            name: name.to_string(),
            duration: Span(duration),
        }
    }

    /// A test timing; a binary total is labelled as one.
    #[must_use]
    pub fn test(timing: &TestTiming) -> Self {
        let name = match timing.kind {
            TestKind::Test => timing.name.clone(),
            TestKind::Binary => format!("{} (all tests)", timing.name),
            TestKind::RunningOver => format!("{} (still running)", timing.name),
        };
        Self {
            name,
            duration: Span(Duration::from_millis(timing.millis)),
        }
    }
}

/// One job a finding names.
#[derive(Debug, Clone, PartialEq)]
pub struct JobView {
    /// Workflow name.
    pub workflow: String,
    /// Job name.
    pub name: String,
    /// How it ended.
    pub conclusion: String,
    /// Its duration.
    pub duration: Span,
    /// Its baseline, when it has one.
    pub baseline: Option<Span>,
    /// The thresholds it ran past.
    pub exceeded: Vec<Reason>,
    /// Its slowest steps, slowest first.
    pub steps: Vec<Timed>,
    /// Its slowest tests, slowest first.
    pub tests: Vec<Timed>,
}

/// The alert the waiting agent gets.
#[derive(Debug, Template)]
#[template(path = "ci_alert.txt", escape = "none")]
pub struct CiAlert<'a> {
    /// The PR.
    pub pr_number: i64,
    /// The abnormal jobs.
    pub jobs: &'a [JobView],
}

/// The warning the user sees.
#[derive(Debug, Template)]
#[template(path = "ci_warning.txt", escape = "none")]
pub struct CiWarning<'a> {
    /// The PR.
    pub pr_number: i64,
    /// The abnormal jobs.
    pub jobs: &'a [JobView],
}

/// One line of `ci stats`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsRow {
    /// Workflow name.
    pub workflow: String,
    /// Job name.
    pub name: String,
    /// Runs the figures are taken over.
    pub runs: usize,
    /// The newest run.
    pub latest: Span,
    /// Median over the runs.
    pub median: Span,
    /// 90th percentile over the runs.
    pub p90: Span,
    /// Where the durations are heading.
    pub trend: TrendView,
}

/// A trend in words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrendView(pub Trend);

impl fmt::Display for TrendView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            Trend::Unknown => "too few runs for a trend",
            Trend::Slower => "getting slower",
            Trend::Faster => "getting faster",
            Trend::Level => "holding level",
        })
    }
}

/// `ci stats`.
#[derive(Debug, Template)]
#[template(path = "ci_stats.txt", escape = "none")]
pub struct CiStats {
    /// One row per job.
    pub rows: Vec<StatsRow>,
}

/// `ci slow`.
#[derive(Debug, Template)]
#[template(path = "ci_slow.txt", escape = "none")]
pub struct CiSlow {
    /// `PR #4` or `run 123`.
    pub label: String,
    /// The jobs, slowest first.
    pub jobs: Vec<JobView>,
}

/// One line of `ci queue`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRow {
    /// The PR.
    pub pr_number: i64,
    /// Time in the merge queue.
    pub wait: Span,
    /// Its checks' run time.
    pub run: Span,
}

/// `ci queue`.
#[derive(Debug, Template)]
#[template(path = "ci_queue.txt", escape = "none")]
pub struct CiQueue {
    /// Newest PR first.
    pub rows: Vec<QueueRow>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abnormal() -> JobView {
        JobView {
            workflow: "CI".into(),
            name: "test (ubuntu)".into(),
            conclusion: "success".into(),
            duration: Span(Duration::from_secs(52 * 60 + 10)),
            baseline: Some(Span(Duration::from_mins(20))),
            exceeded: vec![
                Reason(Exceeded::Limit {
                    limit: Duration::from_mins(40),
                }),
                Reason(Exceeded::Baseline {
                    median: Duration::from_mins(20),
                    multiple: 2.0,
                }),
            ],
            steps: vec![Timed::step("Run tests", Duration::from_mins(48))],
            tests: vec![Timed::test(&TestTiming {
                kind: TestKind::Test,
                name: "index::rebuild".into(),
                millis: 90_000,
            })],
        }
    }

    #[test]
    fn spans_read_in_the_largest_units() {
        assert_eq!(Span(Duration::from_secs(45)).to_string(), "45s");
        assert_eq!(Span(Duration::from_secs(723)).to_string(), "12m 03s");
        assert_eq!(Span(Duration::from_mins(62)).to_string(), "1h 02m");
        assert_eq!(Span(Duration::from_millis(40)).to_string(), "40ms");
    }

    #[test]
    fn the_alert_states_the_finding_the_harm_and_the_skill() {
        let jobs = [abnormal()];
        let text = CiAlert {
            pr_number: 12,
            jobs: &jobs,
        }
        .render()
        .unwrap();
        assert!(
            text.contains(
                "PR #12's checks finished with 1 abnormally long job:\n- CI / test (ubuntu) ran 52m 10s: past the 40m 00s limit; past 2x its baseline of 20m 00s"
            ),
            "{text}"
        );
        assert!(
            text.contains("Slowest steps: Run tests (48m 00s)."),
            "{text}"
        );
        assert!(
            text.contains("Slowest tests: index::rebuild (1m 30s)."),
            "{text}"
        );
        assert!(
            text.contains("slows verification and feedback and wastes compute"),
            "{text}"
        );
        assert!(
            text.contains(
                "Act now to make the run faster, starting by reading the `ci-optimise` skill"
            ),
            "{text}"
        );
    }

    #[test]
    fn the_warning_states_the_same_finding() {
        let jobs = [abnormal()];
        let text = CiWarning {
            pr_number: 12,
            jobs: &jobs,
        }
        .render()
        .unwrap();
        assert!(
            text.starts_with("PR #12: CI / test (ubuntu) ran 52m 10s, past the 40m 00s limit"),
            "{text}"
        );
        assert!(
            text.contains("Slowest tests: index::rebuild (1m 30s)."),
            "{text}"
        );
    }

    #[test]
    fn the_command_reports_render_from_templates() {
        let stats = CiStats {
            rows: vec![StatsRow {
                workflow: "CI".into(),
                name: "test".into(),
                runs: 4,
                latest: Span(Duration::from_mins(5)),
                median: Span(Duration::from_secs(280)),
                p90: Span(Duration::from_secs(320)),
                trend: TrendView(Trend::Slower),
            }],
        }
        .render()
        .unwrap();
        assert!(
            stats.contains(
                "- CI / test: latest 5m 00s, median 4m 40s, p90 5m 20s over 4 runs; getting slower."
            ),
            "{stats}"
        );
        let queue = CiQueue {
            rows: vec![QueueRow {
                pr_number: 3,
                wait: Span(Duration::from_mins(15)),
                run: Span(Duration::from_mins(5)),
            }],
        }
        .render()
        .unwrap();
        assert!(
            queue.contains("PR #3: waited 15m 00s in the queue after a 5m 00s run."),
            "{queue}"
        );
        let slow = CiSlow {
            label: "PR #12".into(),
            jobs: vec![abnormal()],
        }
        .render()
        .unwrap();
        assert!(
            slow.contains("baseline 20m 00s; abnormal: past the 40m 00s limit"),
            "{slow}"
        );
        assert!(
            CiSlow {
                label: "run 9".into(),
                jobs: vec![]
            }
            .render()
            .unwrap()
            .contains("No jobs are recorded for run 9.")
        );
    }
}
