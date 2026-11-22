//! A PR's checks as the agent and the sidebars read them: one state per
//! check, the rollup, the failure excerpts `pr show` prints and the alerts
//! the daemon raises when the rollup changes.

pub use may_protocol::pr_checks::CheckView;
pub use may_protocol::pr_checks::*;

use std::collections::HashMap;

use askama::Template;
use futures_util::{StreamExt, stream};
#[cfg(test)]
use jiff::Timestamp;

use super::merge_watch::{CheckConclusion, CheckContext, RunStatus, Snapshot, StatusState};
use crate::ci::github::Actions;

/// Error lines kept per failed check.
const LOG_LINES: usize = 12;

/// Failed checks whose logs are fetched at once.
const LOG_FETCH_CONCURRENCY: usize = 4;

async fn excerpt(actions: &impl Actions, job: u64) -> LogExcerpt {
    match actions.log(job).await {
        Err(error) => LogExcerpt::Unavailable(error.to_string()),
        Ok(log) => match crate::ci::logs::error_lines(&log, LOG_LINES) {
            lines if lines.is_empty() => LogExcerpt::NoErrorLines,
            lines => LogExcerpt::Lines(lines),
        },
    }
}

/// The rollup line and failures of one read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadChecks {
    /// Checks the rollup counted.
    pub total: u32,
    /// Checks that have concluded.
    pub finished: u32,
    /// Checks that passed.
    pub passed: u32,
    /// Checks still running or waiting.
    pub pending: u32,
    /// Checks that failed.
    pub failed: u32,
    /// The failed checks, with their details.
    pub failures: Vec<FailureDetail>,
}

impl ReadChecks {
    /// The read of `report` with `failures` attached.
    #[must_use]
    pub fn new(report: &ChecksReport, failures: Vec<FailureDetail>) -> Self {
        let summary = report.summary();
        Self {
            total: summary.passed + summary.pending + summary.failed,
            finished: summary.passed + summary.failed,
            passed: summary.passed,
            pending: summary.pending,
            failed: summary.failed,
            failures,
        }
    }
}

/// The checks a `snapshot` carries, as a report.
#[must_use]
pub fn checks_report_of(snapshot: &Snapshot) -> ChecksReport {
    let mut checks: Vec<CheckView> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for context in &snapshot.checks {
        let view = check_view_of(context);
        if let Some(&index) = at.get(&view.name) {
            checks[index] = view;
        } else {
            at.insert(view.name.clone(), checks.len());
            checks.push(view);
        }
    }
    if checks.iter().any(|check| check.required) {
        checks.retain(|check| check.required);
    }
    ChecksReport {
        head_sha: snapshot.head_sha.clone(),
        checks,
    }
}

/// One check context, as the view a report holds.
#[must_use]
pub fn check_view_of(context: &CheckContext) -> CheckView {
    match context {
        CheckContext::CheckRun {
            name,
            details_url,
            status,
            started_at,
            completed_at,
            conclusion,
            is_required,
        } => CheckView {
            name: name.clone(),
            state: match (conclusion, status) {
                // Neutral and skipped deserialize as Other and pass
                // GitHub's rollup the same way.
                (Some(CheckConclusion::Success | CheckConclusion::Other), _) => CheckState::Passed,
                (Some(_), _) => CheckState::Failed,
                (None, Some(RunStatus::InProgress)) => CheckState::Running,
                (None, _) => CheckState::Queued,
            },
            required: *is_required,
            url: details_url.clone(),
            started: *started_at,
            finished: *completed_at,
        },
        CheckContext::StatusContext {
            context,
            target_url,
            state,
            created_at,
            is_required,
        } => CheckView {
            name: context.clone(),
            state: match state {
                StatusState::Success => CheckState::Passed,
                StatusState::Failure | StatusState::Error => CheckState::Failed,
                StatusState::Pending => CheckState::Running,
                StatusState::Expected | StatusState::Other => CheckState::Queued,
            },
            required: *is_required,
            url: target_url.clone(),
            started: *created_at,
            finished: None,
        },
    }
}

/// Each failed check of `report` with the key lines of its CI log.
pub async fn failure_details(actions: &impl Actions, report: &ChecksReport) -> Vec<FailureDetail> {
    let failed: Vec<(String, Option<String>, Option<u64>)> = report
        .failed()
        .map(|check| (check.name.clone(), check.url.clone(), check.job_id()))
        .collect();
    stream::iter(failed)
        .map(|(name, url, job)| async move {
            let log = match job {
                None => LogExcerpt::NoLog,
                Some(job) => excerpt(actions, job).await,
            };
            FailureDetail { name, url, log }
        })
        .buffered(LOG_FETCH_CONCURRENCY)
        .collect()
        .await
}

