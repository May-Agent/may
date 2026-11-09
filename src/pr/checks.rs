//! A PR's checks as the agent and the sidebars read them: one state per
//! check, the rollup, the failure excerpts `pr show` prints and the alerts
//! the daemon raises when the rollup changes.

use std::collections::HashMap;

use askama::Template;
use futures_util::{StreamExt, stream};
use jiff::Timestamp;

use super::merge_watch::{
    CheckConclusion, CheckContext, CheckSummary, RunStatus, Snapshot, StatusState,
};
use crate::ci::github::Actions;

/// Where one check is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    /// Not started.
    Queued,
    /// Running.
    Running,
    /// Concluded successfully (neutral and skipped included).
    Passed,
    /// Failed, timed out, cancelled or errored.
    Failed,
}

impl CheckState {
    /// The name clients show and the wire carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }
}

/// One check of the PR's head commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckView {
    pub name: String,
    pub state: CheckState,
    pub required: bool,
    /// Where GitHub shows the check.
    pub url: Option<String>,
    started: Option<Timestamp>,
    finished: Option<Timestamp>,
}

impl CheckView {
    /// When the check started; `None` while it is queued.
    #[must_use]
    pub const fn started(&self) -> Option<Timestamp> {
        match self.state {
            CheckState::Queued => None,
            _ => self.started,
        }
    }

    /// When the check concluded.
    #[must_use]
    pub const fn finished(&self) -> Option<Timestamp> {
        match self.state {
            CheckState::Passed | CheckState::Failed => self.finished,
            CheckState::Queued | CheckState::Running => None,
        }
    }

    /// The Actions job id a `…/actions/runs/<run>/job/<job>` link names.
    #[must_use]
    pub fn job_id(&self) -> Option<u64> {
        let (_, job) = self.url.as_deref()?.split_once("/actions/runs/")?;
        job.split_once("/job/")?
            .1
            .split(['/', '?', '#'])
            .next()?
            .parse()
            .ok()
    }

