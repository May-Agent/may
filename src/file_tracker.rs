//! Per-session file-change tracking backed by detached git trees.
//!
//! For every writable root the tracker keeps a shadow repository: `GIT_DIR`
//! under the session `state_dir`, `--work-tree` pointed at the root. The
//! baseline is a tree hash produced by `git add -A` + `git write-tree`; no
//! commits are created, so no author identity is needed and the user's own
//! `.git` (if any) is never touched. `git diff` between the previous and
//! current tree yields the changes attributable to one terminal call.
//!
//! The current baseline is persisted in a `diff-base` file inside each
//! shadow repository, so reopening a session continues tracking from the
//! state the last session left behind instead of re-attributing old changes.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A writable directory to track, with repo-local exclude patterns.
#[derive(Debug, Clone)]
pub struct TrackedRootSpec {
    /// Absolute path of the directory to watch.
    pub worktree: PathBuf,
    /// Additional entries for the shadow repo's `info/exclude`
    /// (paths relative to `worktree`).
    pub excludes: Vec<String>,
}

impl TrackedRootSpec {
    /// Track `worktree` with no extra excludes (the root's own
    /// `.gitignore` rules still apply).
    #[must_use]
    pub const fn new(worktree: PathBuf) -> Self {
        Self {
            worktree,
            excludes: Vec::new(),
        }
    }

    /// Add an `info/exclude` entry relative to the worktree root.
    #[must_use]
    pub fn exclude(mut self, pattern: impl Into<String>) -> Self {
        self.excludes.push(pattern.into());
        self
    }
}

/// A file diff attributed to one terminal call, in one tracked root.
#[derive(Debug, Clone)]
pub struct RootDiff {
    /// Tracked root the diff is relative to.
    pub root: PathBuf,
    /// Unified `git diff` output between the previous and current trees.
    pub text: String,
    /// Number of files touched by this diff.
    pub files: usize,
    /// Added lines across all files.
    pub additions: usize,
    /// Deleted lines across all files.
    pub deletions: usize,
}

/// Errors that prevent tracking from being set up.
#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    /// The `git` binary is unavailable.
    #[error("git is not available: {0}")]
    GitUnavailable(String),
    /// Filesystem error while preparing tracker state.
    #[error("tracker state error: {0}")]
    Io(#[from] io::Error),
    /// A git command failed.
    #[error("git {args} failed in {worktree}: {stderr}")]
    Command {
        /// Arguments passed to git.
        args: String,
        /// Worktree the command targeted.
        worktree: String,
        /// Captured stderr.
        stderr: String,
    },
}

/// Name of the file inside each shadow `GIT_DIR` holding the last tree hash.
const BASE_FILE: &str = "diff-base";

#[derive(Debug)]
struct TrackedRoot {
    worktree: PathBuf,
    git_dir: PathBuf,
    tree: String,
}

/// Session-scoped tracker. Owns no background work itself — callers drive
/// [`FileChangeTracker::snapshot`] at call boundaries.
#[derive(Debug)]
pub struct FileChangeTracker {
    state_dir: PathBuf,
    roots: Vec<TrackedRoot>,
}

impl FileChangeTracker {
    /// Create a tracker under `state_dir` watching `roots`.
    ///
    /// Fails fast when `git` cannot be executed or the state directory
    /// cannot be created; individual roots that fail to initialize are
    /// skipped with a warning.
    ///
    /// # Errors
    /// Returns [`TrackerError::GitUnavailable`] when `git --version` fails,
    /// or [`TrackerError::Io`] when `state_dir` cannot be created.
    pub fn new(state_dir: PathBuf, roots: Vec<TrackedRootSpec>) -> Result<Self, TrackerError> {
        check_git_available()?;
        std::fs::create_dir_all(&state_dir)?;
        let mut tracker = Self {
            state_dir,
            roots: Vec::new(),
        };
        for spec in roots {
            let worktree = spec.worktree.clone();
            if let Err(error) = tracker.insert_root(spec) {
                tracing::warn!(
                    worktree = %worktree.display(),
                    %error,
                    "skipping untrackable root"
                );
            }
        }
        Ok(tracker)
    }

    /// Begin tracking an additional root (e.g. a newly granted workdir).
    ///
    /// Returns `false` when the root is already covered by an existing
    /// tracked root.
    ///
    /// # Errors
    /// Returns [`TrackerError`] when the shadow repository cannot be
    /// initialized or the baseline tree cannot be written.
    pub fn add_root(&mut self, spec: TrackedRootSpec) -> Result<bool, TrackerError> {
        let normalized = normalize_path(&spec.worktree);
        let covered = self
            .roots
            .iter()
            .any(|root| normalized == root.worktree || normalized.starts_with(&root.worktree));
        if covered {
            return Ok(false);
        }
        self.insert_root(TrackedRootSpec {
            worktree: normalized,
            ..spec
        })?;
        Ok(true)
    }

