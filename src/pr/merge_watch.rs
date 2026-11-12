//! The watch `pr merge` runs on a GitHub PR after it is queued: follow the PR
//! until it is merged, closed, removed from the merge queue or fails a
//! required check.
//!
//! GitHub answers through typed (serde) responses; the wait is an async
//! timer, so nothing holds a thread while the PR sits in the queue. The bound
//! is not a fixed duration: it is the longest recent successful run of the
//! repository's active workflows, scaled by the stages the PR still has to
//! pass (its own checks, then the merge queue's group run behind the entries
//! ahead of it).

use std::future::Future;
use std::time::Duration;

use askama::Template;
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::Deserialize;
use zenwave::ResponseExt;

use super::PrError;
use crate::issue::github::{GitHubRest, GitHubTokenSource};

/// The GraphQL query that reads everything the watch decides on.
const QUERY: &str = include_str!("merge_watch.graphql");

/// Polls per expected check duration.
const POLLS_PER_EXPECTED: u32 = 20;

/// Recent successful runs of a workflow read to derive its duration.
const RUNS_PER_WORKFLOW: u32 = 20;

/// Workflows whose history is fetched at once.
const WORKFLOW_FETCH_CONCURRENCY: usize = 4;

/// Time as the watch sees it: injectable so tests drive 30 minutes of queue
/// without waiting for them.
pub(super) trait Clock {
    /// Time elapsed on this clock since it was created.
    fn elapsed(&self) -> Duration;
    /// Suspends the caller for `duration` without holding a thread.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send;
}

/// The wall clock.
pub(super) struct SystemClock {
    start: std::time::Instant,
}

impl SystemClock {
    pub(super) fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    async fn sleep(&self, duration: Duration) {
        async_io::Timer::after(duration).await;
    }
}

/// What the watch reads from GitHub.
pub trait Upstream {
    /// The PR as GitHub reports it now.
    fn snapshot(&self) -> impl Future<Output = Result<Snapshot, PrError>> + Send;
    /// How long the repository's checks usually take.
    fn expected_checks(&self) -> impl Future<Output = Result<Duration, PrError>> + Send;
}

/// The PR's state on GitHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

/// How a finished check run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckConclusion {
    Success,
    Failure,
    TimedOut,
    Cancelled,
    StartupFailure,
    #[serde(other)]
    Other,
}

/// The state of a commit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StatusState {
    Failure,
    Error,
    Pending,
    Success,
    /// `EXPECTED` — GitHub waits on a required status it has not seen.
    Expected,
    #[serde(other)]
    Other,
}

/// One check on the PR's head commit.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "__typename")]
pub enum CheckContext {
    CheckRun {
        name: String,
        #[serde(rename = "detailsUrl")]
        details_url: Option<String>,
        status: Option<RunStatus>,
        #[serde(rename = "startedAt")]
        started_at: Option<jiff::Timestamp>,
        #[serde(rename = "completedAt")]
        completed_at: Option<jiff::Timestamp>,
        conclusion: Option<CheckConclusion>,
        #[serde(rename = "isRequired")]
        is_required: bool,
    },
    StatusContext {
        context: String,
        #[serde(rename = "targetUrl")]
        target_url: Option<String>,
        state: StatusState,
        #[serde(rename = "createdAt")]
        created_at: Option<jiff::Timestamp>,
        #[serde(rename = "isRequired")]
        is_required: bool,
    },
}

/// Where a check run is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunStatus {
    InProgress,
    Completed,
    #[serde(other)]
    Queued,
}

/// A required check that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedCheck {
    pub(crate) name: String,
    pub(crate) url: Option<String>,
}

impl CheckContext {
    fn failed_required(&self) -> Option<FailedCheck> {
        match self {
            Self::CheckRun {
                name,
                details_url,
                conclusion:
                    Some(
                        CheckConclusion::Failure
                        | CheckConclusion::TimedOut
                        | CheckConclusion::Cancelled
                        | CheckConclusion::StartupFailure,
                    ),
                is_required: true,
                ..
            } => Some(FailedCheck {
                name: name.clone(),
                url: details_url.clone(),
            }),
            Self::StatusContext {
                context,
                target_url,
                state: StatusState::Failure | StatusState::Error,
                is_required: true,
                ..
            } => Some(FailedCheck {
                name: context.clone(),
                url: target_url.clone(),
            }),
            _ => None,
        }
    }
}

