//! `ci` terminal command: where a session PR's CI time goes.

use std::borrow::Cow;
use std::path::PathBuf;
use std::time::Duration;

use aither_core::llm::{Tool, ToolContext, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::ci::CiError;
use crate::ci::logs::{TestKind, TestTiming};
use crate::ci::report::{
    CiQueue, CiSlow, CiStats, JobView, QueueRow, Reason, Span, StatsRow, Timed, TrendView,
};
use crate::ci::timing::{Limits, judge, median, p90, trend};
use crate::db::{ChatDb, CiJobRow};

/// Steps and tests `ci slow` lists per job.
const SLOW_STEPS: usize = 5;
const SLOW_TESTS: usize = 10;

/// CI durations recorded when a session PR's checks finish.
///
/// Subcommands:
/// - `ci stats` — each job's latest, median and p90 duration and its trend
/// - `ci slow <pr>` — a PR's checks by job, step and test against the job
///   baselines; `ci slow <run> --run` names a GitHub workflow run instead
/// - `ci queue` — merge-queue wait against run time for the session's PRs
///
/// A job is abnormally long past the `[ci]` limit or past a multiple of its
/// baseline, the median of its last successful base-branch runs.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CiArgs {
    /// Per-job latest, median and p90 durations and trend.
    Stats {},
    /// A PR's or a run's checks broken down by job, step and test.
    Slow {
        /// The PR number, or the workflow run id with `--run`.
        number: i64,
        /// `number` is a GitHub workflow run id.
        #[serde(default)]
        run: bool,
    },
    /// Merge-queue wait against run time.
    Queue {},
}

/// The `ci` command of one session.
pub struct CiTool {
    db: ChatDb,
    session_id: String,
    may_dir: PathBuf,
}

impl std::fmt::Debug for CiTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CiTool").finish_non_exhaustive()
    }
}

impl CiTool {
    /// The command for `session_id`, reading `[ci]` from `may_dir`'s config
    /// each time it judges a job.
    #[must_use]
    pub fn new(db: ChatDb, session_id: impl Into<String>, may_dir: PathBuf) -> Self {
        Self {
            db,
            session_id: session_id.into(),
            may_dir,
        }
    }

    async fn limits(&self) -> Result<Limits, CiError> {
        let config = crate::config::MayConfig::load_from(&self.may_dir.join("config.toml")).await?;
        Ok(config.ci.into())
    }

    async fn stats(&self) -> Result<String, CiError> {
        let jobs = self.db.ci_jobs_for_session_repos(&self.session_id).await?;
        let mut groups: Vec<((i64, String, String), Vec<u64>)> = Vec::new();
        // Newest first, so a group's first duration is its latest.
        for job in &jobs {
            let key = (job.project_id, job.workflow.clone(), job.name.clone());
            let secs = job.duration_secs.unsigned_abs();
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, durations)) => durations.push(secs),
                None => groups.push((key, vec![secs])),
            }
        }
        let rows = groups
            .into_iter()
            .map(|((_, workflow, name), newest_first)| {
                let mut oldest_first = newest_first.clone();
                oldest_first.reverse();
                let span = |secs: Option<u64>| Span(Duration::from_secs(secs.unwrap_or(0)));
                StatsRow {
                    workflow,
                    name,
                    runs: newest_first.len(),
                    latest: span(newest_first.first().copied()),
                    median: span(median(&newest_first)),
                    p90: span(p90(&newest_first)),
                    trend: TrendView(trend(&oldest_first)),
                }
            })
            .collect();
        Ok(CiStats { rows }.render()?)
    }

    async fn job_view(&self, job: &CiJobRow, limits: Limits) -> Result<JobView, CiError> {
        let baseline = self
            .db
            .ci_baseline(job.project_id, &job.workflow, &job.name)
            .await?
            .map(|row| Duration::from_secs(row.median_secs.unsigned_abs()));
        let duration = Duration::from_secs(job.duration_secs.unsigned_abs());
        let mut steps = self.db.ci_steps(job.id).await?;
        steps.sort_by_key(|step| std::cmp::Reverse(step.duration_secs));
        let tests = self.db.ci_tests(job.id).await?;
        Ok(JobView {
            workflow: job.workflow.clone(),
            name: job.name.clone(),
            conclusion: job.conclusion.clone(),
            duration: Span(duration),
            baseline: baseline.map(Span),
            exceeded: judge(duration, baseline, limits)
                .into_iter()
                .map(Reason)
                .collect(),
            steps: steps
                .iter()
                .take(SLOW_STEPS)
                .map(|step| {
                    Timed::step(
                        &step.name,
                        Duration::from_secs(step.duration_secs.unsigned_abs()),
                    )
                })
                .collect(),
            tests: tests
                .iter()
                .take(SLOW_TESTS)
                .filter_map(|test| {
                    Some(Timed::test(&TestTiming {
                        kind: TestKind::from_stored(&test.kind)?,
                        name: test.name.clone(),
                        millis: test.duration_ms.unsigned_abs(),
                    }))
                })
                .collect(),
        })
    }

    async fn slow(&self, number: i64, run: bool) -> Result<String, CiError> {
        let (label, jobs) = if run {
            (
                format!("run {number}"),
                self.db.ci_jobs_for_run(number).await?,
            )
        } else {
            let pr = self
                .db
                .list_session_prs(&self.session_id)
                .await?
                .into_iter()
                .find(|pr| pr.number == number);
            let jobs = match &pr {
                Some(pr) => self.db.ci_jobs_for_pr(pr.id).await?,
                None => Vec::new(),
            };
            (format!("PR #{number}"), jobs)
        };
        let limits = self.limits().await?;
        let mut views = Vec::with_capacity(jobs.len());
        for job in &jobs {
            views.push(self.job_view(job, limits).await?);
        }
        Ok(CiSlow { label, jobs: views }.render()?)
    }

    async fn queue(&self) -> Result<String, CiError> {
        let numbers: std::collections::HashMap<i64, i64> = self
            .db
            .list_session_prs(&self.session_id)
            .await?
            .into_iter()
            .map(|pr| (pr.id, pr.number))
            .collect();
        let rows = self
            .db
            .ci_pr_runs_for_session(&self.session_id)
            .await?
            .into_iter()
            .filter_map(|row| {
                Some(QueueRow {
                    pr_number: *numbers.get(&row.pr_id)?,
                    wait: Span(Duration::from_secs(row.queue_wait_secs.unsigned_abs())),
                    run: Span(Duration::from_secs(row.run_secs.unsigned_abs())),
                })
            })
            .collect();
        Ok(CiQueue { rows }.render()?)
    }
}

