//! Once-per-turn workspace mutation detector for Fusion lead `terminal` calls.
//!
//! When the lead is in Fusion mode and no async `sidekick` holds the exclusive
//! workspace write lease, [`WorkspaceEditTracker`] snapshots workspace repository
//! working trees before and after each lead `terminal` call (excluding `<session>/`
//! scratch files outside `<session>/repos/`, `$MAY_TASKS`, and VCS/build dirs).
//! If the lead directly mutated workspace files, a typed Askama reminder is appended
//! at most once per turn (resetting on the next `sidekick` call or new turn) without
//! blocking the edit, and read-only calls never misfire.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use aither_core::llm::ToolCallId;
use askama::Template;
use sha2::{Digest, Sha256};

use crate::command_policy::WorkspaceWriteLease;

/// Maximum file size hashed in full during workspace snapshotting; larger files fall back
/// to `(len, mtime)` metadata.
const MAX_HASH_BYTES: u64 = 4 * 1024 * 1024;

/// Directories skipped while snapshotting workspace working trees.
const SKIPPED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "__pycache__",
    ".next",
    ".turbo",
];

#[derive(Template)]
#[template(path = "fusion_lead_edit_alert.txt", escape = "none")]
struct FusionLeadEditAlert<'a> {
    changed_count: usize,
    sample_paths: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    mtime_nanos: Option<u128>,
    sha256: Option<[u8; 32]>,
}

type TreeSnapshot = BTreeMap<PathBuf, FileStamp>;

#[derive(Debug)]
struct PendingCallSnapshot {
    sidekick_epoch: u64,
    before: TreeSnapshot,
}

#[derive(Debug)]
struct WorkspaceEditTrackerInner {
    sandbox_dir: PathBuf,
    repos_dir: PathBuf,
    tasks_path: PathBuf,
    lease: WorkspaceWriteLease,
    enabled: AtomicBool,
    alerted_this_turn: AtomicBool,
    sidekick_epoch: AtomicU64,
    extra_roots: Mutex<Vec<PathBuf>>,
    pending: Mutex<HashMap<ToolCallId, PendingCallSnapshot>>,
}

/// Shared tracker watching lead `terminal` calls for direct workspace mutations.
#[derive(Debug, Clone)]
pub struct WorkspaceEditTracker {
    inner: Arc<WorkspaceEditTrackerInner>,
}

