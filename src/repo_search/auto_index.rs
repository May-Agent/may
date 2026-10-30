//! Keeps the worktrees May is used in indexed, without being asked.
//!
//! repo-search indexes a worktree on its first request and watches it while
//! its daemon runs; its daemon exits after ten idle minutes. The auto
//! indexer touches each frequently used worktree when it becomes frequent
//! and every [`RETOUCH`] after, so a search never waits on a cold index.
//! It is silent: failures (no embedding model, a removed worktree) only
//! reach the log and the daemon status.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use repo_search_client::protocol::Request;

use super::{RepoSearch, worktree_root};
use crate::db::{ChatDb, RepoUsage};

/// How often the hot set is recomputed.
const TICK: Duration = Duration::from_secs(60);
/// How often a hot worktree is touched again.
pub const RETOUCH: Duration = Duration::from_mins(15);
/// Usage older than this drops out of the hot set.
const WINDOW_SECS: i64 = 30 * 24 * 60 * 60;
/// At most this many worktrees are kept warm.
const HOT_SET_SIZE: usize = 20;
/// Longest wait after repeated failures.
const MAX_BACKOFF: Duration = Duration::from_hours(1);

/// Recency-weighted rank: uses, discounted by days since the latest use.
fn rank(usage: &RepoUsage, now: i64) -> f64 {
    let days = (now - usage.last_used_at).max(0) as f64 / 86_400.0;
    usage.use_count as f64 / (1.0 + days)
}

/// The worktrees to keep warm at `now`, best first.
#[must_use]
pub fn hot_set(mut usage: Vec<RepoUsage>, now: i64) -> Vec<RepoUsage> {
    usage.retain(|entry| now - entry.last_used_at <= WINDOW_SECS);
    usage.sort_by(|a, b| {
        rank(b, now)
            .total_cmp(&rank(a, now))
            .then_with(|| a.worktree.cmp(&b.worktree))
    });
    usage.truncate(HOT_SET_SIZE);
    usage
}

/// Whether `worktree` was touched within [`RETOUCH`] of `now`.
fn fresh(touched: &HashMap<PathBuf, Instant>, worktree: &PathBuf, now: Instant) -> bool {
    touched
        .get(worktree)
        .is_some_and(|at| now.duration_since(*at) < RETOUCH)
}

/// The daemon's auto-index loop.
#[derive(Debug)]
pub struct AutoIndexer {
    db: ChatDb,
    repo_search: RepoSearch,
    touched: HashMap<PathBuf, Instant>,
    backoff: Option<Duration>,
}

impl AutoIndexer {
    /// Auto-indexer over the usage recorded in `db`.
    #[must_use]
    pub fn new(db: ChatDb, repo_search: RepoSearch) -> Self {
        Self {
            db,
            repo_search,
            touched: HashMap::new(),
            backoff: None,
        }
    }

    /// Worktrees of the hot set that are due for a touch, after pruning
    /// usage that expired or no longer names a worktree root.
    async fn due(&mut self) -> Vec<PathBuf> {
        let now = jiff::Timestamp::now().as_second();
        if let Err(error) = self.db.expire_repo_usage(now - WINDOW_SECS).await {
            tracing::debug!(%error, "failed to expire repo usage");
        }
        let usage = match self.db.repo_usage_since(now - WINDOW_SECS).await {
            Ok(usage) => usage,
            Err(error) => {
                tracing::debug!(%error, "failed to read repo usage");
                return Vec::new();
            }
        };
        let instant = Instant::now();
        let mut due = Vec::new();
        for entry in hot_set(usage, now) {
            let path = entry.path();
            if worktree_root(&path).as_ref() != Some(&path) {
                let _ = self.db.forget_repo_usage(&path).await;
                self.touched.remove(&path);
                continue;
            }
            if !fresh(&self.touched, &path, instant) {
                due.push(path);
            }
        }
        due
    }

    async fn touch(&self, worktree: PathBuf) -> Result<(), super::RepoSearchError> {
        let client = self.repo_search.client().await?;
        client
            .request(Request::Index {
                worktree,
                wait: false,
            })
            .await?
            .finish()
            .await?;
        Ok(())
    }

    /// Runs forever.
    pub async fn run(mut self) {
        loop {
            let wait = self.backoff.unwrap_or(TICK);
            async_io::Timer::after(wait).await;
            let mut failed = false;
            for worktree in self.due().await {
                match self.touch(worktree.clone()).await {
                    Ok(()) => {
                        tracing::debug!(worktree = %worktree.display(), "auto-index touched");
                        self.touched.insert(worktree, Instant::now());
                    }
                    Err(error) => {
                        tracing::debug!(worktree = %worktree.display(), %error, "auto-index deferred");
                        failed = true;
                        break;
                    }
                }
            }
            self.backoff = failed.then(|| {
                self.backoff
                    .map_or(RETOUCH, |current| current * 2)
                    .min(MAX_BACKOFF)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::{HOT_SET_SIZE, RETOUCH, WINDOW_SECS, fresh, hot_set};
    use crate::db::RepoUsage;

    fn usage(worktree: &str, last_used_at: i64, use_count: i64) -> RepoUsage {
        RepoUsage {
            worktree: worktree.to_string(),
            last_used_at,
            use_count,
        }
    }

    #[test]
    fn recent_frequent_worktrees_rank_first_and_stale_ones_drop() {
        let now = 1_000_000_000;
        let day = 86_400;
        let ranked = hot_set(
            vec![
                usage("/old-busy", now - 20 * day, 40),
                usage("/today", now, 3),
                usage("/expired", now - WINDOW_SECS - 1, 500),
                usage("/yesterday", now - day, 2),
            ],
            now,
        );
        let names: Vec<&str> = ranked.iter().map(|entry| entry.worktree.as_str()).collect();
        assert_eq!(names, ["/today", "/old-busy", "/yesterday"]);
    }

    #[test]
    fn the_hot_set_is_capped() {
        let now = 1_000;
        let many = (0..50).map(|n| usage(&format!("/r{n}"), now, n)).collect();
        assert_eq!(hot_set(many, now).len(), HOT_SET_SIZE);
    }

    #[test]
    fn a_worktree_is_due_again_after_the_retouch_interval() {
        let path = PathBuf::from("/repo");
        let start = Instant::now();
        let mut touched = HashMap::new();
        assert!(!fresh(&touched, &path, start));
        touched.insert(path.clone(), start);
        assert!(fresh(&touched, &path, start + Duration::from_secs(60)));
        assert!(!fresh(&touched, &path, start + RETOUCH));
    }
}