    /// Advance every root's baseline and return the diffs accumulated since
    /// the previous snapshot. Roots whose git operations fail are logged and
    /// skipped; their baselines stay put so the next snapshot still sees the
    /// accumulated change.
    #[must_use]
    pub fn snapshot(&mut self) -> Vec<RootDiff> {
        let mut diffs = Vec::new();
        for root in &mut self.roots {
            match snapshot_root(root) {
                Ok(Some(diff)) => diffs.push(diff),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        worktree = %root.worktree.display(),
                        %error,
                        "file diff snapshot failed for root"
                    );
                }
            }
        }
        diffs
    }

    fn insert_root(&mut self, spec: TrackedRootSpec) -> Result<(), TrackerError> {
        let spec = TrackedRootSpec {
            worktree: normalize_path(&spec.worktree),
            ..spec
        };
        let index = self.roots.len();
        let git_dir = self.state_dir.join(format!("{index}.git"));
        if !git_dir.join("HEAD").exists() {
            init_bare(&git_dir).map_err(|stderr| TrackerError::Command {
                args: format!("init --bare -b main {}", git_dir.display()),
                worktree: spec.worktree.display().to_string(),
                stderr,
            })?;
        }
        write_excludes(&git_dir, &spec.excludes)?;
        let tree = match std::fs::read_to_string(git_dir.join(BASE_FILE)) {
            Ok(saved) => saved.trim().to_string(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let tree = write_tree(&git_dir, &spec.worktree)?;
                std::fs::write(git_dir.join(BASE_FILE), &tree)?;
                tree
            }
            Err(error) => return Err(TrackerError::Io(error)),
        };
        self.roots.push(TrackedRoot {
            worktree: spec.worktree,
            git_dir,
            tree,
        });
        Ok(())
    }
}

/// Canonicalize `path` through its deepest existing ancestor, so paths that
/// do not exist yet still compare correctly against tracked roots
/// (`/var/...` vs `/private/var/...` on macOS and similar symlinked
/// prefixes).
fn normalize_path(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(resolved) = cursor.canonicalize() {
            return missing
                .iter()
                .rev()
                .fold(resolved, |acc, part| acc.join(part));
        }
        let Some(parent) = cursor.parent() else {
            return path.to_path_buf();
        };
        match cursor.file_name() {
            Some(name) => missing.push(name.to_os_string()),
            None => return path.to_path_buf(),
        }
        cursor = parent;
    }
}

fn check_git_available() -> Result<(), TrackerError> {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|_| ())
        .map_err(|error| TrackerError::GitUnavailable(error.to_string()))
}

