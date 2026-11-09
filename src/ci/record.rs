//! Recording a PR's check durations and judging them.

use std::collections::HashMap;
use std::time::Duration;

use askama::Template;
use futures_util::{StreamExt, stream};

use super::CiError;
use super::github::{Actions, Job, Run};
use super::logs;
use super::report::{CiAlert, CiWarning, JobView, Reason, Span, Timed};
use super::timing::{BASELINE_RUNS, Limits, judge, median};
use crate::db::{ChatDb, NewCiJob, PrRow};

/// Jobs shorter than this have no log read: tests that matter run longer.
const LOG_MIN: Duration = Duration::from_secs(60);

/// Timings kept per job.
const KEPT_TIMINGS: usize = 50;

/// Steps and tests a finding names.
const SHOWN_STEPS: usize = 3;
const SHOWN_TESTS: usize = 5;

/// API reads in flight at once.
const FETCH_CONCURRENCY: usize = 4;

/// What a recorded PR's abnormal jobs became.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The text shown to the user.
    pub warning: String,
}

/// Records and judges the durations of one session's PR.
#[derive(Debug)]
pub struct Recorder<'a, A> {
    /// The database durations are stored in.
    pub db: &'a ChatDb,
    /// GitHub Actions.
    pub actions: &'a A,
    /// The thresholds, read when the checks finished.
    pub limits: Limits,
}

impl<A: Actions> Recorder<'_, A> {
    /// Records every job of the workflow runs on `head_sha`, refreshes their
    /// baselines, and stores a finding when a job ran abnormally long. The
    /// finding is returned only the first time the commit produces one.
    ///
    /// # Errors
    /// Fails when GitHub or the database cannot be read or written.
    pub async fn observe_github(
        &self,
        pr: &PrRow,
        head_sha: &str,
    ) -> Result<Option<Finding>, CiError> {
        let runs: Vec<Run> = self
            .actions
            .runs_for_sha(head_sha)
            .await?
            .into_iter()
            .filter(|run| run.status == "completed")
            .collect();
        let jobs_by_run = self.jobs_of(&runs).await?;
        match self.actions.base_branch().await {
            Ok(base) => self.refresh_baselines(pr.project_id, &runs, &base).await,
            Err(error) => {
                tracing::warn!(%error, "the base branch is unknown, so baselines are not refreshed");
            }
        }

        let mut views = Vec::new();
        for (run, jobs) in runs.iter().zip(jobs_by_run) {
            let workflow = workflow_name(run);
            let timed: Vec<(&Job, Duration)> = jobs
                .iter()
                .filter_map(|job| Some((job, job.duration()?)))
                .collect();
            let reads: Vec<_> = timed
                .iter()
                .map(|(job, duration)| async move {
                    if *duration < LOG_MIN {
                        return None;
                    }
                    match self.actions.log(job.id).await {
                        Ok(log) => Some(log),
                        Err(error) => {
                            tracing::warn!(%error, job = %job.name, "a job log could not be read");
                            None
                        }
                    }
                })
                .collect();
            let job_logs: Vec<Option<String>> = stream::iter(reads)
                .buffered(FETCH_CONCURRENCY)
                .collect()
                .await;
            for ((job, duration), log) in timed.into_iter().zip(job_logs) {
                let mut timings = log.as_deref().map(logs::parse).unwrap_or_default();
                timings.sort_by_key(|timing| std::cmp::Reverse(timing.millis));
                timings.truncate(KEPT_TIMINGS);
                let steps: Vec<(String, Duration)> = job
                    .steps
                    .iter()
                    .filter_map(|step| Some((step.name.clone(), step.duration()?)))
                    .collect();
                let view = self
                    .record(
                        pr,
                        NewCiJob {
                            project_id: pr.project_id,
                            pr_id: pr.id,
                            session_id: pr.session_id.clone(),
                            source: "github".into(),
                            external_id: to_i64(job.id),
                            run_id: to_i64(run.id),
                            workflow: workflow.clone(),
                            name: job.name.clone(),
                            conclusion: job.conclusion.clone().unwrap_or_default(),
                            started_at_ms: job
                                .started_at
                                .map_or(0, jiff::Timestamp::as_millisecond),
                            duration_secs: to_i64(duration.as_secs()),
                            steps: steps
                                .iter()
                                .map(|(name, d)| (name.clone(), to_i64(d.as_secs())))
                                .collect(),
                            tests: timings
                                .iter()
                                .map(|t| (t.kind.as_str().into(), t.name.clone(), to_i64(t.millis)))
                                .collect(),
                        },
                    )
                    .await?;
                if let Some(mut view) = view {
                    view.steps = slowest_steps(&steps);
                    view.tests = timings.iter().take(SHOWN_TESTS).map(Timed::test).collect();
                    views.push(view);
                }
            }
        }
        self.finish(pr, head_sha, &views).await
    }
}