impl WorkspaceEditTracker {
    /// Creates a workspace edit tracker for a lead session rooted at `sandbox_dir`.
    #[must_use]
    pub fn new(
        sandbox_dir: PathBuf,
        tasks_path: PathBuf,
        lease: WorkspaceWriteLease,
        enabled: bool,
    ) -> Self {
        let repos_dir = sandbox_dir.join("repos");
        Self {
            inner: Arc::new(WorkspaceEditTrackerInner {
                sandbox_dir,
                repos_dir,
                tasks_path,
                lease,
                enabled: AtomicBool::new(enabled),
                alerted_this_turn: AtomicBool::new(false),
                sidekick_epoch: AtomicU64::new(0),
                extra_roots: Mutex::new(Vec::new()),
                pending: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Enables or disables watching (true only while the session is in Fusion mode).
    pub fn set_enabled(&self, enabled: bool) {
        self.inner.enabled.store(enabled, Ordering::Release);
    }

    /// Replaces the list of additional writable workspace directories outside `<session>/`.
    pub fn set_extra_roots(&self, roots: Vec<PathBuf>) {
        *self
            .inner
            .extra_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = roots;
    }

    /// Resets the once-per-turn alert state at the start of a new lead turn.
    pub fn begin_turn(&self) {
        self.inner.alerted_this_turn.store(false, Ordering::Release);
    }

    /// Records that a `sidekick` IPC command ran, suppressing false lead-edit alerts
    /// for that call and resetting the once-per-turn reminder budget.
    pub fn mark_sidekick_call(&self) {
        self.inner.sidekick_epoch.fetch_add(1, Ordering::AcqRel);
        self.inner.alerted_this_turn.store(false, Ordering::Release);
    }

    fn should_watch(&self) -> bool {
        self.inner.enabled.load(Ordering::Acquire) && !self.inner.lease.is_held()
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if self.inner.repos_dir.exists() {
            roots.push(self.inner.repos_dir.clone());
        }
        let extra = self
            .inner
            .extra_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for root in extra {
            if !root.starts_with(&self.inner.sandbox_dir) && root.exists() {
                roots.push(root);
            }
        }
        roots
    }

    /// Captures a pre-call workspace snapshot for `call_id` when watching is active.
    pub async fn before_terminal(&self, call_id: ToolCallId) {
        if !self.should_watch() {
            return;
        }
        let sidekick_epoch = self.inner.sidekick_epoch.load(Ordering::Acquire);
        let roots = self.watch_roots();
        if roots.is_empty() {
            return;
        }
        let sandbox_dir = self.inner.sandbox_dir.clone();
        let repos_dir = self.inner.repos_dir.clone();
        let tasks_path = self.inner.tasks_path.clone();
        let before = blocking::unblock(move || {
            snapshot_roots(&roots, &sandbox_dir, &repos_dir, &tasks_path)
        })
        .await;
        self.inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                call_id,
                PendingCallSnapshot {
                    sidekick_epoch,
                    before,
                },
            );
    }

    /// Compares workspace state after `call_id` finishes and returns a typed reminder
    /// at most once per turn if the lead directly mutated workspace files.
    ///
    /// # Panics
    /// Panics if the embedded Askama reminder template fails to render.
    pub async fn after_terminal(&self, call_id: &ToolCallId) -> Option<String> {
        let pending = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(call_id)?;
        if !self.should_watch() {
            return None;
        }
        if self.inner.sidekick_epoch.load(Ordering::Acquire) != pending.sidekick_epoch {
            return None;
        }
        let roots = self.watch_roots();
        let sandbox_dir = self.inner.sandbox_dir.clone();
        let repos_dir = self.inner.repos_dir.clone();
        let tasks_path = self.inner.tasks_path.clone();
        let after = blocking::unblock(move || {
            snapshot_roots(&roots, &sandbox_dir, &repos_dir, &tasks_path)
        })
        .await;
        let changed = diff_snapshots(&pending.before, &after);
        if changed.is_empty() {
            return None;
        }
        if self.inner.alerted_this_turn.swap(true, Ordering::AcqRel) {
            return None;
        }
        let sample: Vec<String> = changed
            .iter()
            .take(5)
            .map(|path| {
                path.strip_prefix(&self.inner.repos_dir)
                    .unwrap_or(path.as_path())
                    .display()
                    .to_string()
            })
            .collect();
        let mut sample_paths = sample.join(", ");
        if changed.len() > sample.len() {
            use std::fmt::Write as _;
            let _ = write!(sample_paths, ", +{} more", changed.len() - sample.len());
        }
        Some(
            FusionLeadEditAlert {
                changed_count: changed.len(),
                sample_paths: &sample_paths,
            }
            .render()
            .expect("fusion lead edit alert template renders"),
        )
    }
}

#[allow(clippy::disallowed_methods)]
fn snapshot_roots(
    roots: &[PathBuf],
    sandbox_dir: &Path,
    repos_dir: &Path,
    tasks_path: &Path,
) -> TreeSnapshot {
    let mut snapshot = TreeSnapshot::new();
    for root in roots {
        walk_dir(root, sandbox_dir, repos_dir, tasks_path, &mut snapshot);
    }
    snapshot
}

#[allow(clippy::disallowed_methods)]
fn walk_dir(
    dir: &Path,
    sandbox_dir: &Path,
    repos_dir: &Path,
    tasks_path: &Path,
    out: &mut TreeSnapshot,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == tasks_path {
            continue;
        }
        if path.starts_with(sandbox_dir) && !path.starts_with(repos_dir) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && SKIPPED_DIRS.contains(&name)
            {
                continue;
            }
            walk_dir(&path, sandbox_dir, repos_dir, tasks_path, out);
        } else if file_type.is_file()
            && let Ok(meta) = entry.metadata()
        {
            let len = meta.len();
            let mtime_nanos = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos());
            let sha256 = if len <= MAX_HASH_BYTES {
                std::fs::read(&path).ok().map(|bytes| {
                    let digest = Sha256::digest(&bytes);
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(&digest);
                    hash
                })
            } else {
                None
            };
            out.insert(
                path,
                FileStamp {
                    len,
                    mtime_nanos,
                    sha256,
                },
            );
        }
    }
}

