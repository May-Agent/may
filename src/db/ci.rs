//! CI durations on [`ChatDb`]: every job a PR's check runs took, its steps
//! and tests, the baselines jobs are judged against, the merge-queue wait,
//! and the findings raised when a job ran abnormally long.
//!
//! Jobs belong to the `issue_projects` row of their repository, like PRs, so
//! a repository's history outlives the sessions that recorded it.

use super::common::I64ValueRow;
use super::{ChatDb, ChatDbError, query};

/// One `ci_jobs` row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct CiJobRow {
    /// Surrogate id.
    pub id: i64,
    /// Repository the job ran for.
    pub project_id: i64,
    /// PR whose checks it was part of.
    pub pr_id: i64,
    /// Session that recorded it.
    pub session_id: String,
    /// `github` or `local`.
    pub source: String,
    /// The Actions job id; the start time in milliseconds for a local check.
    pub external_id: i64,
    /// The Actions workflow run id; 0 for a local check.
    pub run_id: i64,
    /// Workflow name; `local` for a local check.
    pub workflow: String,
    /// Job name, or the local check command.
    pub name: String,
    /// `success`, `failure`, `cancelled` and so on.
    pub conclusion: String,
    /// Start time, Unix milliseconds.
    pub started_at_ms: i64,
    /// Wall-clock duration.
    pub duration_secs: i64,
    /// When May recorded it, Unix milliseconds.
    pub recorded_at_ms: i64,
}

/// One `ci_steps` row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct CiStepRow {
    /// Owning job.
    pub job_id: i64,
    /// Position in the job.
    pub ordinal: i64,
    /// Step name.
    pub name: String,
    /// Wall-clock duration.
    pub duration_secs: i64,
}

/// One `ci_tests` row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct CiTestRow {
    /// Owning job.
    pub job_id: i64,
    /// Position in the log.
    pub ordinal: i64,
    /// `test`, `binary` or `running_over`.
    pub kind: String,
    /// Test or test binary name.
    pub name: String,
    /// Duration in milliseconds; for `running_over`, the lower bound.
    pub duration_ms: i64,
}

/// One `ci_baselines` row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct CiBaselineRow {
    /// Repository.
    pub project_id: i64,
    /// Workflow name.
    pub workflow: String,
    /// Job name.
    pub name: String,
    /// Median duration of the job's last successful base-branch runs.
    pub median_secs: i64,
    /// How many runs the median is taken over.
    pub samples: i64,
    /// When it was computed, Unix milliseconds.
    pub updated_at_ms: i64,
}

/// One `ci_pr_runs` row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct CiPrRunRow {
    /// The PR.
    pub pr_id: i64,
    /// Time from queueing to landing.
    pub queue_wait_secs: i64,
    /// The longest job of the PR's checks.
    pub run_secs: i64,
    /// When it was recorded, Unix milliseconds.
    pub recorded_at_ms: i64,
}

/// One `ci_findings` row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct CiFindingRow {
    /// Surrogate id, increasing in time.
    pub id: i64,
    /// Session whose PR was checked.
    pub session_id: String,
    /// The PR.
    pub pr_id: i64,
    /// The commit that was checked.
    pub head_sha: String,
    /// The text shown to the user.
    pub warning: String,
    /// The text the agent is told.
    pub alert: String,
    /// Whether the agent has been told.
    pub alerted: bool,
    /// When the finding was made, Unix milliseconds.
    pub created_at_ms: i64,
}

/// A job to record, with its steps and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCiJob {
    /// Repository.
    pub project_id: i64,
    /// PR whose checks it was part of.
    pub pr_id: i64,
    /// Session recording it.
    pub session_id: String,
    /// `github` or `local`.
    pub source: String,
    /// See [`CiJobRow::external_id`].
    pub external_id: i64,
    /// See [`CiJobRow::run_id`].
    pub run_id: i64,
    /// Workflow name.
    pub workflow: String,
    /// Job name.
    pub name: String,
    /// How it ended.
    pub conclusion: String,
    /// Start time, Unix milliseconds.
    pub started_at_ms: i64,
    /// Wall-clock duration.
    pub duration_secs: i64,
    /// `(name, duration_secs)` per step, in order.
    pub steps: Vec<(String, i64)>,
    /// `(kind, name, duration_ms)` per test, in log order.
    pub tests: Vec<(String, String, i64)>,
}