    fn of(context: &CheckContext) -> Self {
        match context {
            CheckContext::CheckRun {
                name,
                details_url,
                status,
                started_at,
                completed_at,
                conclusion,
                is_required,
            } => Self {
                name: name.clone(),
                state: match (conclusion, status) {
                    // Neutral and skipped deserialize as Other and pass
                    // GitHub's rollup the same way.
                    (Some(CheckConclusion::Success | CheckConclusion::Other), _) => {
                        CheckState::Passed
                    }
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
            } => Self {
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
}

/// How the checks stand as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rollup {
    /// Some check is queued or running, and none has failed.
    Pending,
    /// Every check passed.
    Passed,
    /// Some check failed.
    Failed,
}

impl Rollup {
    /// The name the database stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }
}

/// The checks on a PR's head commit: required ones when any exist (they are
/// what merge waits on), every one otherwise. A re-run check shares its
/// name; the last report wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksReport {
    pub head_sha: String,
    pub checks: Vec<CheckView>,
}

impl ChecksReport {
    #[must_use]
    pub fn of(snapshot: &Snapshot) -> Self {
        let mut checks: Vec<CheckView> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        for context in &snapshot.checks {
            let view = CheckView::of(context);
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
        Self {
            head_sha: snapshot.head_sha.clone(),
            checks,
        }
    }

    #[must_use]
    pub fn summary(&self) -> CheckSummary {
        let count = |states: &[CheckState]| {
            let n = self
                .checks
                .iter()
                .filter(|check| states.contains(&check.state))
                .count();
            u32::try_from(n).expect("a PR has fewer than 2^32 checks")
        };
        CheckSummary {
            pending: count(&[CheckState::Queued, CheckState::Running]),
            passed: count(&[CheckState::Passed]),
            failed: count(&[CheckState::Failed]),
        }
    }

    #[must_use]
    pub fn rollup(&self) -> Rollup {
        let summary = self.summary();
        if summary.failed > 0 {
            Rollup::Failed
        } else if summary.pending > 0 || self.checks.is_empty() {
            Rollup::Pending
        } else {
            Rollup::Passed
        }
    }

    pub fn failed(&self) -> impl Iterator<Item = &CheckView> {
        self.checks
            .iter()
            .filter(|check| check.state == CheckState::Failed)
    }
}

/// What the log of a failed check yields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogExcerpt {
    /// The lines that say what went wrong.
    Lines(Vec<String>),
    /// The log has no error line.
    NoErrorLines,
    /// The log could not be read, for this reason.
    Unavailable(String),
    /// The check is not an Actions job, so it has no log to read.
    NoLog,
}

/// A failed check with what its log says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureDetail {
    pub name: String,
    pub url: Option<String>,
    pub log: LogExcerpt,
}

/// Error lines kept per failed check.
const LOG_LINES: usize = 12;

/// Failed checks whose logs are fetched at once.
const LOG_FETCH_CONCURRENCY: usize = 4;

/// Each failed check of `report` with the key lines of its Actions log.
pub async fn failure_details(
    owner_repo: &str,
    pr_number: i64,
    report: &ChecksReport,
) -> Vec<FailureDetail> {
    let actions = crate::ci::github::GhActions::new(owner_repo, pr_number);
    let actions = &actions;
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

async fn excerpt(actions: &crate::ci::github::GhActions, job: u64) -> LogExcerpt {
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
    pub total: u32,
    pub finished: u32,
    pub passed: u32,
    pub pending: u32,
    pub failed: u32,
    pub failures: Vec<FailureDetail>,
}

impl ReadChecks {
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

/// The checks part of `pr show`.
#[derive(Debug, Clone, PartialEq, Eq, Template)]
#[template(path = "pr_checks.txt", escape = "none")]
pub enum ChecksSection {
    /// GitHub answered.
    Read(ReadChecks),
    /// The GitHub query failed.
    Unreadable(String),
}

/// A change in a PR's checks the agent is told about.
#[derive(Debug, Clone, PartialEq, Eq, Template)]
#[template(path = "pr_checks_alert.txt", escape = "none")]
pub enum ChecksEvent {
    /// A check failed.
    Failed { pr_number: i64, names: Vec<String> },
    /// Every check passed.
    Passed { pr_number: i64, total: u32 },
    /// The checks could not be read from GitHub.
    Unreadable { pr_number: i64, error: String },
}

impl ChecksEvent {
    /// The alert for a rollup the agent hears of: a failure, or every check
    /// passed.
    #[must_use]
    pub fn for_rollup(pr_number: i64, report: &ChecksReport) -> Option<Self> {
        match report.rollup() {
            Rollup::Pending => None,
            Rollup::Failed => Some(Self::Failed {
                pr_number,
                names: report.failed().map(|check| check.name.clone()).collect(),
            }),
            Rollup::Passed => Some(Self::Passed {
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
            head_sha: "abc".to_string(),
            checks,
        }
    }

    #[test]
    fn each_check_has_a_state_and_a_duration() {
        let now: Timestamp = "2026-11-09T10:01:00Z".parse().unwrap();
        let report = ChecksReport::of(&snapshot(vec![
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
            ChecksReport::of(&snapshot(vec![run(
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
        let pending = ChecksReport::of(&snapshot(vec![run(
            "a",
            RunStatus::InProgress,
            None,
            true,
            None,
        )]));
        assert_eq!(ChecksEvent::for_rollup(3, &pending), None);
        let passed = ChecksReport::of(&snapshot(vec![
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
            ChecksEvent::for_rollup(3, &passed),
            Some(ChecksEvent::Passed {
                pr_number: 3,
                total: 1
            })
        );
    }

    #[test]
    fn the_alert_names_the_failed_checks_and_points_to_pr_show() {
        let text = ChecksEvent::Failed {
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
            pr_number: 12,
            error: "GitHub 502".to_string(),
        }
        .render()
        .unwrap();
        assert_eq!(
            text,
            "PR #12's checks could not be read from GitHub (GitHub 502); `pr show 12` retries the query."
        );
        let text = ChecksSection::Unreadable("GitHub 502".to_string())
            .render()
            .unwrap();
        assert_eq!(text, "checks: could not be read from GitHub (GitHub 502)");
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
             failed ci/legacy: it is not an Actions job, so it has no log"
        );
    }
}