fn diff_snapshots(before: &TreeSnapshot, after: &TreeSnapshot) -> Vec<PathBuf> {
    let mut changed = Vec::new();
    for (path, after_stamp) in after {
        match before.get(path) {
            Some(before_stamp) if before_stamp == after_stamp => {}
            _ => changed.push(path.clone()),
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            changed.push(path.clone());
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fires_once_per_turn_ignores_scratch_and_reads_and_resets_on_sidekick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sandbox = dir.path().join("session");
        let repo = sandbox.join("repos/demo");
        let scratch = sandbox.join("tmp");
        let tasks = sandbox.join("tasks.md");
        std::fs::create_dir_all(&repo).expect("create repo");
        std::fs::create_dir_all(&scratch).expect("create scratch");
        let src = repo.join("main.rs");
        std::fs::write(&src, "fn main() {}\n").expect("write main.rs");

        let lease = WorkspaceWriteLease::new(&sandbox, &tasks);
        let tracker =
            WorkspaceEditTracker::new(sandbox.clone(), tasks.clone(), lease.clone(), true);

        let call_id = |name: &str| {
            serde_json::from_value::<ToolCallId>(serde_json::json!(name)).expect("call id")
        };

        // 1. Read-only terminal call: never misfires.
        let c1 = call_id("call-read");
        tracker.before_terminal(c1.clone()).await;
        let _ = std::fs::read_to_string(&src).expect("read");
        assert!(tracker.after_terminal(&c1).await.is_none());

        // 2. Scratch and tasks.md writes: ignored.
        let c2 = call_id("call-scratch");
        tracker.before_terminal(c2.clone()).await;
        std::fs::write(scratch.join("note.txt"), "note").expect("write scratch");
        std::fs::write(&tasks, "- [ ] task\n").expect("write tasks");
        assert!(tracker.after_terminal(&c2).await.is_none());

        // 3. Direct workspace edit: fires alert once.
        let c3 = call_id("call-edit-1");
        tracker.before_terminal(c3.clone()).await;
        std::fs::write(&src, "fn main() { println!(\"1\"); }\n").expect("edit 1");
        let alert = tracker.after_terminal(&c3).await;
        assert!(
            alert
                .as_deref()
                .is_some_and(|text| text.contains("fusion_workspace_edit_reminder")
                    && text.contains("demo/main.rs")),
            "{alert:?}"
        );

        // 4. Second direct workspace edit in same turn: deduplicated (no second alert).
        let c4 = call_id("call-edit-2");
        tracker.before_terminal(c4.clone()).await;
        std::fs::write(&src, "fn main() { println!(\"2\"); }\n").expect("edit 2");
        assert!(tracker.after_terminal(&c4).await.is_none());

        // 5. Calling `sidekick` resets the alert budget AND does not misfire on sidekick's own edits.
        let c5 = call_id("call-sidekick");
        tracker.before_terminal(c5.clone()).await;
        tracker.mark_sidekick_call();
        std::fs::write(&src, "fn main() { println!(\"sidekick\"); }\n").expect("sidekick edit");
        assert!(tracker.after_terminal(&c5).await.is_none());

        // 6. Direct lead edit after sidekick fires again.
        let c6 = call_id("call-edit-3");
        tracker.before_terminal(c6.clone()).await;
        std::fs::write(&src, "fn main() { println!(\"3\"); }\n").expect("edit 3");
        assert!(tracker.after_terminal(&c6).await.is_some());

        // 7. While async sidekick holds write lease, read-only lead call never misfires even if workspace changes.
        tracker.begin_turn();
        let _guard = lease.acquire();
        let c7 = call_id("call-read-while-async");
        tracker.before_terminal(c7.clone()).await;
        std::fs::write(&src, "fn main() { println!(\"async\"); }\n").expect("async edit");
        assert!(tracker.after_terminal(&c7).await.is_none());
    }
}
