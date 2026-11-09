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
pub const PULL_TIMEOUT: Duration = Duration::from_secs(60);

/// What became of one checkout's pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncedCheckout {
    /// The checkout.
    pub path: String,
    /// Why the pull failed; `None` when the checkout is up to date.
    pub failure: Option<String>,
}

/// A recorded checkout of a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checkout {
    /// The directory exists and its `origin` is the repository.
    Present(PathBuf),
    /// The directory is gone; it was recorded under the repository.
    Gone(PathBuf),
}

/// The local checkouts recorded for the GitHub repository `owner_repo`. A
/// checkout whose directory still exists is matched by its current `origin`;
/// one that is gone, by the repository recorded with it.
pub async fn recorded_checkouts(db: &crate::db::ChatDb, owner_repo: &str) -> Vec<Checkout> {
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
            .filter_map(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    matches!(
                        crate::trust::repo_kind(&path),
                        crate::trust::RepoKind::Github { owner_repo: found } if found == owner_repo
                    )
                    .then_some(Checkout::Present(path))
                } else {
                    (entry.owner_repo.as_deref() == Some(owner_repo.as_str()))
                        .then_some(Checkout::Gone(path))
                }
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
/// at a time, each bounded by `deadline`; results come back in the order of
/// `checkouts`. A checkout whose directory is gone is reported without a
/// pull. Failures are logged and returned; nothing is raised.
pub async fn sync(
    host: &HostGit,
    checkouts: Vec<Checkout>,
    deadline: Duration,
) -> Vec<SyncedCheckout> {
    futures_util::stream::iter(checkouts.into_iter().map(|checkout| async move {
        let (path, outcome) = match checkout {
            Checkout::Present(path) => {
                let outcome = pull(host, &path, deadline).await;
                (path, outcome)
            }
            Checkout::Gone(path) => (path, Err("checkout gone".to_string())),
        };
        let path = path.display().to_string();
        let failure = outcome.err();
        if let Some(reason) = &failure {
            tracing::warn!(checkout = %path, %reason, "post-merge pull sync failed");
        }
        SyncedCheckout { path, failure }
    }))
    .buffered(PULL_CONCURRENCY)
    .collect()
    .await
}

/// `git pull --ff-only` in `checkout`, as a child process in its own process
/// group. When `deadline` passes, the whole group is killed: git and
/// everything it started (transport helpers, `ssh`, hooks).
async fn pull(host: &HostGit, checkout: &Path, deadline: Duration) -> Result<(), String> {
    let mut std_command = host.command();
    std::os::unix::process::CommandExt::process_group(&mut std_command, 0);
    let mut command = async_process::Command::from(std_command);
    command
        .arg("-C")
        .arg(checkout)
        .args(["pull", "--ff-only"])
        .stdin(async_process::Stdio::null())
        .stdout(async_process::Stdio::null())
        .stderr(async_process::Stdio::piped());
    let child = command.spawn().map_err(|error| error.to_string())?;
    // The group leader's pid is the group id.
    let group = child
        .id()
        .try_into()
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .expect("a spawned child has a positive pid");
    let run = async {
        let output = child.output().await.map_err(|error| error.to_string())?;
        Ok::<_, String>(if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        })
    };
    let timed_out = async {
        async_io::Timer::after(deadline).await;
        Ok(Err(format!("git pull did not finish in {deadline:?}")))
    };
    // `run` sees the pull end, `timed_out` the deadline pass; only the
    // deadline leaves a group to stop.
    let mut expired = false;
    let outcome = futures_lite::future::or(run, async {
        let outcome = timed_out.await;
        expired = true;
        outcome
    })
    .await?;
    if expired {
        // The group outlives its leader while any member runs.
        match rustix::process::kill_process_group(group, rustix::process::Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(error) => return Err(format!("cannot stop the timed-out pull: {error}")),
        }
    }
    outcome
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
        let synced = sync(
            &HostGit::from_file("/dev/null"),
            vec![Checkout::Present(fx.first.clone())],
            PULL_TIMEOUT,
        )
        .await;
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
            vec![
                Checkout::Present(fx.first.clone()),
                Checkout::Present(fx.second.clone()),
            ],
            PULL_TIMEOUT,
        )
        .await;
        assert!(synced[0].failure.is_some(), "{synced:?}");
        assert_eq!(synced[1].failure, None);
        assert!(fx.second.join("merged.txt").exists());
        assert!(!fx.first.join("merged.txt").exists());
    }

    #[tokio::test]
    async fn a_recorded_checkout_that_is_gone_is_reported_under_its_repository() {
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
            db.touch_repo_usage(checkout).await.unwrap();
        }
        std::fs::remove_dir_all(&fx.first).unwrap();

        let targets = recorded_checkouts(&db, "acme/app").await;
        assert_eq!(
            targets,
            vec![
                Checkout::Gone(fx.first.clone()),
                Checkout::Present(fx.second.clone())
            ]
        );
        assert_eq!(recorded_checkouts(&db, "acme/other").await.len(), 0);

        let gone = targets.into_iter().take(1).collect();
        let synced = sync(&HostGit::from_file("/dev/null"), gone, PULL_TIMEOUT).await;
        assert_eq!(synced.len(), 1);
        assert_eq!(synced[0].failure.as_deref(), Some("checkout gone"));
    }

    /// A pull stuck on a transport helper is stopped at the deadline together
    /// with everything the helper started.
    #[tokio::test]
    async fn a_hung_pull_is_killed_with_its_children() {
        use std::os::unix::fs::PermissionsExt as _;
        let fx = Fixture::new();
        let hang = fx.tmp.path().join("hang.sh");
        let pid_file = fx.tmp.path().join("child.pid");
        std::fs::write(
            &hang,
            format!(
                "#!/bin/sh\nsleep 300 &\necho $! > {}\nwait\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hang, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(
            &fx.first,
            &[
                "remote",
                "set-url",
                "origin",
                &format!("ext::{}", hang.display()),
            ],
        );
        let config = fx.tmp.path().join("gitconfig");
        std::fs::write(&config, "[protocol \"ext\"]\n\tallow = always\n").unwrap();

        let started = std::time::Instant::now();
        let synced = sync(
            &HostGit::from_file(config),
            vec![Checkout::Present(fx.first.clone())],
            Duration::from_secs(2),
        )
        .await;

        assert!(started.elapsed() < Duration::from_secs(30));
        let failure = synced[0].failure.as_deref().unwrap();
        assert!(failure.contains("did not finish"), "{failure}");
        let child: i32 = std::fs::read_to_string(&pid_file)
            .expect("the helper started its child")
            .trim()
            .parse()
            .unwrap();
        // Signal 0 only asks whether the process exists; give the kernel a
        // moment to deliver the kill.
        let mut alive = true;
        for _ in 0..50 {
            alive = std::process::Command::new("kill")
                .args(["-0", &child.to_string()])
                .output()
                .unwrap()
                .status
                .success();
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!alive, "the helper's child {child} survived the deadline");
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
        assert_eq!(targets, vec![Checkout::Present(fx.first.clone())]);
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
