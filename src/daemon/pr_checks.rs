//! The poller that follows the checks of every in-flight GitHub PR.
//!
//! Each tick reads every live PR's checks once. The newest read of each PR is
//! kept for the clients' PR rows (and pushed to them when it changes), and a
//! move of the rollup the agent has to hear of -- a failure, every check
//! passed, the checks unreadable -- becomes one stored alert. The stored
//! state per PR (`pr_check_state`) is what makes that one alert per event:
//! an alert is raised only when the rollup moves from what was stored, and
//! the new state and the alert are written in one transaction.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use askama::Template;
use futures_util::StreamExt;

use super::session_events::SessionChange;
use super::state::DaemonState;
use super::types::PrChecksRead;
use crate::db::{PrCheckState, PrRow};
use crate::pr::checks::{ChecksEvent, ChecksReport};
use crate::pr::merge_watch::{GithubPr, Upstream};

/// PRs read from GitHub at once.
const READ_CONCURRENCY: usize = 4;

/// The newest read of each in-flight PR's checks, by PR row id.
#[derive(Debug, Default)]
pub struct PrChecksCache(Mutex<HashMap<i64, PrChecksRead>>);

impl PrChecksCache {
    /// The newest read of the PR's checks.
    pub fn get(&self, pr_id: i64) -> Option<PrChecksRead> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&pr_id)
            .cloned()
    }

    /// Stores `read`; whether it differs from the one stored.
    fn set(&self, pr_id: i64, read: &PrChecksRead) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pr_id, read.clone())
            .as_ref()
            != Some(read)
    }

    /// Drops every PR not in `live`; the ids dropped.
    fn retain(&self, live: &[i64]) -> Vec<i64> {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let gone: Vec<i64> = map
            .keys()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        for id in &gone {
            map.remove(id);
        }
        gone
    }
}

/// The state to store after a read, and the event the agent hears of, given
/// the state stored before.
fn advance(
    previous: Option<&PrCheckState>,
    pr_number: i64,
    read: &PrChecksRead,
) -> (PrCheckState, Option<ChecksEvent>) {
    match read {
        Ok(report) => {
            let rollup = report.rollup();
            let moved =
                previous.is_none_or(|p| p.head_sha != report.head_sha || p.rollup != rollup);
            let state = PrCheckState {
                head_sha: report.head_sha.clone(),
                rollup,
                unreadable: false,
            };
            let event = if moved {
                ChecksEvent::for_rollup(pr_number, report)
            } else {
                None
            };
            (state, event)
        }
        Err(error) => {
            let was_unreadable = previous.is_some_and(|p| p.unreadable);
            let state = PrCheckState {
                unreadable: true,
                ..previous.cloned().unwrap_or_else(|| PrCheckState {
                    head_sha: String::new(),
                    rollup: crate::pr::checks::Rollup::Pending,
                    unreadable: true,
                })
            };
            let event = (!was_unreadable).then(|| ChecksEvent::Unreadable {
                pr_number,
                error: error.clone(),
            });
            (state, event)
        }
    }
}

/// Starts the poller. The interval is `[ci] pr_poll_seconds`, read on every
/// tick so a change applies at once.
pub(super) fn spawn(state: Arc<DaemonState>) {
    state.tokio_handle.clone().spawn(async move {
        loop {
            poll(&state).await;
            let seconds = state.config.load().ci.pr_poll_seconds;
            async_io::Timer::after(Duration::from_secs(seconds)).await;
        }
    });
}

async fn poll(state: &DaemonState) {
    let rows = match state.db().live_github_prs().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(%error, "could not list the in-flight PRs to follow their checks");
            return;
        }
    };
    let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    for id in state.pr_checks.retain(&ids) {
        tracing::debug!(
            pr_id = id,
            "PR left flight; its checks are no longer followed"
        );
    }
    let reads: Vec<(PrRow, PrChecksRead)> = futures_util::stream::iter(rows)
        .map(|row| async move {
            let read = read_checks(state, &row).await;
            (row, read)
        })
        .buffered(READ_CONCURRENCY)
        .collect()
        .await;
    for (row, read) in reads {
        apply(state, &row, read).await;
    }
}

async fn read_checks(state: &DaemonState, row: &PrRow) -> PrChecksRead {
    let repo_key = state
        .db()
        .repo_key_of_project(row.project_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "PR #{} belongs to a repository the database lost",
                row.number
            )
        })?;
    let external = row
        .external_number
        .ok_or_else(|| format!("PR #{} has no GitHub number", row.number))?;
    let github = GithubPr {
        api: &state.github_api,
        owner_repo: &repo_key,
        number: external,
    };
    github
        .snapshot()
        .await
        .map(|snapshot| ChecksReport::of(&snapshot))
        .map_err(|e| e.to_string())
}

