//! Request-time alerts when a daemon-maintained read-only checkout updates
//! across files the session has previously read.
//!
//! Reads the session's last-seen commit (`session_checkout_seen`) and read
//! files (`session_checkout_files`) on `Moment::Request` without ever waking
//! an idle session, collapsing any number of intermediate updates into one
//! alert covering `old_commit..current_commit`.

use std::collections::HashSet;

use aither_agent::{ContextBlock, ContextSource, Moment, SourceId};
use askama::Template;

use crate::db::ChatDb;
use crate::repo_clone::DaemonCheckouts;

const TAG: &str = "checkout_update";

#[derive(Template)]
#[template(path = "checkout_update.txt", escape = "none")]
struct CheckoutUpdateTemplate<'a> {
    repo_key: &'a str,
    old_commit: &'a str,
    new_commit: &'a str,
    files: &'a [String],
}

/// Emits one collapsed alert per repository at `Moment::Request` when its
/// daemon checkout moved past the last commit seen by the session and touches
/// files the session previously read.
#[derive(Debug)]
pub struct CheckoutUpdates {
    db: ChatDb,
    checkouts: DaemonCheckouts,
    session_id: String,
}

impl CheckoutUpdates {
    /// Context source tracking `session_id`'s read-only checkouts.
    #[must_use]
    pub fn new(db: ChatDb, checkouts: DaemonCheckouts, session_id: impl Into<String>) -> Self {
        Self {
            db,
            checkouts,
            session_id: session_id.into(),
        }
    }

    async fn collect_updates(&self, advance: bool) -> Vec<ContextBlock> {
        let tracked = match self.db.list_checkout_seen_repos(&self.session_id).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(session_id = %self.session_id, %error, "failed to load checkout seen commits");
                return Vec::new();
            }
        };
        let mut rendered = Vec::new();
        for (repo_key, old_commit) in tracked {
            let current_commit = match self.checkouts.current_head(&repo_key).await {
                Ok(Some(sha)) => sha,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(session_id = %self.session_id, repo = %repo_key, %error, "failed to read current checkout head");
                    continue;
                }
            };
            if current_commit == old_commit {
                continue;
            }
            let read_files = match self
                .db
                .list_checkout_read_files(&self.session_id, &repo_key)
                .await
            {
                Ok(files) => files,
                Err(error) => {
                    tracing::warn!(session_id = %self.session_id, repo = %repo_key, %error, "failed to load read checkout files");
                    continue;
                }
            };
            if !read_files.is_empty() {
                match self
                    .checkouts
                    .diff_files(&repo_key, &old_commit, &current_commit)
                    .await
                {
                    Ok(changed) => {
                        let read_set: HashSet<&str> =
                            read_files.iter().map(String::as_str).collect();
                        let relevant: Vec<String> = changed
                            .into_iter()
                            .filter(|path| read_set.contains(path.as_str()))
                            .collect();
                        if !relevant.is_empty() {
                            let text = CheckoutUpdateTemplate {
                                repo_key: &repo_key,
                                old_commit: &old_commit,
                                new_commit: &current_commit,
                                files: &relevant,
                            }
                            .render()
                            .expect("checkout update template renders");
                            rendered.push(text.trim_end().to_string());
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            session_id = %self.session_id,
                            repo = %repo_key,
                            old_commit = %old_commit,
                            new_commit = %current_commit,
                            %error,
                            "failed to diff updated checkout"
                        );
                        continue;
                    }
                }
            }
            if advance
                && let Err(error) = self
                    .db
                    .set_seen_checkout_commit(&self.session_id, &repo_key, &current_commit)
                    .await
            {
                tracing::warn!(session_id = %self.session_id, repo = %repo_key, %error, "failed to advance seen checkout commit");
            }
        }
        if rendered.is_empty() {
            Vec::new()
        } else {
            vec![ContextBlock::stable(TAG, rendered.join("\n\n"))]
        }
    }
}

impl ContextSource for CheckoutUpdates {
    fn id(&self) -> SourceId {
        SourceId::new(TAG)
    }

    async fn blocks(&mut self, moment: Moment) -> Vec<ContextBlock> {
        if !matches!(moment, Moment::Request { .. }) {
            return Vec::new();
        }
        self.collect_updates(true).await
    }