impl<A: Sync> Recorder<'_, A> {
    /// Records a local `[[projects]] checks` run that took `duration`, and
    /// judges it against the median of the last successful local runs.
    ///
    /// # Errors
    /// Fails when the database cannot be read or written.
    pub async fn observe_local(
        &self,
        pr: &PrRow,
        head_sha: &str,
        command: &str,
        started: jiff::Timestamp,
        duration: Duration,
        success: bool,
    ) -> Result<Option<Finding>, CiError> {
        let history = self
            .db
            .ci_job_history(
                pr.project_id,
                "local",
                command,
                to_i64(BASELINE_RUNS as u64),
            )
            .await?;
        let samples: Vec<u64> = history
            .iter()
            .filter(|job| job.conclusion == "success")
            .map(|job| job.duration_secs.unsigned_abs())
            .collect();
        if let Some(median) = median(&samples) {
            self.db
                .set_ci_baseline(
                    pr.project_id,
                    "local",
                    command,
                    to_i64(median),
                    to_i64(samples.len() as u64),
                )
                .await?;
        }
        let view = self
            .record(
                pr,
                NewCiJob {
                    project_id: pr.project_id,
                    pr_id: pr.id,
                    session_id: pr.session_id.clone(),
                    source: "local".into(),
                    external_id: started.as_millisecond(),
                    run_id: 0,
                    workflow: "local".into(),
                    name: command.to_string(),
                    conclusion: if success { "success" } else { "failure" }.into(),
                    started_at_ms: started.as_millisecond(),
                    duration_secs: to_i64(duration.as_secs()),
                    steps: Vec::new(),
                    tests: Vec::new(),
                },
            )
            .await?;
        let views: Vec<JobView> = view.into_iter().collect();
        self.finish(pr, &format!("local:{head_sha}"), &views).await
    }

    /// Records how long the PR waited in the merge queue against the run
    /// time of its slowest job.
    ///
    /// # Errors
    /// Fails when the database cannot be read or written.
    pub async fn record_queue_wait(&self, pr: &PrRow) -> Result<(), CiError> {
        let (Some(queued), Some(merged)) = (
            self.db.pr_event_time(pr.id, "queued").await?,
            self.db.pr_event_time(pr.id, "merged").await?,
        ) else {
            return Ok(());
        };
        let wait = merged.duration_since(queued).as_secs().max(0);
        let run = self
            .db
            .ci_jobs_for_pr(pr.id)
            .await?
            .first()
            .map_or(0, |job| job.duration_secs);
        self.db.record_ci_pr_run(pr.id, wait, run).await?;
        Ok(())
    }
}

impl<A: Actions> Recorder<'_, A> {
    async fn jobs_of(&self, runs: &[Run]) -> Result<Vec<Vec<Job>>, CiError> {
        let reads: Vec<_> = runs.iter().map(|run| self.actions.jobs(run.id)).collect();
        stream::iter(reads)
            .buffered(FETCH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect()
    }

    /// Recomputes the baseline of every job of the workflows in `runs` from
    /// their last successful runs on `base`. A workflow whose history cannot
    /// be read keeps the baseline it had.
    async fn refresh_baselines(&self, project_id: i64, runs: &[Run], base: &str) {
        let mut workflows: Vec<(u64, String)> = runs
            .iter()
            .map(|run| (run.workflow_id, workflow_name(run)))
            .collect();
        workflows.sort();
        workflows.dedup();
        for (workflow_id, workflow) in workflows {
            let samples = async {
                let history = self
                    .actions
                    .successful_runs(workflow_id, base, BASELINE_RUNS)
                    .await?;
                self.jobs_of(&history).await
            }
            .await;
            let samples = match samples {
                Ok(samples) => samples,
                Err(error) => {
                    tracing::warn!(%error, %workflow, "baselines are not refreshed");
                    continue;
                }
            };
            let mut by_name: HashMap<&str, Vec<u64>> = HashMap::new();
            for job in samples.iter().flatten() {
                if job.conclusion.as_deref() == Some("success")
                    && let Some(duration) = job.duration()
                {
                    by_name
                        .entry(&job.name)
                        .or_default()
                        .push(duration.as_secs());
                }
            }
            for (name, durations) in by_name {
                let Some(median) = median(&durations) else {
                    continue;
                };
                if let Err(error) = self
                    .db
                    .set_ci_baseline(
                        project_id,
                        &workflow,
                        name,
                        to_i64(median),
                        to_i64(durations.len() as u64),
                    )
                    .await
                {
                    tracing::warn!(%error, %workflow, job = name, "a baseline could not be stored");
                }
            }
        }
    }
}

