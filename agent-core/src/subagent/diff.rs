//! The unified diff a subagent run leaves in its granted and cloned
//! worktrees, embedded in its report.
//!
//! Where a worktree stood before the run is taken as a [`Baseline`] at the
//! same moment the commit ledger marks its namespaces; the diff itself runs
//! after the run ends, against the worktree's host git. A worktree's diff is
//! `git diff <pre-run HEAD>` — which covers the work it committed and the
//! tracked changes it left uncommitted alike — plus one `--no-index` diff
//! per file still untracked, each capped so a runaway change cannot crowd
//! the rest of the report out.

use std::path::PathBuf;

use crate::host_git::HostGit;

/// The empty tree, the base of a worktree whose `HEAD` is unborn.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The most bytes of diff one worktree contributes to the report, after
/// which the text is cut on a line boundary and says so.
const DIFF_CAP: usize = 64 * 1024;

/// Where one worktree stood before a run.
#[derive(Debug, Clone)]
pub struct Baseline {
    /// The worktree.
    pub path: PathBuf,
    /// Its `HEAD` then; `None` when unborn.
    head: Option<String>,
    /// Whether the directory is a git worktree at all.
    is_repo: bool,
}

/// The diff of one worktree since its baseline, or why it could not be
/// made.
#[derive(Debug)]
pub struct WorktreeDiff {
    /// The worktree.
    pub path: PathBuf,
    /// The unified diff text, `Ok("No changes.")` when it left none.
    pub outcome: Result<String, String>,
}

/// Records where each of `roots` stands; the worktree a path is not, a git
/// repository or not, is decided now so a later change cannot fake it.
pub async fn baseline(git: &HostGit, roots: Vec<PathBuf>) -> Vec<Baseline> {
    let git = git.clone();
    blocking::unblock(move || {
        roots
            .into_iter()
            .map(|path| {
                let head = git
                    .output(Some(&path), &["rev-parse", "--verify", "HEAD"])
                    .ok()
                    .map(|oid| oid.trim().to_string());
                let is_repo =
                    head.is_some() || git.output(Some(&path), &["rev-parse", "--git-dir"]).is_ok();
                Baseline {
                    path,
                    head,
                    is_repo,
                }
            })
            .collect()
    })
    .await
}

/// Diffs each worktree against its baseline.
pub async fn diff(git: &HostGit, baseline: &[Baseline]) -> Vec<WorktreeDiff> {
    let git = git.clone();
    let baseline = baseline.to_vec();
    blocking::unblock(move || {
        baseline
            .iter()
            .map(|baseline| diff_one(&git, baseline))
            .collect()
    })
    .await
}

/// `git -C path` returning stdout; exit code 1 is accepted since `diff`
/// signals "files differ" with it.
fn git_allowing_diff(
    git: &HostGit,
    path: &std::path::Path,
    args: &[&str],
) -> Result<String, String> {
    let output = git
        .command()
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if output.status.code() > Some(1) {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn diff_one(git: &HostGit, baseline: &Baseline) -> WorktreeDiff {
    let outcome = if baseline.is_repo {
        worktree_diff(git, baseline)
    } else {
        Err("not a git worktree".to_string())
    };
    WorktreeDiff {
        path: baseline.path.clone(),
        outcome,
    }
}

fn worktree_diff(git: &HostGit, baseline: &Baseline) -> Result<String, String> {
    let base = baseline.head.as_deref().unwrap_or(EMPTY_TREE);
    let mut text = git_allowing_diff(
        git,
        &baseline.path,
        &["diff", "--no-ext-diff", "--no-renames", base, "--"],
    )?;
    let untracked = git_allowing_diff(
        git,
        &baseline.path,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    for file in untracked.split('\0').filter(|file| !file.is_empty()) {
        // `/dev/null` on the left makes an untracked file read as a new-file
        // diff; `--no-index` answers 1 on a difference, which is the case.
        let added = git_allowing_diff(
            git,
            &baseline.path,
            &[
                "diff",
                "--no-index",
                "--no-renames",
                "--",
                "/dev/null",
                file,
            ],
        )?;
        if !added.is_empty() {
            text.push_str(&added);
        }
    }
    Ok(cap(text))
}

/// `text` cut to [`DIFF_CAP`] on a char boundary, with a marker that says
/// where the rest went.
fn cap(text: String) -> String {
    if text.len() <= DIFF_CAP {
        return text;
    }
    let mut end = DIFF_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    // Cut back to the last whole line so no hunk line is halved.
    let end = text[..end].rfind('\n').unwrap_or(end);
    let mut cut = text[..end].to_string();
    cut.push_str("\n[diff truncated]\n");
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", dir)
            .output()
            .expect("git runs");
        assert!(status.status.success(), "{args:?}: {status:?}");
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).expect("repo");
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("tracked.txt"), "before\n").expect("write");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        (temp, repo)
    }

    #[tokio::test]
    async fn the_diff_covers_committed_uncommitted_and_untracked_work() {
        let (_temp, repo) = repo();
        let host = HostGit::inherit();
        let baseline = baseline(&host, vec![repo.clone()]).await;

        std::fs::write(repo.join("committed.txt"), "committed work\n").expect("write");
        git(&repo, &["add", "committed.txt"]);
        git(&repo, &["commit", "-q", "-m", "work"]);
        std::fs::write(repo.join("tracked.txt"), "before\nuncommitted\n").expect("write");
        std::fs::write(repo.join("untracked.txt"), "brand new\n").expect("write");

        let diffs = diff(&host, &baseline).await;
        assert_eq!(diffs.len(), 1);
        let text = diffs[0].outcome.as_ref().expect("a diff");
        assert!(
            text.contains("+committed work") && text.contains("+uncommitted"),
            "committed and uncommitted work:\n{text}"
        );
        assert!(
            text.contains("untracked.txt") && text.contains("+brand new"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_clean_worktree_reports_no_changes_and_a_plain_dir_is_named() {
        let (_temp, repo) = repo();
        let plain = repo.parent().expect("parent").join("plain");
        std::fs::create_dir(&plain).expect("plain");
        let host = HostGit::inherit();
        let baseline = baseline(&host, vec![repo.clone(), plain]).await;

        let diffs = diff(&host, &baseline).await;
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].outcome.as_deref(), Ok(""));
        assert!(
            diffs[1]
                .outcome
                .as_ref()
                .expect_err("an error")
                .contains("not a git worktree")
        );
    }

    #[test]
    fn a_runaway_diff_is_capped_on_a_line_boundary() {
        let text = format!("{}\n", "x".repeat(DIFF_CAP + 128));
        let capped = cap(text);
        assert!(capped.len() < DIFF_CAP + 32);
        assert!(capped.ends_with("[diff truncated]\n"));
    }
}