/// What the watch decides on, read from one GitHub response.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub(crate) state: PullRequestState,
    /// The PR's place in the merge queue, `None` when it is not queued.
    pub(crate) queue_position: Option<u32>,
    /// How many times the PR has been removed from a merge queue.
    pub(crate) removals: usize,
    /// The reason GitHub gave for the latest removal.
    pub(crate) last_removal: Option<String>,
    /// The commit the checks ran on.
    pub(crate) head_sha: String,
    pub(crate) checks: Vec<CheckContext>,
}

impl Snapshot {
    pub(crate) fn failed_check(&self) -> Option<FailedCheck> {
        self.checks.iter().find_map(CheckContext::failed_required)
    }
}

/// How many of the PR's checks sit in each state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckSummary {
    /// Still running or not yet reported.
    pub(crate) pending: u32,
    /// Concluded successfully (neutral and skipped included).
    pub(crate) passed: u32,
    /// Failed, timed out, cancelled or errored.
    pub(crate) failed: u32,
}

/// Why the watch ended without a merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Stopped {
    /// The PR was closed upstream without merging.
    Closed,
    /// A required check failed.
    CheckFailed(FailedCheck),
    /// The merge queue removed the PR, for the reason GitHub gave.
    Removed(String),
    /// The PR was still open when the bound passed.
    TimedOut { waited: Duration, bound: Duration },
}

/// How the watch ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    Merged,
    Stopped(Stopped),
}

/// The text the agent reads when a watched PR did not land.
#[derive(Template)]
#[template(path = "pr_watch_stopped.txt", escape = "none")]
pub(super) struct StoppedReport<'a> {
    pub(super) number: i64,
    pub(super) stopped: &'a Stopped,
    waited_minutes: u64,
    bound_minutes: u64,
}

impl<'a> StoppedReport<'a> {
    pub(super) const fn new(number: i64, stopped: &'a Stopped) -> Self {
        let (waited, bound) = match stopped {
            Stopped::TimedOut { waited, bound } => (*waited, *bound),
            _ => (Duration::ZERO, Duration::ZERO),
        };
        Self {
            number,
            stopped,
            waited_minutes: waited.as_secs() / 60,
            bound_minutes: bound.as_secs() / 60,
        }
    }
}

/// Follows the PR until it is merged, closed, removed from the merge queue
/// or fails a required check.
///
/// `expected` is how long the PR's checks usually take; the watch gives up
/// after that, times the stages ahead of the PR (its own checks, the queue's
/// group run, and each entry ahead of it in the queue), and polls
/// [`POLLS_PER_EXPECTED`] times per `expected`. `removals` is the number of
/// queue removals the PR had before it was queued.
///
/// # Errors
/// Fails when GitHub cannot be read.
pub(super) async fn watch<U: Upstream + Sync, C: Clock + Sync>(
    upstream: &U,
    clock: &C,
    removals: usize,
    expected: Duration,
) -> Result<Outcome, PrError> {
    let started = clock.elapsed();
    loop {
        let snapshot = upstream.snapshot().await?;
        match snapshot.state {
            PullRequestState::Merged => return Ok(Outcome::Merged),
            PullRequestState::Closed => return Ok(Outcome::Stopped(Stopped::Closed)),
            PullRequestState::Open => {}
        }
        if let Some(check) = snapshot.failed_check() {
            return Ok(Outcome::Stopped(Stopped::CheckFailed(check)));
        }
        if snapshot.removals > removals {
            let reason = snapshot
                .last_removal
                .unwrap_or_else(|| "no reason given".to_string());
            return Ok(Outcome::Stopped(Stopped::Removed(reason)));
        }
        let bound = expected * (2 + snapshot.queue_position.unwrap_or(0));
        let waited = clock.elapsed().saturating_sub(started);
        if waited >= bound {
            return Ok(Outcome::Stopped(Stopped::TimedOut { waited, bound }));
        }
        clock.sleep(expected / POLLS_PER_EXPECTED).await;
    }
}

/// Told once, the first time every check run of the PR has concluded.
pub trait ChecksObserver: Sync {
    /// The PR's checks have finished.
    fn finished(&self) -> impl Future<Output = ()> + Send;
}

/// An [`Upstream`] that tells `observer` when the PR's checks finish.
pub(super) struct Observing<'a, U, O> {
    upstream: &'a U,
    observer: &'a O,
    told: std::sync::atomic::AtomicBool,
}