    async fn peek(&self, moment: Moment) -> Vec<ContextBlock> {
        if !matches!(moment, Moment::Request { .. }) {
            return Vec::new();
        }
        self.collect_updates(false).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_git::HostGit;
    use std::path::Path;

    fn git(host: &HostGit, dir: Option<&Path>, args: &[&str]) -> String {
        host.output(dir, args).unwrap().trim().to_string()
    }

    #[tokio::test]
    async fn collapsed_checkout_alert_fires_once_across_restart_and_ignores_unread_files() {
        let tmp = tempfile::tempdir().unwrap();
        let db = ChatDb::open(&tmp.path().join("history.db")).await.unwrap();
        let host = HostGit::from_file("/dev/null");

        let remote = tmp.path().join("remote.git");
        git(
            &host,
            None,
            &["init", "--bare", "-b", "main", &remote.to_string_lossy()],
        );

        let work = tmp.path().join("work");
        git(
            &host,
            None,
            &["init", "-b", "main", &work.to_string_lossy()],
        );
        git(&host, Some(&work), &["config", "user.name", "Test"]);
        git(
            &host,
            Some(&work),
            &["config", "user.email", "t@example.com"],
        );
        std::fs::write(work.join("a.txt"), "a1\n").unwrap();
        std::fs::write(work.join("b.txt"), "b1\n").unwrap();
        std::fs::write(work.join("unread.txt"), "u1\n").unwrap();
        git(&host, Some(&work), &["add", "."]);
        git(&host, Some(&work), &["commit", "-m", "c1"]);
        git(
            &host,
            Some(&work),
            &["remote", "add", "origin", &remote.to_string_lossy()],
        );
        git(&host, Some(&work), &["push", "-u", "origin", "main"]);
        let c1 = git(&host, Some(&work), &["rev-parse", "HEAD"]);

        let checkouts = DaemonCheckouts::new(tmp.path().join("checkouts")).with_host(host.clone());
        let remote_url = format!("file://{}", remote.display());
        let repo_key = "o/r";
        let snap1 = checkouts
            .ensure_present(repo_key, &remote_url)
            .await
            .unwrap();
        assert_eq!(snap1.head_sha, c1);

        db.create_session("s1", Some("t"), Some("m")).await.unwrap();
        db.seed_seen_checkout_commit("s1", repo_key, &c1)
            .await
            .unwrap();
        db.record_checkout_read_files("s1", repo_key, &["a.txt".to_string(), "b.txt".to_string()])
            .await
            .unwrap();

        // Advance remote twice while session stays idle: c2 modifies a.txt and
        // unread.txt; c3 modifies b.txt.
        std::fs::write(work.join("a.txt"), "a2\n").unwrap();
        std::fs::write(work.join("unread.txt"), "u2\n").unwrap();
        git(&host, Some(&work), &["add", "."]);
        git(&host, Some(&work), &["commit", "-m", "c2"]);
        git(&host, Some(&work), &["push", "origin", "main"]);
        checkouts.refresh(repo_key, &remote_url).await.unwrap();

        std::fs::write(work.join("b.txt"), "b3\n").unwrap();
        git(&host, Some(&work), &["add", "."]);
        git(&host, Some(&work), &["commit", "-m", "c3"]);
        git(&host, Some(&work), &["push", "origin", "main"]);
        let c3 = git(&host, Some(&work), &["rev-parse", "HEAD"]);
        checkouts.refresh(repo_key, &remote_url).await.unwrap();

        // Reconstruct CheckoutUpdates fresh (simulating reopen days later).
        let mut updates = CheckoutUpdates::new(db, checkouts, "s1");
        assert_eq!(
            updates.blocks(Moment::ToolTurnEnd).await,
            [] as [aither_agent::ContextBlock; 0]
        );

        let blocks = updates.blocks(Moment::Request { reassembled: false }).await;
        assert_eq!(blocks.len(), 1);
        let text = blocks[0].block().content();
        assert!(text.contains(&c1), "{text}");
        assert!(text.contains(&c3), "{text}");
        assert!(text.contains("a.txt"), "{text}");
        assert!(text.contains("b.txt"), "{text}");
        assert!(!text.contains("unread.txt"), "{text}");

        // Next request produces nothing once seen commit advanced to c3.
        assert_eq!(
            updates.blocks(Moment::Request { reassembled: false }).await,
            [] as [aither_agent::ContextBlock; 0]
        );
    }
}