/// Run `git` with the shadow repo + worktree attached, returning trimmed
/// stdout. `stderr` becomes the error payload on non-zero exit.
fn run_git(git_dir: &Path, worktree: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .arg(format!("--git-dir={}", git_dir.display()))
        .arg(format!("--work-tree={}", worktree.display()))
        .args(args);
    let output = command
        .output()
        .map_err(|error| format!("spawn failed: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Create a bare shadow repository. `git init` rejects the
/// `--git-dir`/`--work-tree` flag form for a not-yet-existing dir, so the
/// path goes as a positional argument instead.
fn init_bare(git_dir: &Path) -> Result<(), String> {
    let output = Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(git_dir)
        .output()
        .map_err(|error| format!("spawn failed: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn write_tree(git_dir: &Path, worktree: &Path) -> Result<String, TrackerError> {
    run_git(git_dir, worktree, &["add", "-A"]).map_err(|stderr| TrackerError::Command {
        args: "add -A".to_string(),
        worktree: worktree.display().to_string(),
        stderr,
    })?;
    run_git(git_dir, worktree, &["write-tree"]).map_err(|stderr| TrackerError::Command {
        args: "write-tree".to_string(),
        worktree: worktree.display().to_string(),
        stderr,
    })
}

fn write_excludes(git_dir: &Path, excludes: &[String]) -> Result<(), TrackerError> {
    if excludes.is_empty() {
        return Ok(());
    }
    let info_dir = git_dir.join("info");
    std::fs::create_dir_all(&info_dir)?;
    let mut content = excludes.join("\n");
    content.push('\n');
    std::fs::write(info_dir.join("exclude"), content)?;
    Ok(())
}

/// Refresh `root`'s index and return the diff against its stored baseline,
/// or `None` when the tree is unchanged.
fn snapshot_root(root: &mut TrackedRoot) -> Result<Option<RootDiff>, TrackerError> {
    let tree = write_tree(&root.git_dir, &root.worktree)?;
    if tree == root.tree {
        return Ok(None);
    }
    let text =
        run_git(&root.git_dir, &root.worktree, &["diff", &root.tree, &tree]).map_err(|stderr| {
            TrackerError::Command {
                args: format!("diff {} {tree}", root.tree),
                worktree: root.worktree.display().to_string(),
                stderr,
            }
        })?;
    std::fs::write(root.git_dir.join(BASE_FILE), &tree)?;
    root.tree = tree;
    if text.is_empty() {
        // Tree hash changed without a visible diff (e.g. mode-only changes
        // on filesystems that normalize them) — nothing to show.
        return Ok(None);
    }
    let (files, additions, deletions) = diff_stats(&text);
    Ok(Some(RootDiff {
        root: root.worktree.clone(),
        text,
        files,
        additions,
        deletions,
    }))
}

/// Count files, added and removed lines in a unified diff body.
fn diff_stats(text: &str) -> (usize, usize, usize) {
    let mut files = 0;
    let mut additions = 0;
    let mut deletions = 0;
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            files += 1;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            additions += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions += 1;
        }
    }
    (files, additions, deletions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn spec(dir: &Path) -> TrackedRootSpec {
        TrackedRootSpec::new(dir.to_path_buf())
    }

    fn tracker() -> (tempfile::TempDir, PathBuf, FileChangeTracker) {
        let temp = tempfile::tempdir().expect("tempdir");
        let worktree = temp.path().join("work");
        fs::create_dir_all(&worktree).expect("workdir");
        let state_dir = temp.path().join("state");
        let tracker =
            FileChangeTracker::new(state_dir, vec![spec(&worktree)]).expect("tracker init");
        (temp, worktree, tracker)
    }

    #[test]
    fn new_file_produces_diff() {
        let (_temp, worktree, mut tracker) = tracker();
        fs::write(worktree.join("hello.txt"), "world\n").expect("write");

        let diffs = tracker.snapshot();

        assert_eq!(diffs.len(), 1);
        let diff = &diffs[0];
        assert_eq!(diff.files, 1);
        assert_eq!(diff.additions, 1);
        assert_eq!(diff.deletions, 0);
        assert!(diff.text.contains("new file"));
        assert!(diff.text.contains("+world"));
    }

    #[test]
    fn modify_file_produces_diff() {
        let (_temp, worktree, mut tracker) = tracker();
        fs::write(worktree.join("a.txt"), "one\n").expect("write");
        assert_eq!(tracker.snapshot().len(), 1);

        fs::write(worktree.join("a.txt"), "one\ntwo\n").expect("rewrite");
        let diffs = tracker.snapshot();

        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].additions, 1);
        assert_eq!(diffs[0].deletions, 0);
        assert!(diffs[0].text.contains("+two"));
    }

    #[test]
    fn delete_file_produces_diff() {
        let (_temp, worktree, mut tracker) = tracker();
        fs::write(worktree.join("gone.txt"), "bye\n").expect("write");
        assert_eq!(tracker.snapshot().len(), 1);

        fs::remove_file(worktree.join("gone.txt")).expect("remove");
        let diffs = tracker.snapshot();

        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].deletions, 1);
        assert!(diffs[0].text.contains("deleted file"));
        assert!(diffs[0].text.contains("-bye"));
    }

    #[test]
    fn clean_tree_produces_no_diff() {
        let (_temp, _worktree, mut tracker) = tracker();
        assert!(tracker.snapshot().is_empty());
        assert!(tracker.snapshot().is_empty());
    }

    #[test]
    fn gitignored_files_are_not_tracked() {
        let (_temp, worktree, mut tracker) = tracker();
        fs::write(worktree.join(".gitignore"), "ignored/\n").expect("gitignore");
        assert_eq!(tracker.snapshot().len(), 1); // the .gitignore itself

        let ignored = worktree.join("ignored");
        fs::create_dir_all(&ignored).expect("ignored dir");
        fs::write(ignored.join("x.txt"), "nope\n").expect("write ignored");

        assert!(tracker.snapshot().is_empty());
    }

    #[test]
    fn add_root_dedupes_and_tracks() {
        let (temp, worktree, mut tracker) = tracker();
        let nested = worktree.join("nested");
        assert!(!tracker.add_root(spec(&nested)).expect("add nested"));

        let other = temp.path().join("other");
        fs::create_dir_all(&other).expect("other dir");
        assert!(tracker.add_root(spec(&other)).expect("add other"));
        assert_eq!(tracker.roots.len(), 2);

        fs::write(other.join("o.txt"), "data\n").expect("write other");
        let diffs = tracker.snapshot();
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].root, normalize_path(&other));
    }

    #[test]
    fn baseline_persists_across_tracker_instances() {
        let temp = tempfile::tempdir().expect("tempdir");
        let worktree = temp.path().join("work");
        fs::create_dir_all(&worktree).expect("workdir");
        let state_dir = temp.path().join("state");
        fs::write(worktree.join("old.txt"), "before\n").expect("write");
        {
            let mut tracker =
                FileChangeTracker::new(state_dir.clone(), vec![spec(&worktree)]).expect("init");
            fs::write(worktree.join("old.txt"), "changed\n").expect("rewrite");
            assert_eq!(tracker.snapshot().len(), 1);
        }

        // A new tracker must not re-report the change the first one saw.
        let mut tracker = FileChangeTracker::new(state_dir, vec![spec(&worktree)]).expect("reinit");
        assert!(tracker.snapshot().is_empty());
    }
}
