//! The PR check-summary wire types.

use jiff::Timestamp;

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

/// One check of the PR's head commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckView {
    /// The check's display name.
    pub name: String,
    /// Where the check is.
    pub state: CheckState,
    /// Whether merge waits on it.
    pub required: bool,
    /// Where GitHub shows the check.
    pub url: Option<String>,
    /// When the check started.
    pub started: Option<Timestamp>,
    /// When the check ended.
    pub finished: Option<Timestamp>,
}

/// How the checks stand as a whole; stored as its `snake_case` name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, skyzen::Column)]
pub enum Rollup {
    /// Some check is queued or running, and none has failed.
    Pending,
    /// Every check passed.
    Passed,
    /// Some check failed.
    Failed,
}

/// The checks on a PR's head commit: required ones when any exist (they are
/// what merge waits on), every one otherwise. A re-run check shares its
/// name; the last report wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksReport {
    /// The head commit the checks ran on.
    pub head_sha: String,
    /// The checks, one per name.
    pub checks: Vec<CheckView>,
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
    /// The check is not a CI job, so it has no log to read.
    NoLog,
}

/// A failed check with what its log says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureDetail {
    /// The failed check's display name.
    pub name: String,
    /// Where the check is shown; `None` when it has no page.
    pub url: Option<String>,
    /// What its log says.
    pub log: LogExcerpt,
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

    /// The CI job id a job link names: `…/actions/runs/<run>/job/<job>` on
    /// GitHub, `…/-/jobs/<job>` on GitLab.
    #[must_use]
    pub fn job_id(&self) -> Option<u64> {
        let url = self.url.as_deref()?;
        if let Some((_, run)) = url.split_once("/actions/runs/") {
            return run
                .split_once("/job/")?
                .1
                .split(['/', '?', '#'])
                .next()?
                .parse()
                .ok();
        }
        url.split_once("/-/jobs/")?
            .1
            .split(['/', '?', '#'])
            .next()?
            .parse()
            .ok()
    }
}

impl ChecksReport {
    /// The checks whose run failed.
    pub fn failed(&self) -> impl Iterator<Item = &CheckView> {
        self.checks
            .iter()
            .filter(|check| check.state == CheckState::Failed)
    }

    /// How many of the checks sit in each state.
    ///
    /// # Panics
    /// Panics when a check count does not fit in `u32`.
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

    /// How the checks stand as a whole.
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
}

/// How many of the PR's checks sit in each state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckSummary {
    /// Still running or not yet reported.
    pub pending: u32,
    /// Concluded successfully (neutral and skipped included).
    pub passed: u32,
    /// Failed, timed out, cancelled or errored.
    pub failed: u32,
}