impl<'a, U: Sync, O: ChecksObserver> Observing<'a, U, O> {
    pub(super) const fn new(upstream: &'a U, observer: &'a O) -> Self {
        Self {
            upstream,
            observer,
            told: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Tells the observer now unless it was told already: the watch ended
    /// before a snapshot showed the checks finished.
    pub(super) async fn tell_if_unheard(&self) {
        if !self.told.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.observer.finished().await;
        }
    }
}

impl Snapshot {
    /// Whether the PR has check runs and every one has concluded.
    fn checks_finished(&self) -> bool {
        let mut runs = self.checks.iter().filter_map(|check| match check {
            CheckContext::CheckRun { conclusion, .. } => Some(conclusion.is_some()),
            CheckContext::StatusContext { .. } => None,
        });
        let first = runs.next();
        first == Some(true) && runs.all(|concluded| concluded)
    }
}

impl<U: Upstream + Sync, O: ChecksObserver> Upstream for Observing<'_, U, O> {
    async fn snapshot(&self) -> Result<Snapshot, PrError> {
        let snapshot = self.upstream.snapshot().await?;
        if snapshot.checks_finished() {
            self.tell_if_unheard().await;
        }
        Ok(snapshot)
    }

    fn expected_checks(&self) -> impl Future<Output = Result<Duration, PrError>> + Send {
        self.upstream.expected_checks()
    }
}

/// GitHub through its API, for one PR.
pub struct GithubPr<'a, T> {
    pub(crate) api: &'a GitHubRest<T>,
    pub(crate) owner_repo: &'a str,
    pub(crate) number: i64,
}

impl<T: GitHubTokenSource + Sync> GithubPr<'_, T> {
    async fn get<R: serde::de::DeserializeOwned>(&self, url: String) -> Result<R, PrError> {
        self.api
            .request(zenwave::Method::GET, url, None)
            .await
            .map_err(|e| PrError::Gh(e.to_string()))?
            .into_json::<R>()
            .await
            .map_err(|e| PrError::Gh(e.to_string()))
    }

    /// The recent successful runs of one workflow, as durations.
    async fn workflow_durations(&self, workflow_id: u64) -> Result<Vec<Duration>, PrError> {
        let mut url = url::Url::parse(&format!(
            "{}/repos/{}/actions/workflows/{workflow_id}/runs",
            self.api.api_base(),
            self.owner_repo
        ))
        .map_err(|e| PrError::Gh(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("status", "success")
            .append_pair("per_page", &RUNS_PER_WORKFLOW.to_string());
        let runs: WorkflowRuns = self.get(url.into()).await?;
        Ok(runs
            .workflow_runs
            .iter()
            .filter_map(WorkflowRun::duration)
            .collect())
    }
}

impl<T: GitHubTokenSource + Sync> Upstream for GithubPr<'_, T> {
    async fn snapshot(&self) -> Result<Snapshot, PrError> {
        let (owner, name) = self
            .owner_repo
            .split_once('/')
            .ok_or_else(|| PrError::Resolve(format!("not an owner/repo: {}", self.owner_repo)))?;
        // The checks connection pages: follow it until every check is read.
        let mut snapshot: Option<Snapshot> = None;
        let mut after: Option<String> = None;
        loop {
            let body = serde_json::json!({
                "query": QUERY,
                "variables": {
                    "owner": owner,
                    "name": name,
                    "number": self.number,
                    "after": after,
                },
            });
            let response: GraphResponse = self
                .api
                .request(
                    zenwave::Method::POST,
                    format!("{}/graphql", self.api.api_base()),
                    Some(body),
                )
                .await
                .map_err(|e| PrError::Gh(e.to_string()))?
                .into_json()
                .await
                .map_err(|e| PrError::Gh(e.to_string()))?;
            let (page, next) = response.into_page(self.number)?;
            match snapshot.as_mut() {
                Some(snapshot) => snapshot.checks.extend(page.checks),
                None => snapshot = Some(page),
            }
            match next {
                Some(cursor) => after = Some(cursor),
                None => return Ok(snapshot.expect("a page was read")),
            }
        }
    }

    async fn expected_checks(&self) -> Result<Duration, PrError> {
        let url = format!(
            "{}/repos/{}/actions/workflows?per_page=100",
            self.api.api_base(),
            self.owner_repo
        );
        let workflows: Workflows = self.get(url).await?;
        let durations: Vec<Vec<Duration>> = stream::iter(workflows.active_ids())
            .map(|id| self.workflow_durations(id))
            .buffered(WORKFLOW_FETCH_CONCURRENCY)
            .try_collect()
            .await?;
        slowest(&durations).ok_or_else(|| {
            PrError::State(format!(
                "no active workflow of {} has a successful run, so there is no way to tell how \
                 long PR #{}'s checks take",
                self.owner_repo, self.number
            ))
        })
    }
}