/// The checks part of `pr show`.
#[derive(Debug, Clone, PartialEq, Eq, Template)]
#[template(path = "pr_checks.txt", escape = "none")]
pub enum ChecksSection {
    /// The forge answered.
    Read(ReadChecks),
    /// The forge's query failed.
    Unreadable {
        /// The forge the query ran against (`GitHub` or `GitLab`).
        forge: &'static str,
        /// Why it failed.
        error: String,
    },
}

/// A change in a PR's checks the agent is told about.
#[derive(Debug, Clone, PartialEq, Eq, Template)]
#[template(path = "pr_checks_alert.txt", escape = "none")]
pub enum ChecksEvent {
    /// A check failed.
    Failed {
        /// The PR's sigil (`#` or `!`).
        sigil: char,
        /// The PR number.
        pr_number: i64,
        /// The failed checks.
        names: Vec<String>,
    },
    /// Every check passed.
    Passed {
        /// The PR's sigil.
        sigil: char,
        /// The PR number.
        pr_number: i64,
        /// How many checks ran.
        total: u32,
    },
    /// The checks could not be read from the forge.
    Unreadable {
        /// The PR's sigil.
        sigil: char,
        /// The PR number.
        pr_number: i64,
        /// The forge the query ran against (`GitHub` or `GitLab`).
        forge: &'static str,
        /// Why it failed.
        error: String,
    },
}

