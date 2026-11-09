//! Post-merge sync of local checkouts.
//!
//! A merged GitHub PR changes the remote only. Every local checkout recorded
//! for the repository (the repo-usage registry) is a sync target and gets a
//! best-effort fast-forward `git pull`. A failure — dirty tree, divergence,
//! no network — is reported and logged, and never blocks the session.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt as _;

use crate::host_git::HostGit;

/// How long one checkout's pull may run before it is reported as failed.
const PULL_TIMEOUT: Duration = Duration::from_secs(60);

/// What became of one checkout's pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncedCheckout {
    /// The checkout.
    pub path: String,
    /// Why the pull failed; `None` when the checkout is up to date.
    pub failure: Option<String>,
}

/// The local checkouts recorded for the GitHub repository `owner_repo`.
pub async fn recorded_checkouts(db: &crate::db::ChatDb, owner_repo: &str) -> Vec<PathBuf> {
    let usage = match db.repo_usage_since(0).await {
        Ok(usage) => usage,
        Err(error) => {
            tracing::warn!(%error, "cannot read the recorded checkouts for pull-sync");
            return Vec::new();
        }
    };
    let owner_repo = owner_repo.to_string();
    blocking::unblock(move || {
        usage
            .iter()
            .map(crate::db::RepoUsage::path)
            .filter(|path| path.is_dir())
            .filter(|path| {
                matches!(
                    crate::trust::repo_kind(path),
                    crate::trust::RepoKind::Github { owner_repo: found } if found == owner_repo
                )
            })
            .collect()
    })
    .await
}

/// How many checkouts are pulled at once; each pull is a network round trip,
/// not CPU work, so the bound only keeps a long registry from opening
/// hundreds of connections.
const PULL_CONCURRENCY: usize = 8;

/// Fast-forward every checkout in `checkouts`, at most [`PULL_CONCURRENCY`]
/// at a time, each bounded by [`PULL_TIMEOUT`]; results come back in the
/// order of `checkouts`. Failures are logged and returned; nothing is raised.
pub async fn sync(host: &HostGit, checkouts: Vec<PathBuf>) -> Vec<SyncedCheckout> {
    futures_util::stream::iter(checkouts.into_iter().map(|checkout| async move {
        let path = checkout.display().to_string();
        let failure = pull(host, &checkout).await.err();
        if let Some(reason) = &failure {
            tracing::warn!(checkout = %path, %reason, "post-merge pull sync failed");
        }
        SyncedCheckout { path, failure }
    }))
    .buffered(PULL_CONCURRENCY)
    .collect()
    .await
}

/// `git pull --ff-only` in `checkout`, as a child process that is killed when
/// [`PULL_TIMEOUT`] passes.
async fn pull(host: &HostGit, checkout: &Path) -> Result<(), String> {
    let mut command = async_process::Command::from(host.command());
    command
        .arg("-C")
        .arg(checkout)
        .args(["pull", "--ff-only"])
        .stdin(async_process::Stdio::null())
        .kill_on_drop(true);
    // Dropping the losing future drops the child, which kills it.
    let run = async {
        let output = command.output().await.map_err(|error| error.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    };
    let timed_out = async {
        async_io::Timer::after(PULL_TIMEOUT).await;
        Err(format!("git pull did not finish in {PULL_TIMEOUT:?}"))
    };
    futures_lite::future::or(run, timed_out).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git");
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A bare origin, two checkouts of it, and a commit pushed after both
    /// were cloned.
    struct Fixture {
        tmp: tempfile::TempDir,
        first: PathBuf,
        second: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let seed = root.join("seed");
            let origin = root.join("origin.git");
            std::fs::create_dir_all(&seed).unwrap();
            git(&seed, &["init", "-q", "-b", "main"]);
            std::fs::write(seed.join("a.txt"), "a").unwrap();
            git(&seed, &["add", "."]);
            git(&seed, &["commit", "-qm", "init"]);
            git(
                &root,
                &[
                    "clone",
                    "-q",
                    "--bare",
                    seed.to_str().unwrap(),
                    origin.to_str().unwrap(),
                ],
            );
            let (first, second) = (root.join("first"), root.join("second"));
            for checkout in [&first, &second] {
                git(
                    &root,
                    &[
                        "clone",
                        "-q",
                        origin.to_str().unwrap(),
                        checkout.to_str().unwrap(),
                    ],
                );
            }
            // The merge happens upstream, after both checkouts exist.
            std::fs::write(seed.join("merged.txt"), "m").unwrap();
            git(&seed, &["add", "."]);
            git(&seed, &["commit", "-qm", "merged PR"]);
            git(&seed, &["push", "-q", origin.to_str().unwrap(), "main"]);
            Self { tmp, first, second }
        }
    }

    #[tokio::test]
    async fn a_checkout_behind_the_merge_is_fast_forwarded() {
        let fx = Fixture::new();
        let synced = sync(&HostGit::from_file("/dev/null"), vec![fx.first.clone()]).await;
        assert_eq!(synced.len(), 1);
        assert_eq!(synced[0].failure, None);
        assert!(fx.first.join("merged.txt").exists());
        drop(fx.tmp);
    }

    #[tokio::test]
    async fn a_failing_pull_is_reported_and_does_not_stop_the_other_checkouts() {
        let fx = Fixture::new();
        // A local commit makes the pull a non-fast-forward.
        std::fs::write(fx.first.join("local.txt"), "l").unwrap();
        git(&fx.first, &["add", "."]);
        git(&fx.first, &["commit", "-qm", "local"]);
        let synced = sync(
            &HostGit::from_file("/dev/null"),
            vec![fx.first.clone(), fx.second.clone()],
        )
        .await;
        assert!(synced[0].failure.is_some(), "{synced:?}");
        assert_eq!(synced[1].failure, None);
        assert!(fx.second.join("merged.txt").exists());
        assert!(!fx.first.join("merged.txt").exists());
    }

    #[tokio::test]
    async fn a_checkout_that_is_gone_is_reported() {
        let fx = Fixture::new();
        let gone = fx.first.with_file_name("gone");
        let synced = sync(&HostGit::from_file("/dev/null"), vec![gone]).await;
        assert!(synced[0].failure.is_some());
    }

    #[tokio::test]
    async fn only_recorded_checkouts_of_the_repository_are_targets() {
        let fx = Fixture::new();
        let db = crate::db::ChatDb::open(&fx.tmp.path().join("history.db"))
            .await
            .unwrap();
        for checkout in [&fx.first, &fx.second] {
            git(
                checkout,
                &[
                    "remote",
                    "set-url",
                    "origin",
                    "https://github.com/acme/app.git",
                ],
            );
        }
        // The second checkout belongs to another repository.
        git(
            &fx.second,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/acme/other.git",
            ],
        );
        for checkout in [&fx.first, &fx.second] {
            db.touch_repo_usage(checkout).await.unwrap();
        }
        let targets = recorded_checkouts(&db, "acme/app").await;
        assert_eq!(targets, vec![fx.first.clone()]);
    }

    #[test]
    fn the_flag_defaults_on_and_turns_off() {
        use crate::workspace::ProjectPr;
        assert!(ProjectPr::default().pulls_after_merge());
        let off = ProjectPr {
            pull_sync: Some(false),
            ..ProjectPr::default()
        };
        assert!(!off.pulls_after_merge());
    }
}