/// The workflows run side by side, so the checks take as long as the slowest
/// run of any of them has taken.
fn slowest(durations: &[Vec<Duration>]) -> Option<Duration> {
    durations
        .iter()
        .flatten()
        .copied()
        .filter(|d| !d.is_zero())
        .max()
}

#[derive(Deserialize)]
struct Workflows {
    workflows: Vec<Workflow>,
}

#[derive(Deserialize)]
struct Workflow {
    id: u64,
    state: WorkflowState,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WorkflowState {
    Active,
    #[serde(other)]
    Other,
}

impl Workflows {
    fn active_ids(&self) -> Vec<u64> {
        self.workflows
            .iter()
            .filter(|w| w.state == WorkflowState::Active)
            .map(|w| w.id)
            .collect()
    }
}

#[derive(Deserialize)]
struct WorkflowRuns {
    workflow_runs: Vec<WorkflowRun>,
}

#[derive(Deserialize)]
struct WorkflowRun {
    run_started_at: Option<jiff::Timestamp>,
    updated_at: jiff::Timestamp,
}

impl WorkflowRun {
    fn duration(&self) -> Option<Duration> {
        let span = self.updated_at.duration_since(self.run_started_at?);
        Duration::try_from(span).ok()
    }
}

#[derive(Deserialize)]
struct GraphResponse {
    data: Option<GraphData>,
    #[serde(default)]
    errors: Vec<GraphError>,
}

#[derive(Deserialize)]
struct GraphError {
    message: String,
}

#[derive(Deserialize)]
struct GraphData {
    repository: GraphRepository,
}

#[derive(Deserialize)]
struct GraphRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<GraphPullRequest>,
}

#[derive(Deserialize)]
struct GraphPullRequest {
    state: PullRequestState,
    #[serde(rename = "mergeQueueEntry")]
    merge_queue_entry: Option<GraphQueueEntry>,
    #[serde(rename = "timelineItems")]
    timeline_items: GraphRemovals,
    commits: GraphCommits,
}

#[derive(Deserialize)]
struct GraphQueueEntry {
    position: u32,
}

#[derive(Deserialize)]
struct GraphRemovals {
    #[serde(rename = "totalCount")]
    total_count: usize,
    nodes: Vec<GraphRemoval>,
}

#[derive(Deserialize)]
struct GraphRemoval {
    reason: Option<String>,
}

#[derive(Deserialize)]
struct GraphCommits {
    nodes: Vec<GraphCommitNode>,
}

#[derive(Deserialize)]
struct GraphCommitNode {
    commit: GraphCommit,
}

#[derive(Deserialize)]
struct GraphCommit {
    oid: String,
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Option<GraphRollup>,
}

#[derive(Deserialize)]
struct GraphRollup {
    contexts: GraphContexts,
}

#[derive(Deserialize)]
struct GraphContexts {
    #[serde(rename = "pageInfo")]
    page_info: GraphPageInfo,
    nodes: Vec<CheckContext>,
}

#[derive(Deserialize)]
struct GraphPageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