impl ChecksEvent {
    /// The alert for a rollup the agent hears of: a failure, or every check
    /// passed.
    ///
    /// # Panics
    /// Panics when the alert template fails to render.
    #[must_use]
    pub fn for_rollup(sigil: char, pr_number: i64, report: &ChecksReport) -> Option<Self> {
        match report.rollup() {
            Rollup::Pending => None,
            Rollup::Failed => Some(Self::Failed {
                sigil,
                pr_number,
                names: report.failed().map(|check| check.name.clone()).collect(),
            }),
            Rollup::Passed => Some(Self::Passed {
                sigil,
                pr_number,
                total: u32::try_from(report.checks.len()).expect("fewer than 2^32 checks"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::merge_watch::PullRequestState;

    fn run(
        name: &str,
        status: RunStatus,
        conclusion: Option<CheckConclusion>,
        required: bool,
        url: Option<&str>,
    ) -> CheckContext {
        CheckContext::CheckRun {
            name: name.to_string(),
            details_url: url.map(str::to_string),
            status: Some(status),
            started_at: Some("2026-11-09T10:00:00Z".parse().unwrap()),
            completed_at: conclusion.map(|_| "2026-11-09T10:02:05Z".parse().unwrap()),
            conclusion,
            is_required: required,
        }
    }

    fn snapshot(checks: Vec<CheckContext>) -> Snapshot {
        Snapshot {
            state: PullRequestState::Open,
            queue_position: None,
            removals: 0,
            last_removal: None,
            removal_checks: Vec::new(),
            merge_state: None,
            head_sha: "abc".to_string(),
            checks,
        }
    }

    #[test]
    fn each_check_has_a_state_and_a_duration() {
        let now: Timestamp = "2026-11-09T10:01:00Z".parse().unwrap();
        let report = checks_report_of(&snapshot(vec![
            run(
                "fmt",
                RunStatus::Completed,
                Some(CheckConclusion::Success),
                true,
                None,
            ),
            run("测试", RunStatus::InProgress, None, true, None),
            run("clippy", RunStatus::Queued, None, true, None),
            run(
                "テスト",
                RunStatus::Completed,
                Some(CheckConclusion::Failure),
                true,
                None,
            ),
        ]));
        let states: Vec<_> = report.checks.iter().map(|c| c.state).collect();
        assert_eq!(
            states,
            [
                CheckState::Passed,
                CheckState::Running,
                CheckState::Queued,
                CheckState::Failed
            ]
        );
        let spans: Vec<_> = report
            .checks
            .iter()
            .map(|c| {
                c.started()
                    .map(|start| c.finished().unwrap_or(now).duration_since(start).as_secs())
            })
            .collect();
        assert_eq!(spans, [Some(125), Some(60), None, Some(125)]);
        assert_eq!(report.rollup(), Rollup::Failed);
        let read = ReadChecks::new(&report, Vec::new());
        assert_eq!((read.finished, read.total), (2, 4));
    }

    #[test]
    fn a_job_id_comes_from_an_actions_link() {
        let check = |url: &str| {
            checks_report_of(&snapshot(vec![run(
                "x",
                RunStatus::Completed,
                Some(CheckConclusion::Failure),
                true,
                Some(url),
            )]))
            .checks
            .remove(0)
        };
        assert_eq!(
            check("https://github.com/o/r/actions/runs/9/job/8?pr=1").job_id(),
            Some(8)
        );
        assert_eq!(check("https://ci.example/build/3").job_id(), None);
    }

    #[test]
    fn the_rollup_alerts_only_on_failure_or_all_passed() {
        let pending = checks_report_of(&snapshot(vec![run(
            "a",
            RunStatus::InProgress,
            None,
            true,
            None,
        )]));
        assert_eq!(ChecksEvent::for_rollup('#', 3, &pending), None);
        let passed = checks_report_of(&snapshot(vec![
            run(
                "a",
                RunStatus::Completed,
                Some(CheckConclusion::Success),
                true,
                None,
            ),
            run(
                "docs",
                RunStatus::Completed,
                Some(CheckConclusion::Failure),
                false,
                None,
            ),
        ]));
        assert_eq!(
            ChecksEvent::for_rollup('#', 3, &passed),
            Some(ChecksEvent::Passed {
                sigil: '#',
                pr_number: 3,
                total: 1
            })
        );
    }

    #[test]
    fn the_alert_names_the_failed_checks_and_points_to_pr_show() {
        let text = ChecksEvent::Failed {
            sigil: '#',
            pr_number: 12,
            names: vec![
                "test (ubuntu)".to_string(),
                "测试 テスト 테스트".to_string(),
            ],
        }
        .render()
        .unwrap();
        assert_eq!(
            text,
            "PR #12's checks failed: test (ubuntu), 测试 テスト 테스트. `pr show 12` lists the error lines of their logs."
        );
        let text = ChecksEvent::Passed {
            sigil: '#',
            pr_number: 12,
            total: 5,
        }
        .render()
        .unwrap();
        assert_eq!(text, "All 5 checks of PR #12 passed.");
    }

    #[test]
    fn a_failed_query_is_stated() {
        let text = ChecksEvent::Unreadable {
            sigil: '#',
            pr_number: 12,
            forge: "GitHub",
            error: "GitHub 502".to_string(),
        }
        .render()
        .unwrap();
        assert_eq!(
            text,
            "PR #12's checks could not be read from GitHub (GitHub 502); `pr show 12` retries the query."
        );
        let text = ChecksSection::Unreadable {
            forge: "GitHub",
            error: "GitHub 502".to_string(),
        }
        .render()
        .unwrap();
        assert_eq!(text, "checks: could not be read from GitHub (GitHub 502)");
        let text = ChecksEvent::Unreadable {
            sigil: '!',
            pr_number: 12,
            forge: "GitLab",
            error: "GitLab 502".to_string(),
        }
        .render()
        .unwrap();
        assert_eq!(
            text,
            "PR !12's checks could not be read from GitLab (GitLab 502); `pr show 12` retries the query."
        );
    }

    #[test]
    fn pr_show_lists_each_failed_check_with_its_log_lines() {
        let section = ChecksSection::Read(ReadChecks {
            total: 4,
            finished: 3,
            passed: 1,
            pending: 1,
            failed: 2,
            failures: vec![
                FailureDetail {
                    name: "测试".to_string(),
                    url: Some("https://github.com/o/r/actions/runs/9/job/8".to_string()),
                    log: LogExcerpt::Lines(vec![
                        "error[E0308]: 类型不匹配".to_string(),
                        "テスト failed: 테스트".to_string(),
                    ]),
                },
                FailureDetail {
                    name: "ci/legacy".to_string(),
                    url: None,
                    log: LogExcerpt::NoLog,
                },
            ],
        });
        assert_eq!(
            section.render().unwrap(),
            "checks: 3 of 4 finished, 1 passed, 1 running or queued, 2 failed\n\
             failed 测试 (https://github.com/o/r/actions/runs/9/job/8):\n\
             > error[E0308]: 类型不匹配\n\
             > テスト failed: 테스트\n\
             failed ci/legacy: it is not a CI job, so it has no log"
        );
    }
}