impl ChatDb {
    /// Records a job with its steps and tests, replacing the same job
    /// recorded before.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn record_ci_job(&self, job: &NewCiJob) -> Result<i64, ChatDbError> {
        let now = jiff::Timestamp::now().as_millisecond();
        let mut tx = self.db.begin().await?;
        let old = query!(
            tx,
            "SELECT id AS value FROM ci_jobs WHERE project_id = ? AND source = ? AND external_id = ?",
            job.project_id,
            job.source.clone(),
            job.external_id
        )
        .fetch_optional::<I64ValueRow>()
        .await?;
        if let Some(old) = old {
            query!(tx, "DELETE FROM ci_steps WHERE job_id = ?", old.value)
                .execute()
                .await?;
            query!(tx, "DELETE FROM ci_tests WHERE job_id = ?", old.value)
                .execute()
                .await?;
            query!(tx, "DELETE FROM ci_jobs WHERE id = ?", old.value)
                .execute()
                .await?;
        }
        query!(
            tx,
            "INSERT INTO ci_jobs (project_id, pr_id, session_id, source, external_id, run_id, workflow, name, conclusion, started_at_ms, duration_secs, recorded_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            job.project_id,
            job.pr_id,
            job.session_id.clone(),
            job.source.clone(),
            job.external_id,
            job.run_id,
            job.workflow.clone(),
            job.name.clone(),
            job.conclusion.clone(),
            job.started_at_ms,
            job.duration_secs,
            now
        )
        .execute()
        .await?;
        let id = query!(tx, "SELECT last_insert_rowid() AS value")
            .fetch_one::<I64ValueRow>()
            .await?
            .value;
        for (ordinal, (name, secs)) in (0_i64..).zip(&job.steps) {
            query!(
                tx,
                "INSERT INTO ci_steps (job_id, ordinal, name, duration_secs) VALUES (?, ?, ?, ?)",
                id,
                ordinal,
                name.clone(),
                *secs
            )
            .execute()
            .await?;
        }
        for (ordinal, (kind, name, millis)) in (0_i64..).zip(&job.tests) {
            query!(
                tx,
                "INSERT INTO ci_tests (job_id, ordinal, kind, name, duration_ms) VALUES (?, ?, ?, ?, ?)",
                id,
                ordinal,
                kind.clone(),
                name.clone(),
                *millis
            )
            .execute()
            .await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    /// The jobs recorded for a PR, slowest first.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn ci_jobs_for_pr(&self, pr_id: i64) -> Result<Vec<CiJobRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM ci_jobs WHERE pr_id = ? ORDER BY duration_secs DESC, id",
            pr_id
        )
        .fetch_all::<CiJobRow>()
        .await?)
    }

    /// The jobs recorded for a workflow run, slowest first.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn ci_jobs_for_run(&self, run_id: i64) -> Result<Vec<CiJobRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM ci_jobs WHERE run_id = ? AND source = 'github' ORDER BY duration_secs DESC, id",
            run_id
        )
        .fetch_all::<CiJobRow>()
        .await?)
    }

    /// A job's steps in order.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn ci_steps(&self, job_id: i64) -> Result<Vec<CiStepRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM ci_steps WHERE job_id = ? ORDER BY ordinal",
            job_id
        )
        .fetch_all::<CiStepRow>()
        .await?)
    }

    /// A job's tests, slowest first.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn ci_tests(&self, job_id: i64) -> Result<Vec<CiTestRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM ci_tests WHERE job_id = ? ORDER BY duration_ms DESC, ordinal",
            job_id
        )
        .fetch_all::<CiTestRow>()
        .await?)
    }

    /// The newest jobs of one name in a repository, newest first.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn ci_job_history(
        &self,
        project_id: i64,
        workflow: &str,
        name: &str,
        limit: i64,
    ) -> Result<Vec<CiJobRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM ci_jobs WHERE project_id = ? AND workflow = ? AND name = ? ORDER BY started_at_ms DESC LIMIT ?",
            project_id,
            workflow,
            name,
            limit
        )
        .fetch_all::<CiJobRow>()
        .await?)
    }

    /// Every job recorded for the repositories a session's PRs belong to,
    /// newest first.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn ci_jobs_for_session_repos(
        &self,
        session_id: &str,
    ) -> Result<Vec<CiJobRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM ci_jobs WHERE project_id IN (SELECT project_id FROM prs WHERE session_id = ?) ORDER BY started_at_ms DESC",
            session_id
        )
        .fetch_all::<CiJobRow>()
        .await?)
    }

    /// Stores a job's baseline, replacing the previous one.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn set_ci_baseline(
        &self,
        project_id: i64,
        workflow: &str,
        name: &str,
        median_secs: i64,
        samples: i64,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().as_millisecond();
        query!(
            self.db,
            "INSERT INTO ci_baselines (project_id, workflow, name, median_secs, samples, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (project_id, workflow, name) DO UPDATE SET median_secs = excluded.median_secs, samples = excluded.samples, updated_at_ms = excluded.updated_at_ms",
            project_id,
            workflow,
            name,
            median_secs,
            samples,
            now
        )
        .execute()
        .await?;
        Ok(())
    }

    /// A job's baseline.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn ci_baseline(
        &self,
        project_id: i64,
        workflow: &str,
        name: &str,
    ) -> Result<Option<CiBaselineRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM ci_baselines WHERE project_id = ? AND workflow = ? AND name = ?",
            project_id,
            workflow,
            name
        )
        .fetch_optional::<CiBaselineRow>()
        .await?)
    }

    /// Records how long a PR waited in the merge queue against its run time.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn record_ci_pr_run(
        &self,
        pr_id: i64,
        queue_wait_secs: i64,
        run_secs: i64,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().as_millisecond();
        query!(
            self.db,
            "INSERT OR REPLACE INTO ci_pr_runs (pr_id, queue_wait_secs, run_secs, recorded_at_ms) VALUES (?, ?, ?, ?)",
            pr_id,
            queue_wait_secs,
            run_secs,
            now
        )
        .execute()
        .await?;
        Ok(())
    }

    /// The queue waits recorded for a session's PRs, newest first.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn ci_pr_runs_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<CiPrRunRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM ci_pr_runs WHERE pr_id IN (SELECT id FROM prs WHERE session_id = ?) ORDER BY recorded_at_ms DESC",
            session_id
        )
        .fetch_all::<CiPrRunRow>()
        .await?)
    }

    /// Stores a finding unless the PR's commit already has one; whether this
    /// call stored it.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn add_ci_finding(
        &self,
        session_id: &str,
        pr_id: i64,
        head_sha: &str,
        warning: &str,
        alert: &str,
    ) -> Result<bool, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let before = query!(tx, "SELECT COUNT(*) AS value FROM ci_findings")
            .fetch_one::<I64ValueRow>()
            .await?
            .value;
        let now = jiff::Timestamp::now().as_millisecond();
        query!(
            tx,
            "INSERT OR IGNORE INTO ci_findings (session_id, pr_id, head_sha, warning, alert, created_at_ms) VALUES (?, ?, ?, ?, ?, ?)",
            session_id,
            pr_id,
            head_sha,
            warning,
            alert,
            now
        )
        .execute()
        .await?;
        let after = query!(tx, "SELECT COUNT(*) AS value FROM ci_findings")
            .fetch_one::<I64ValueRow>()
            .await?
            .value;
        tx.commit().await?;
        Ok(after > before)
    }

    /// The alerts the session's agent has not been told, oldest first, marked
    /// as told: each finding is returned by exactly one call.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn take_ci_alerts(&self, session_id: &str) -> Result<Vec<String>, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let rows = query!(
            tx,
            "SELECT * FROM ci_findings WHERE session_id = ? AND alerted = 0 ORDER BY id",
            session_id
        )
        .fetch_all::<CiFindingRow>()
        .await?;
        query!(
            tx,
            "UPDATE ci_findings SET alerted = 1 WHERE session_id = ? AND alerted = 0",
            session_id
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(|row| row.alert).collect())
    }

    /// The session's findings after `after`, oldest first.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn ci_findings_after(
        &self,
        session_id: &str,
        after: i64,
    ) -> Result<Vec<CiFindingRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM ci_findings WHERE session_id = ? AND id > ? ORDER BY id",
            session_id,
            after
        )
        .fetch_all::<CiFindingRow>()
        .await?)
    }

    /// The newest finding id of the session; 0 when it has none.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn latest_ci_finding_id(&self, session_id: &str) -> Result<i64, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT COALESCE(MAX(id), 0) AS value FROM ci_findings WHERE session_id = ?",
            session_id
        )
        .fetch_one::<I64ValueRow>()
        .await?
        .value)
    }

    /// The time of a PR's first event of `kind`, when it has one.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn pr_event_time(
        &self,
        pr_id: i64,
        kind: &str,
    ) -> Result<Option<jiff::Timestamp>, ChatDbError> {
        Ok(self
            .pr_events(pr_id)
            .await?
            .into_iter()
            .find(|event| event.kind == kind)
            .and_then(|event| event.created_at.parse().ok()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open() -> (tempfile::TempDir, ChatDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = ChatDb::open(&dir.path().join("history.db")).await.unwrap();
        (dir, db)
    }

    fn job(external_id: i64, secs: i64) -> NewCiJob {
        NewCiJob {
            project_id: 1,
            pr_id: 1,
            session_id: "s".into(),
            source: "github".into(),
            external_id,
            run_id: 7,
            workflow: "CI".into(),
            name: "test".into(),
            conclusion: "success".into(),
            started_at_ms: external_id,
            duration_secs: secs,
            steps: vec![("build".into(), 10), ("test".into(), secs - 10)],
            tests: vec![("test".into(), "a::b".into(), 1500)],
        }
    }

    #[tokio::test]
    async fn a_job_is_recorded_once_with_its_steps_and_tests() {
        let (_dir, db) = open().await;
        let id = db.record_ci_job(&job(5, 100)).await.unwrap();
        let again = db.record_ci_job(&job(5, 120)).await.unwrap();
        assert_ne!(id, again);
        let jobs = db.ci_jobs_for_pr(1).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].duration_secs, 120);
        assert_eq!(db.ci_steps(again).await.unwrap().len(), 2);
        assert_eq!(db.ci_tests(again).await.unwrap()[0].name, "a::b");
        assert_eq!(db.ci_steps(id).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn a_finding_is_stored_once_and_told_once() {
        let (_dir, db) = open().await;
        assert!(db.add_ci_finding("s", 1, "abc", "w", "a").await.unwrap());
        assert!(!db.add_ci_finding("s", 1, "abc", "w2", "a2").await.unwrap());
        assert_eq!(db.take_ci_alerts("s").await.unwrap(), ["a"]);
        assert!(db.take_ci_alerts("s").await.unwrap().is_empty());
        assert_eq!(db.latest_ci_finding_id("s").await.unwrap(), 1);
        assert_eq!(db.ci_findings_after("s", 1).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn a_baseline_is_replaced() {
        let (_dir, db) = open().await;
        db.set_ci_baseline(1, "CI", "test", 100, 5).await.unwrap();
        db.set_ci_baseline(1, "CI", "test", 130, 6).await.unwrap();
        let row = db.ci_baseline(1, "CI", "test").await.unwrap().unwrap();
        assert_eq!((row.median_secs, row.samples), (130, 6));
    }
}