async fn apply(state: &DaemonState, row: &PrRow, read: PrChecksRead) {
    if state.pr_checks.set(row.id, &read) {
        state
            .ui_event_bus
            .publish_session_change(&row.session_id, SessionChange::Prs);
    }
    let db = state.db();
    let previous = match db.pr_check_state(row.id).await {
        Ok(previous) => previous,
        Err(error) => {
            tracing::error!(pr = row.number, %error, "could not load the PR's checks state");
            return;
        }
    };
    let (next, event) = advance(previous.as_ref(), row.number, &read);
    if previous.as_ref() == Some(&next) && event.is_none() {
        return;
    }
    let text = match event.as_ref().map(Template::render).transpose() {
        Ok(text) => text,
        Err(error) => {
            tracing::error!(pr = row.number, %error, "could not render the PR checks alert");
            return;
        }
    };
    if let Err(error) = db
        .store_pr_check_state(row.id, &row.session_id, &next, text.as_deref())
        .await
    {
        tracing::error!(pr = row.number, %error, "could not store the PR's checks state");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::checks::ChecksReport;
    use crate::pr::merge_watch::{
        CheckConclusion, CheckContext, PullRequestState, RunStatus, Snapshot,
    };

    #[allow(clippy::unnecessary_wraps)]
    fn read(head: &str, conclusions: &[Option<CheckConclusion>]) -> PrChecksRead {
        Ok(ChecksReport::of(&Snapshot {
            state: PullRequestState::Open,
            queue_position: None,
            removals: 0,
            last_removal: None,
            head_sha: head.to_string(),
            checks: conclusions
                .iter()
                .enumerate()
                .map(|(i, conclusion)| CheckContext::CheckRun {
                    name: format!("测试{i}"),
                    details_url: None,
                    status: Some(if conclusion.is_some() {
                        RunStatus::Completed
                    } else {
                        RunStatus::InProgress
                    }),
                    started_at: None,
                    completed_at: None,
                    conclusion: *conclusion,
                    is_required: true,
                })
                .collect(),
        }))
    }

    /// Feeds `reads` through `advance` the way the poller does and collects
    /// the events raised.
    fn events(reads: &[PrChecksRead]) -> Vec<ChecksEvent> {
        let mut state: Option<PrCheckState> = None;
        let mut raised = Vec::new();
        for read in reads {
            let (next, event) = advance(state.as_ref(), 7, read);
            state = Some(next);
            raised.extend(event);
        }
        raised
    }

    const OK: Option<CheckConclusion> = Some(CheckConclusion::Success);
    const BAD: Option<CheckConclusion> = Some(CheckConclusion::Failure);

    #[test]
    fn a_failure_alerts_once_however_often_it_is_read() {
        let raised = events(&[
            read("a", &[None, None]),
            read("a", &[OK, None]),
            read("a", &[OK, BAD]),
            read("a", &[OK, BAD]),
            read("a", &[BAD, BAD]),
        ]);
        assert_eq!(
            raised,
            [ChecksEvent::Failed {
                pr_number: 7,
                names: vec!["测试1".to_string()]
            }]
        );
    }

    #[test]
    fn all_checks_passing_alerts_once() {
        let raised = events(&[
            read("a", &[OK, None]),
            read("a", &[OK, OK]),
            read("a", &[OK, OK]),
        ]);
        assert_eq!(
            raised,
            [ChecksEvent::Passed {
                pr_number: 7,
                total: 2
            }]
        );
    }

    #[test]
    fn a_new_commit_or_a_rerun_is_a_new_event() {
        let raised = events(&[
            read("a", &[BAD]),
            read("a", &[None]),
            read("a", &[BAD]),
            read("b", &[BAD]),
        ]);
        assert_eq!(raised.len(), 3);
    }

    #[test]
    fn an_unreadable_query_alerts_once_until_it_reads_again() {
        let down: PrChecksRead = Err("GitHub 502".to_string());
        let raised = events(&[
            read("a", &[BAD]),
            down.clone(),
            down.clone(),
            read("a", &[BAD]),
            down,
        ]);
        assert_eq!(
            raised,
            [
                ChecksEvent::Failed {
                    pr_number: 7,
                    names: vec!["测试0".to_string()]
                },
                ChecksEvent::Unreadable {
                    pr_number: 7,
                    error: "GitHub 502".to_string()
                },
                ChecksEvent::Unreadable {
                    pr_number: 7,
                    error: "GitHub 502".to_string()
                },
            ]
        );
    }

    #[test]
    fn the_cache_reports_a_change_once() {
        let cache = PrChecksCache::default();
        assert!(cache.set(1, &read("a", &[None])));
        assert!(!cache.set(1, &read("a", &[None])));
        assert!(cache.set(1, &read("a", &[OK])));
        assert_eq!(cache.retain(&[2]), [1]);
        assert_eq!(cache.get(1), None);
    }
}