impl<A: Sync> Recorder<'_, A> {
    /// Stores the job and returns its view when it ran abnormally long.
    async fn record(&self, _pr: &PrRow, job: NewCiJob) -> Result<Option<JobView>, CiError> {
        let baseline = self
            .db
            .ci_baseline(job.project_id, &job.workflow, &job.name)
            .await?
            .map(|row| Duration::from_secs(row.median_secs.unsigned_abs()));
        let duration = Duration::from_secs(job.duration_secs.unsigned_abs());
        self.db.record_ci_job(&job).await?;
        let exceeded = judge(duration, baseline, self.limits);
        if exceeded.is_empty() {
            return Ok(None);
        }
        Ok(Some(JobView {
            workflow: job.workflow,
            name: job.name,
            conclusion: job.conclusion,
            duration: Span(duration),
            baseline: baseline.map(Span),
            exceeded: exceeded.into_iter().map(Reason).collect(),
            steps: Vec::new(),
            tests: Vec::new(),
        }))
    }

    /// Stores the finding the abnormal jobs make, once per checked commit.
    async fn finish(
        &self,
        pr: &PrRow,
        key: &str,
        views: &[JobView],
    ) -> Result<Option<Finding>, CiError> {
        if views.is_empty() {
            return Ok(None);
        }
        let warning = CiWarning {
            pr_number: pr.number,
            jobs: views,
        }
        .render()?;
        let alert = CiAlert {
            pr_number: pr.number,
            jobs: views,
        }
        .render()?;
        let warning = warning.trim_end().to_string();
        let stored = self
            .db
            .add_ci_finding(&pr.session_id, pr.id, key, &warning, alert.trim_end())
            .await?;
        Ok(stored.then_some(Finding { warning }))
    }
}

fn workflow_name(run: &Run) -> String {
    run.name
        .clone()
        .unwrap_or_else(|| format!("workflow {}", run.workflow_id))
}