impl GraphResponse {
    /// One page of the PR: its snapshot with that page's checks, and the
    /// cursor of the next page of checks when there is one.
    fn into_page(self, number: i64) -> Result<(Snapshot, Option<String>), PrError> {
        if !self.errors.is_empty() {
            let messages: Vec<&str> = self.errors.iter().map(|e| e.message.as_str()).collect();
            return Err(PrError::Gh(format!(
                "GraphQL error reading PR #{number}: {}",
                messages.join("; ")
            )));
        }
        let pr = self
            .data
            .and_then(|data| data.repository.pull_request)
            .ok_or_else(|| PrError::Gh(format!("GitHub has no PR #{number}")))?;
        let commit = pr.commits.nodes.into_iter().next().map(|node| node.commit);
        let head_sha = commit
            .as_ref()
            .map(|commit| commit.oid.clone())
            .ok_or_else(|| PrError::Gh(format!("GitHub reports no commit for PR #{number}")))?;
        let contexts = commit
            .and_then(|commit| commit.status_check_rollup)
            .map(|rollup| rollup.contexts);
        let next = contexts
            .as_ref()
            .filter(|c| c.page_info.has_next_page)
            .and_then(|c| c.page_info.end_cursor.clone());
        let checks = contexts.map(|c| c.nodes).unwrap_or_default();
        let snapshot = Snapshot {
            state: pr.state,
            queue_position: pr.merge_queue_entry.map(|entry| entry.position),
            removals: pr.timeline_items.total_count,
            last_removal: pr
                .timeline_items
                .nodes
                .into_iter()
                .next_back()
                .and_then(|node| node.reason),
            head_sha,
            checks,
        };
        Ok((snapshot, next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::thread::ThreadId;

    /// A clock that jumps over every sleep.
    struct VirtualClock {
        now: Mutex<Duration>,
        sleepers: Mutex<Vec<ThreadId>>,
    }

    impl VirtualClock {
        fn new() -> Self {
            Self {
                now: Mutex::new(Duration::ZERO),
                sleepers: Mutex::new(Vec::new()),
            }
        }
    }

    impl Clock for VirtualClock {
        fn elapsed(&self) -> Duration {
            *self.now.lock().unwrap()
        }

        async fn sleep(&self, duration: Duration) {
            self.sleepers
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            *self.now.lock().unwrap() += duration;
        }
    }

    /// GitHub as a script: the snapshot in force at each moment.
    struct Script<'a> {
        clock: &'a VirtualClock,
        steps: Vec<(Duration, Snapshot)>,
        readers: Mutex<Vec<ThreadId>>,
    }

    impl Upstream for Script<'_> {
        fn snapshot(&self) -> impl Future<Output = Result<Snapshot, PrError>> + Send {
            self.readers
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            let now = self.clock.elapsed();
            std::future::ready(Ok(self
                .steps
                .iter()
                .rev()
                .find(|(at, _)| *at <= now)
                .expect("a step at zero")
                .1
                .clone()))
        }

        async fn expected_checks(&self) -> Result<Duration, PrError> {
            std::future::ready(()).await;
            unreachable!("the watch is given the expected duration")
        }
    }

    const EXPECTED: Duration = Duration::from_mins(12);

    fn minutes(m: u64) -> Duration {
        Duration::from_secs(m * 60)
    }

    fn snapshot(state: PullRequestState, position: Option<u32>) -> Snapshot {
        Snapshot {
            state,
            queue_position: position,
            removals: 0,
            last_removal: None,
            head_sha: "abc123".to_string(),
            checks: vec![CheckContext::CheckRun {
                name: "clippy".to_string(),
                details_url: Some("https://github.com/o/r/actions/runs/1/job/2".to_string()),
                status: Some(RunStatus::Completed),
                started_at: None,
                completed_at: None,
                conclusion: Some(CheckConclusion::Success),
                is_required: true,
            }],
        }
    }

    async fn run(steps: Vec<(Duration, Snapshot)>) -> (Outcome, VirtualClock, Vec<ThreadId>) {
        let clock = VirtualClock::new();
        let script = Script {
            clock: &clock,
            steps,
            readers: Mutex::new(Vec::new()),
        };
        let outcome = watch(&script, &clock, 0, EXPECTED).await.unwrap();
        let readers = script.readers.into_inner().unwrap();
        (outcome, clock, readers)
    }

    struct Counter(std::sync::atomic::AtomicUsize);

    impl ChecksObserver for Counter {
        fn finished(&self) -> impl Future<Output = ()> + Send {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(())
        }
    }

    #[test]
    fn the_observer_is_told_once_when_every_check_run_has_concluded() {
        let mut running = snapshot(PullRequestState::Open, None);
        running.checks.push(CheckContext::CheckRun {
            name: "test".to_string(),
            details_url: None,
            status: Some(RunStatus::InProgress),
            started_at: None,
            completed_at: None,
            conclusion: None,
            is_required: true,
        });
        let steps = vec![
            (Duration::ZERO, running),
            (minutes(2), snapshot(PullRequestState::Open, Some(1))),
            (minutes(30), snapshot(PullRequestState::Merged, None)),
        ];
        let clock = VirtualClock::new();
        let script = Script {
            clock: &clock,
            steps,
            readers: Mutex::new(Vec::new()),
        };
        let counter = Counter(std::sync::atomic::AtomicUsize::new(0));
        let observing = Observing::new(&script, &counter);
        let outcome =
            futures_lite::future::block_on(watch(&observing, &clock, 0, EXPECTED)).unwrap();
        futures_lite::future::block_on(observing.tell_if_unheard());
        assert_eq!(outcome, Outcome::Merged);
        assert_eq!(counter.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[test]
    fn a_pr_landing_after_thirty_minutes_in_the_merge_queue_is_merged() {
        let steps = vec![
            (Duration::ZERO, snapshot(PullRequestState::Open, None)),
            (minutes(4), snapshot(PullRequestState::Open, Some(3))),
            (minutes(30), snapshot(PullRequestState::Merged, None)),
        ];
        // No runtime and no blocking pool: the watch runs on this thread
        // alone, and every read and every sleep happens on it.
        let (outcome, clock, readers) = futures_lite::future::block_on(run(steps));
        assert_eq!(outcome, Outcome::Merged);
        assert!(clock.elapsed() >= minutes(30));
        let me = std::thread::current().id();
        assert_ne!(readers.len(), 0);
        assert!(readers.iter().all(|id| *id == me));
        assert!(clock.sleepers.lock().unwrap().iter().all(|id| *id == me));
    }

    #[test]
    fn a_failed_required_check_is_reported_with_its_name_and_url() {
        let mut failing = snapshot(PullRequestState::Open, None);
        failing.checks.push(CheckContext::CheckRun {
            name: "test (ubuntu-latest)".to_string(),
            details_url: Some("https://github.com/o/r/actions/runs/9/job/8".to_string()),
            status: Some(RunStatus::Completed),
            started_at: None,
            completed_at: None,
            conclusion: Some(CheckConclusion::Failure),
            is_required: true,
        });
        // A failed check that is not required does not stop the watch.
        failing.checks.push(CheckContext::CheckRun {
            name: "docs".to_string(),
            details_url: None,
            status: Some(RunStatus::Completed),
            started_at: None,
            completed_at: None,
            conclusion: Some(CheckConclusion::Failure),
            is_required: false,
        });
        let (outcome, _, _) = futures_lite::future::block_on(run(vec![
            (Duration::ZERO, snapshot(PullRequestState::Open, None)),
            (minutes(7), failing),
        ]));
        let stopped = Stopped::CheckFailed(FailedCheck {
            name: "test (ubuntu-latest)".to_string(),
            url: Some("https://github.com/o/r/actions/runs/9/job/8".to_string()),
        });
        assert_eq!(outcome, Outcome::Stopped(stopped.clone()));
        let text = StoppedReport::new(7, &stopped).render().unwrap();
        assert!(text.contains("test (ubuntu-latest)"), "{text}");
        assert!(
            text.contains("https://github.com/o/r/actions/runs/9/job/8"),
            "{text}"
        );
    }

    #[test]
    fn a_removal_from_the_merge_queue_is_reported_with_githubs_reason() {
        let mut removed = snapshot(PullRequestState::Open, None);
        removed.removals = 1;
        removed.last_removal = Some("failed status checks".to_string());
        let (outcome, _, _) = futures_lite::future::block_on(run(vec![
            (Duration::ZERO, snapshot(PullRequestState::Open, Some(1))),
            (minutes(20), removed),
        ]));
        let stopped = Stopped::Removed("failed status checks".to_string());
        assert_eq!(outcome, Outcome::Stopped(stopped.clone()));
        let text = StoppedReport::new(7, &stopped).render().unwrap();
        assert!(text.contains("failed status checks"), "{text}");
    }

    #[test]
    fn a_removal_before_the_pr_was_queued_is_not_a_removal() {
        let mut earlier = snapshot(PullRequestState::Open, Some(1));
        earlier.removals = 2;
        earlier.last_removal = Some("old".to_string());
        let clock = VirtualClock::new();
        let mut merged = earlier.clone();
        merged.state = PullRequestState::Merged;
        let script = Script {
            clock: &clock,
            steps: vec![(Duration::ZERO, earlier), (minutes(25), merged)],
            readers: Mutex::new(Vec::new()),
        };
        let outcome = futures_lite::future::block_on(watch(&script, &clock, 2, EXPECTED)).unwrap();
        assert_eq!(outcome, Outcome::Merged);
    }

    #[test]
    fn a_pr_still_open_past_the_bound_times_out_with_the_bound_reported() {
        let (outcome, _, _) = futures_lite::future::block_on(run(vec![(
            Duration::ZERO,
            snapshot(PullRequestState::Open, None),
        )]));
        // Unqueued: its own checks, then the queue's group run.
        let Outcome::Stopped(Stopped::TimedOut { waited, bound }) = outcome else {
            panic!("expected a timeout, got {outcome:?}");
        };
        assert_eq!(bound, EXPECTED * 2);
        assert!(waited >= bound);
        let text = StoppedReport::new(7, &Stopped::TimedOut { waited, bound })
            .render()
            .unwrap();
        assert!(text.contains("within 24"), "{text}");
    }

    #[test]
    fn a_closed_pr_is_reported_closed() {
        let (outcome, _, _) = futures_lite::future::block_on(run(vec![(
            Duration::ZERO,
            snapshot(PullRequestState::Closed, None),
        )]));
        assert_eq!(outcome, Outcome::Stopped(Stopped::Closed));
    }

    /// One page of the query's answer, with the given checks and paging.
    fn page(checks: &str, next: Option<&str>) -> GraphResponse {
        let page_info = next.map_or_else(
            || r#"{"hasNextPage":false,"endCursor":null}"#.to_string(),
            |cursor| format!(r#"{{"hasNextPage":true,"endCursor":"{cursor}"}}"#),
        );
        let answer = format!(
            r#"{{"data":{{"repository":{{"pullRequest":{{
            "state":"OPEN",
            "mergeQueueEntry":{{"position":2}},
            "timelineItems":{{"totalCount":1,"nodes":[
                {{"__typename":"RemovedFromMergeQueueEvent","reason":"merge conflict"}}]}},
            "commits":{{"nodes":[{{"commit":{{"oid":"abc123","statusCheckRollup":{{"contexts":{{
                "pageInfo":{page_info},"nodes":[{checks}]}}}}}}}}]}}}}}}}}}}"#
        );
        serde_json::from_str(&answer).unwrap()
    }

    const FAILING: &str = r#"{"__typename":"CheckRun","name":"fmt","detailsUrl":"https://x/1",
        "conclusion":"FAILURE","isRequired":true}"#;
    const PENDING: &str = r#"{"__typename":"CheckRun","name":"slow","detailsUrl":null,
        "conclusion":null,"isRequired":true}"#;
    const LEGACY: &str = r#"{"__typename":"StatusContext","context":"ci/legacy",
        "targetUrl":"https://x/2","state":"ERROR","isRequired":false}"#;

    /// A failed required check on the second page is not missed.
    #[test]
    fn a_two_page_answer_yields_the_cursor_and_every_check() {
        let (mut snapshot, next) = page(&[PENDING, LEGACY].join(","), Some("c1"))
            .into_page(7)
            .unwrap();
        assert_eq!(next.as_deref(), Some("c1"));
        assert_eq!(snapshot.failed_check(), None);
        let (second, next) = page(FAILING, None).into_page(7).unwrap();
        assert_eq!(next, None);
        snapshot.checks.extend(second.checks);
        assert_eq!(snapshot.checks.len(), 3);
        assert_eq!(snapshot.failed_check().unwrap().name, "fmt");
    }

    #[test]
    fn only_active_workflows_are_read() {
        let workflows: Workflows = serde_json::from_str(
            r#"{"total_count":3,"workflows":[
                {"id":1,"name":"ci","state":"active"},
                {"id":2,"name":"old","state":"disabled_manually"},
                {"id":3,"name":"gone","state":"deleted"}]}"#,
        )
        .unwrap();
        assert_eq!(workflows.active_ids(), vec![1]);
    }

    #[test]
    fn the_bound_is_the_slowest_run_of_any_workflow() {
        let secs = Duration::from_secs;
        let durations = vec![
            vec![secs(300), secs(420)],
            vec![],
            vec![secs(900), Duration::ZERO, secs(600)],
        ];
        assert_eq!(slowest(&durations), Some(secs(900)));
        assert_eq!(slowest(&[vec![], vec![]]), None);
        assert_eq!(slowest(&[]), None);
    }

    /// The shape GitHub answers the query with.
    #[test]
    fn a_graphql_answer_parses_into_a_snapshot() {
        let (snapshot, next) = page(&[FAILING, PENDING, LEGACY].join(","), None)
            .into_page(7)
            .unwrap();
        assert_eq!(next, None);
        assert_eq!(snapshot.state, PullRequestState::Open);
        assert_eq!(snapshot.queue_position, Some(2));
        assert_eq!(snapshot.removals, 1);
        assert_eq!(snapshot.last_removal.as_deref(), Some("merge conflict"));
        assert_eq!(
            snapshot.failed_check(),
            Some(FailedCheck {
                name: "fmt".to_string(),
                url: Some("https://x/1".to_string()),
            })
        );
    }

    #[test]
    fn a_graphql_error_is_reported() {
        let response: GraphResponse = serde_json::from_str(
            r#"{"data":{"repository":{"pullRequest":null}},"errors":[{"message":"boom"}]}"#,
        )
        .unwrap();
        let error = response.into_page(7).unwrap_err().to_string();
        assert!(error.contains("boom"), "{error}");
    }

    #[test]
    fn a_workflow_run_lasts_from_its_start_to_its_last_update() {
        let runs: WorkflowRuns = serde_json::from_str(
            r#"{"workflow_runs":[{"run_started_at":"2026-11-07T06:00:00Z",
                "updated_at":"2026-11-07T06:12:30Z"}]}"#,
        )
        .unwrap();
        assert_eq!(
            runs.workflow_runs[0].duration(),
            Some(Duration::from_secs(750))
        );
    }

    fn check_run(name: &str, conclusion: Option<CheckConclusion>, required: bool) -> CheckContext {
        CheckContext::CheckRun {
            name: name.to_string(),
            details_url: None,
            status: Some(if conclusion.is_some() {
                RunStatus::Completed
            } else {
                RunStatus::InProgress
            }),
            started_at: None,
            completed_at: None,
            conclusion,
            is_required: required,
        }
    }

    fn status_context(context: &str, state: StatusState, required: bool) -> CheckContext {
        CheckContext::StatusContext {
            context: context.to_string(),
            target_url: None,
            state,
            created_at: None,
            is_required: required,
        }
    }

    #[test]
    fn the_check_summary_counts_each_context_once_with_the_last_report_winning() {
        let mut snap = snapshot(PullRequestState::Open, None);
        snap.checks = vec![
            check_run("fmt", Some(CheckConclusion::Success), true),
            // A re-run: the earlier failure is superseded by the pass.
            check_run("clippy", Some(CheckConclusion::Failure), true),
            check_run("clippy", Some(CheckConclusion::Success), true),
            check_run("test", None, true),
        ];
        let summary = crate::pr::checks::ChecksReport::of(&snap).summary();
        assert_eq!((summary.passed, summary.pending, summary.failed), (2, 1, 0));
    }

    #[test]
    fn the_check_summary_ignores_non_required_contexts_when_required_exist() {
        let mut snap = snapshot(PullRequestState::Open, None);
        snap.checks = vec![
            check_run("fmt", Some(CheckConclusion::Success), true),
            check_run("coverage", Some(CheckConclusion::Failure), false),
        ];
        let summary = crate::pr::checks::ChecksReport::of(&snap).summary();
        assert_eq!((summary.passed, summary.pending, summary.failed), (1, 0, 0));
        // With nothing required, every context counts.
        snap.checks = vec![
            check_run("coverage", Some(CheckConclusion::Failure), false),
            status_context("ci/test", StatusState::Pending, false),
        ];
        let summary = crate::pr::checks::ChecksReport::of(&snap).summary();
        assert_eq!((summary.passed, summary.pending, summary.failed), (0, 1, 1));
    }

    #[test]
    fn the_check_summary_maps_neutral_and_status_states() {
        let mut snap = snapshot(PullRequestState::Open, None);
        snap.checks = vec![
            // Neutral and skipped both deserialize as Other and count as
            // passed, matching GitHub's rollup.
            check_run("fmt", Some(CheckConclusion::Other), true),
            check_run("clippy", Some(CheckConclusion::TimedOut), true),
            status_context("ci/test", StatusState::Expected, true),
            status_context("ci/web", StatusState::Error, true),
        ];
        let summary = crate::pr::checks::ChecksReport::of(&snap).summary();
        assert_eq!((summary.passed, summary.pending, summary.failed), (1, 1, 2));
    }
}