impl Tool for CiTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("ci")
    }

    type Arguments = CiArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> aither_core::Result<Self::Res> {
        let text = match args {
            CiArgs::Stats {} => self.stats().await,
            CiArgs::Slow { number, run } => self.slow(number, run).await,
            CiArgs::Queue {} => self.queue().await,
        };
        Ok(ToolResult::text(text.unwrap_or_else(|e| e.to_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NewCiJob;

    async fn setup() -> (tempfile::TempDir, ChatDb, CiTool, crate::db::PrRow) {
        let dir = tempfile::tempdir().unwrap();
        let db = ChatDb::open(&dir.path().join("history.db")).await.unwrap();
        let pr = db
            .create_pr(
                "github:o/r",
                "s",
                "t",
                "",
                "b",
                "base",
                "github",
                true,
                None,
                Some(9),
            )
            .await
            .unwrap();
        let tool = CiTool::new(db.clone(), "s", dir.path().to_path_buf());
        (dir, db, tool, pr)
    }

    fn job(pr: &crate::db::PrRow, id: i64, secs: i64) -> NewCiJob {
        NewCiJob {
            project_id: pr.project_id,
            pr_id: pr.id,
            session_id: "s".into(),
            source: "github".into(),
            external_id: id,
            run_id: 77,
            workflow: "CI".into(),
            name: "test".into(),
            conclusion: "success".into(),
            started_at_ms: id,
            duration_secs: secs,
            steps: vec![("build".into(), 20), ("Run tests".into(), secs - 20)],
            tests: vec![
                ("binary".into(), "src/lib.rs".into(), 62_000),
                ("test".into(), "a::slow".into(), 61_000),
            ],
        }
    }

    #[tokio::test]
    async fn stats_slow_and_queue_report_from_recorded_runs() {
        let (_dir, db, tool, pr) = setup().await;
        for (id, secs) in [(1, 100), (2, 100), (3, 200), (4, 200)] {
            db.record_ci_job(&job(&pr, id, secs)).await.unwrap();
        }
        db.set_ci_baseline(pr.project_id, "CI", "test", 90, 5)
            .await
            .unwrap();
        db.record_ci_pr_run(pr.id, 600, 200).await.unwrap();

        let stats = tool.stats().await.unwrap();
        assert!(
            stats.contains(
                "CI / test: latest 3m 20s, median 2m 30s, p90 3m 20s over 4 runs; getting slower."
            ),
            "{stats}"
        );
        let slow = tool.slow(pr.number, false).await.unwrap();
        assert!(slow.contains("PR #1, by job, slowest first"), "{slow}");
        assert!(
            slow.contains("baseline 1m 30s; abnormal: past 2x its baseline"),
            "{slow}"
        );
        assert!(
            slow.contains("Slowest steps: Run tests (3m 00s), build (20s)."),
            "{slow}"
        );
        assert!(
            slow.contains("src/lib.rs (all tests) (1m 02s), a::slow (1m 01s)"),
            "{slow}"
        );
        assert!(
            tool.slow(77, true)
                .await
                .unwrap()
                .contains("run 77, by job")
        );
        let queue = tool.queue().await.unwrap();
        assert!(
            queue.contains("PR #1: waited 10m 00s in the queue after a 3m 20s run."),
            "{queue}"
        );
    }

    #[tokio::test]
    async fn nothing_recorded_says_so() {
        let (_dir, _db, tool, _pr) = setup().await;
        assert!(
            tool.stats()
                .await
                .unwrap()
                .contains("No CI runs are recorded")
        );
        assert!(
            tool.slow(5, false)
                .await
                .unwrap()
                .contains("No jobs are recorded for PR #5")
        );
        assert!(tool.queue().await.unwrap().contains("No merge-queue waits"));
    }
}