fn slowest_steps(steps: &[(String, Duration)]) -> Vec<Timed> {
    let mut sorted: Vec<&(String, Duration)> = steps.iter().collect();
    sorted.sort_by_key(|(_, duration)| std::cmp::Reverse(*duration));
    sorted
        .into_iter()
        .take(SHOWN_STEPS)
        .map(|(name, duration)| Timed::step(name, *duration))
        .collect()
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).expect("a duration or id fits in an i64")
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::config::CiConfig;

    struct Fake {
        runs: Vec<Run>,
        jobs: HashMap<u64, Vec<Job>>,
        history: Vec<Run>,
        log: String,
    }

    #[allow(clippy::unused_async_trait_impl)]
    impl Actions for Fake {
        async fn base_branch(&self) -> Result<String, CiError> {
            Ok("dev".to_string())
        }
        async fn runs_for_sha(&self, _: &str) -> Result<Vec<Run>, CiError> {
            Ok(self.runs.clone())
        }
        async fn jobs(&self, run_id: u64) -> Result<Vec<Job>, CiError> {
            Ok(self.jobs.get(&run_id).cloned().unwrap_or_default())
        }
        async fn successful_runs(&self, _: u64, _: &str, _: usize) -> Result<Vec<Run>, CiError> {
            Ok(self.history.clone())
        }
        async fn log(&self, _: u64) -> Result<String, CiError> {
            Ok(self.log.clone())
        }
    }

    fn run(id: u64) -> Run {
        Run {
            id,
            workflow_id: 1,
            name: Some("CI".into()),
            status: "completed".into(),
        }
    }

    fn job(id: u64, secs: i64) -> Job {
        let start: jiff::Timestamp = "2026-11-06T10:00:00Z".parse().unwrap();
        let end = start + jiff::SignedDuration::from_secs(secs);
        Job {
            id,
            name: "test".into(),
            conclusion: Some("success".into()),
            started_at: Some(start),
            completed_at: Some(end),
            steps: vec![super::super::github::Step {
                name: "Run tests".into(),
                started_at: Some(start),
                completed_at: Some(end),
            }],
        }
    }

    /// Five base-branch runs of 5 minutes, and a PR run of `pr_secs`.
    fn fake(pr_secs: i64) -> Fake {
        Fake {
            runs: vec![run(100)],
            jobs: HashMap::from([(100, vec![job(1, pr_secs)])]),
            history: (200..205).map(run).collect(),
            log: include_str!("../../tests/fixtures/ci/nextest.log").to_string(),
        }
        .with_history_jobs()
    }

    impl Fake {
        fn with_history_jobs(mut self) -> Self {
            for id in 200..205 {
                self.jobs.insert(id, vec![job(id, 300)]);
            }
            self
        }
    }

    async fn setup() -> (tempfile::TempDir, ChatDb, PrRow) {
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
        (dir, db, pr)
    }

    fn limits() -> Limits {
        CiConfig::default().into()
    }

    #[tokio::test]
    async fn a_job_past_twice_its_baseline_makes_one_finding() {
        let (_dir, db, pr) = setup().await;
        let actions = fake(11 * 60);
        let recorder = Recorder {
            db: &db,
            actions: &actions,
            limits: limits(),
        };
        let finding = recorder.observe_github(&pr, "abc").await.unwrap().unwrap();
        assert!(
            finding.warning.contains("CI / test ran 11m 00s"),
            "{}",
            finding.warning
        );
        assert!(finding.warning.contains("5m 00s"), "{}", finding.warning);
        assert!(
            finding.warning.contains("sample tests::slow_index_rebuild"),
            "{}",
            finding.warning
        );
        let alerts = db.take_ci_alerts("s").await.unwrap();
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].contains("ci-optimise"));
        // The same commit observed again is not a new event.
        assert_eq!(recorder.observe_github(&pr, "abc").await.unwrap(), None);
        assert_eq!(db.take_ci_alerts("s").await.unwrap(), [] as [String; 0]);
        let jobs = db.ci_jobs_for_pr(pr.id).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            db.ci_tests(jobs[0].id).await.unwrap()[0].name,
            "sample tests::slow_index_rebuild"
        );
        assert_eq!(db.ci_steps(jobs[0].id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_job_past_the_absolute_limit_makes_a_finding_even_with_no_baseline() {
        let (_dir, db, pr) = setup().await;
        let mut actions = fake(41 * 60);
        actions.history.clear();
        let recorder = Recorder {
            db: &db,
            actions: &actions,
            limits: limits(),
        };
        let finding = recorder.observe_github(&pr, "abc").await.unwrap().unwrap();
        assert!(
            finding.warning.contains("past the 40m 00s limit"),
            "{}",
            finding.warning
        );
    }

    #[tokio::test]
    async fn a_normal_run_makes_no_finding() {
        let (_dir, db, pr) = setup().await;
        let actions = fake(9 * 60);
        let recorder = Recorder {
            db: &db,
            actions: &actions,
            limits: limits(),
        };
        assert_eq!(recorder.observe_github(&pr, "abc").await.unwrap(), None);
        assert_eq!(db.take_ci_alerts("s").await.unwrap(), [] as [String; 0]);
        assert_eq!(db.ci_jobs_for_pr(pr.id).await.unwrap().len(), 1);
        let baseline = db
            .ci_baseline(pr.project_id, "CI", "test")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(baseline.median_secs, 300);
    }

    #[tokio::test]
    async fn a_local_check_is_judged_against_the_last_local_runs() {
        let (_dir, db, pr) = setup().await;
        let actions = fake(0);
        let recorder = Recorder {
            db: &db,
            actions: &actions,
            limits: limits(),
        };
        let at: jiff::Timestamp = "2026-11-06T10:00:00Z".parse().unwrap();
        for i in 0..3 {
            let started = at + jiff::SignedDuration::from_secs(i * 1000);
            let found = recorder
                .observe_local(
                    &pr,
                    "abc",
                    "cargo test",
                    started,
                    Duration::from_secs(60),
                    true,
                )
                .await
                .unwrap();
            assert!(found.is_none());
        }
        let started = at + jiff::SignedDuration::from_secs(9000);
        let found = recorder
            .observe_local(
                &pr,
                "abc",
                "cargo test",
                started,
                Duration::from_secs(150),
                true,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            found.warning.contains("local / cargo test ran 2m 30s"),
            "{}",
            found.warning
        );
    }
}
