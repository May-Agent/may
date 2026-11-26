//! Daemon runtime for Fusion mode's persistent companion sidekick.
//!
//! Manages hidden companion sessions linked via `session_companions`, drives
//! blocking, asynchronous, and mid-run steered `sidekick` turns through the
//! daemon's standard `run_agent_loop` + `ParkedAgents` + `history.db` pipeline,
//! computes incremental unified `git diff`s against pre-handoff working-tree
//! baselines without running host git on agent-writable clones, and coordinates
//! the exclusive workspace write lease during async sidekick turns.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use similar::TextDiff;
use tokio::sync::watch;

use crate::model_selector::ModelSelector;
use crate::session::{
    AcquireError, MayHomePaths, PendingMessageState, PendingUserMessage, PendingUserMessageMode,
    SandboxResourceLayout, SessionPaths, SessionStore,
};
use crate::state::DaemonState;
use may_agent_core::fusion::{
    FusionPairRegistry, FusionService, SIDEKICK_ROLE, SidekickExecutor, SidekickOutcome,
    SidekickRequest, render_sidekick_completed,
};

/// Maximum file size diffed in full for incremental working-tree diffs (2 MiB).
const MAX_DIFF_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Maximum characters of unified diff returned inline to the lead.
const MAX_DIFF_OUTPUT_CHARS: usize = 64 * 1024;

/// Directories skipped while snapshotting repository working trees for diffs.
const SKIPPED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "__pycache__",
    ".next",
    ".turbo",
];

type RunResult = Result<(String, String), String>;

/// Daemon-backed [`SidekickExecutor`] managing persistent companion sessions.
#[derive(Clone)]
pub struct DaemonSidekickExecutor {
    state: Arc<DaemonState>,
    pairs: FusionPairRegistry,
    active_runs: Arc<Mutex<HashMap<String, watch::Receiver<Option<RunResult>>>>>,
}

impl std::fmt::Debug for DaemonSidekickExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonSidekickExecutor")
            .finish_non_exhaustive()
    }
}

impl DaemonSidekickExecutor {
    /// Builds a [`FusionService`] backed by `state`.
    #[must_use]
    pub fn service(state: Arc<DaemonState>) -> FusionService {
        let pairs = FusionPairRegistry::default();
        let executor = Arc::new(Self {
            state,
            pairs: pairs.clone(),
            active_runs: Arc::new(Mutex::new(HashMap::new())),
        });
        FusionService::new(pairs, executor)
    }

    async fn ensure_companion_session(
        &self,
        parent_session_id: &str,
        sidekick_model: &ModelSelector,
    ) -> Result<(String, PathBuf), String> {
        let db = self.state.db();
        let parent = db
            .get_session(parent_session_id)
            .await
            .map_err(|error| format!("failed to load parent session: {error}"))?
            .ok_or_else(|| format!("parent session '{parent_session_id}' not found"))?;
        let store = SessionStore::new(MayHomePaths::new(self.state.may_dir.clone()));
        let parent_sandbox_dir = store.sandbox_dir(parent_session_id, parent.profile_id.as_deref());
        let model_str = sidekick_model.to_string();

        let existing = db
            .companion_session_for(parent_session_id, SIDEKICK_ROLE)
            .await
            .map_err(|error| format!("failed to look up companion session: {error}"))?;

        let sidekick_id = if let Some(id) = existing
            && db
                .session_exists(&id)
                .await
                .map_err(|error| format!("failed to check companion session: {error}"))?
        {
            db.update_session(&id, None, Some(&model_str), None)
                .await
                .map_err(|error| format!("failed to update companion model: {error}"))?;
            if let Some(cwd) = parent.cwd.as_deref() {
                let _ = db.set_session_cwd(&id, cwd).await;
            }
            if let Some(repo) = parent.repo.as_deref() {
                let _ = db.set_session_repo(&id, Some(repo)).await;
            }
            id
        } else {
            let candidate = format!("{parent_session_id}-sidekick");
            let id = if db
                .session_exists(&candidate)
                .await
                .map_err(|error| format!("failed to check candidate companion id: {error}"))?
            {
                format!(
                    "{parent_session_id}-sidekick-{}",
                    jiff::Timestamp::now().as_millisecond()
                )
            } else {
                candidate
            };
            let title = format!("Sidekick ({parent_session_id})");
            db.create_session_in_profile(
                &id,
                Some(&title),
                Some(&model_str),
                parent.profile_id.as_deref(),
            )
            .await
            .map_err(|error| format!("failed to create companion session: {error}"))?;
            if let Some(cwd) = parent.cwd.as_deref() {
                db.set_session_cwd(&id, cwd)
                    .await
                    .map_err(|error| format!("failed to set companion cwd: {error}"))?;
            }
            if let Some(repo) = parent.repo.as_deref() {
                db.set_session_repo(&id, Some(repo))
                    .await
                    .map_err(|error| format!("failed to set companion repo: {error}"))?;
            }
            db.link_companion_session(&id, parent_session_id, SIDEKICK_ROLE)
                .await
                .map_err(|error| format!("failed to link companion session: {error}"))?;
            id
        };

        let layout = match parent.profile_id.as_deref() {
            Some(profile_id) => match db.get_profile(profile_id).await {
                Ok(Some(profile))
                    if profile.environment
                        == crate::db::ProfileEnvironmentKind::Container.as_str() =>
                {
                    SandboxResourceLayout::ContainerMountPoints
                }
                _ => SandboxResourceLayout::Symlinked,
            },
            None => SandboxResourceLayout::Symlinked,
        };
        store
            .create_session_dirs(&sidekick_id, parent.profile_id.as_deref(), layout)
            .await
            .map_err(|error| format!("failed to prepare companion sandbox: {error}"))?;
        let _ = self.pairs.link_companion(parent_session_id, &sidekick_id);
        Ok((sidekick_id, parent_sandbox_dir))
    }

    async fn enqueue_steer(&self, sidekick_id: &str, prompt: String) -> Result<(), String> {
        let message = PendingUserMessage {
            id: None,
            state: PendingMessageState::default(),
            error: None,
            content: prompt,
            attachments: Vec::new(),
            hidden_context_xml: None,
            created_at: jiff::Timestamp::now().to_string(),
            delivery_mode: PendingUserMessageMode::Steer,
        };
        self.state
            .runtime_service
            .enqueue(sidekick_id, message, PendingUserMessageMode::Steer)
            .await
            .map_err(|error| format!("failed to steer running sidekick: {error}"))?;
        Ok(())
    }

    fn workspace_roots_for_diff(parent_sandbox_dir: &Path) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        let repos_dir = parent_sandbox_dir.join("repos");
        if repos_dir.exists() {
            roots.push(repos_dir);
        }
        let ws_cfg_path = SessionPaths::new(parent_sandbox_dir.to_path_buf()).workspace_config();
        if let Ok(ws_cfg) = crate::workspace::WorkspaceConfig::load(&ws_cfg_path) {
            for entry in ws_cfg.workdirs {
                let path = entry.path;
                if !path.starts_with(parent_sandbox_dir) && path.exists() {
                    roots.push(path);
                }
            }
        }
        roots
    }

    async fn execute_inner(&self, request: SidekickRequest) -> Result<SidekickOutcome, String> {
        let (sidekick_id, parent_sandbox_dir) = self
            .ensure_companion_session(&request.parent_session_id, &request.sidekick_model)
            .await?;

        let guard = match self
            .state
            .runtime_service
            .runs()
            .acquire(&sidekick_id)
            .await
        {
            Ok(guard) => guard,
            Err(AcquireError::Busy(_)) => {
                // Mid-run steer: enqueue without aborting mid-turn state.
                self.enqueue_steer(&sidekick_id, request.prompt).await?;
                if request.async_run {
                    return Ok(SidekickOutcome::Steered);
                }
                let rx = self
                    .active_runs
                    .lock()
                    .expect("active runs mutex never poisoned")
                    .get(&sidekick_id)
                    .cloned();
                if let Some(mut rx) = rx {
                    loop {
                        let current = rx.borrow().clone();
                        if let Some(outcome) = current {
                            return outcome
                                .map(|(report, diff)| SidekickOutcome::Completed { report, diff });
                        }
                        if rx.changed().await.is_err() {
                            return Ok(SidekickOutcome::Steered);
                        }
                    }
                }
                return Ok(SidekickOutcome::Steered);
            }
            Err(error) => {
                return Err(format!("failed to acquire companion sidekick run: {error}"));
            }
        };

        let diff_roots = Self::workspace_roots_for_diff(&parent_sandbox_dir);
        let baseline_roots = diff_roots.clone();
        let baseline = blocking::unblock(move || capture_workspace_baseline(&baseline_roots)).await;

        let (result_tx, result_rx) = watch::channel(None);
        self.active_runs
            .lock()
            .expect("active runs mutex never poisoned")
            .insert(sidekick_id.clone(), result_rx);

        if request.async_run {
            let pair = self.pairs.for_parent(&request.parent_session_id);
            let tasks_path = SessionPaths::new(parent_sandbox_dir.clone()).tasks_path();
            let lease_guard = pair.write_lease(&parent_sandbox_dir, &tasks_path).acquire();
            let state = Arc::clone(&self.state);
            let active_runs = Arc::clone(&self.active_runs);
            let parent_id = request.parent_session_id.clone();
            let sidekick = sidekick_id.clone();
            let prompt = request.prompt;
            self.state.tokio_handle.spawn(async move {
                let run_res =
                    super::chat::run_companion_turn(&state, &sidekick, prompt, guard).await;
                // Release the workspace write lease before delivering completion so the lead
                // can act immediately once the completion arrives.
                drop(lease_guard);
                let after =
                    blocking::unblock(move || capture_workspace_baseline(&diff_roots)).await;
                let diff = render_incremental_diff(&baseline, &after);
                let completed: RunResult = match run_res {
                    Ok(report) => Ok((report, diff)),
                    Err(error) => Err(error),
                };
                let _ = result_tx.send(Some(completed.clone()));
                active_runs
                    .lock()
                    .expect("active runs mutex never poisoned")
                    .remove(&sidekick);
                let notice = match completed {
                    Ok((report, diff)) => render_sidekick_completed(report.trim(), diff.trim()),
                    Err(error) => {
                        render_sidekick_completed(&format!("Sidekick turn failed: {error}"), "")
                    }
                };
                if let Err(error) = state
                    .session_prompts
                    .deliver_hidden(&parent_id, notice)
                    .await
                {
                    tracing::warn!(
                        parent_session_id = %parent_id,
                        sidekick_session_id = %sidekick,
                        %error,
                        "failed to deliver async sidekick completion to lead"
                    );
                }
            });
            return Ok(SidekickOutcome::StartedAsync);
        }

        let run_res =
            super::chat::run_companion_turn(&self.state, &sidekick_id, request.prompt, guard).await;
        let after = blocking::unblock(move || capture_workspace_baseline(&diff_roots)).await;
        let diff = render_incremental_diff(&baseline, &after);
        let completed: RunResult = match run_res {
            Ok(report) => Ok((report, diff)),
            Err(error) => Err(error),
        };
        let _ = result_tx.send(Some(completed.clone()));
        self.active_runs
            .lock()
            .expect("active runs mutex never poisoned")
            .remove(&sidekick_id);
        completed.map(|(report, diff)| SidekickOutcome::Completed { report, diff })
    }
}

impl SidekickExecutor for DaemonSidekickExecutor {
    fn execute<'a>(
        &'a self,
        request: SidekickRequest,
    ) -> Pin<Box<dyn Future<Output = Result<SidekickOutcome, String>> + Send + 'a>> {
        Box::pin(async move { self.execute_inner(request).await })
    }
}

/// Snapshot of workspace repository file contents prior to a sidekick handoff.
#[derive(Debug, Default)]
pub struct WorkspaceBaseline {
    files: BTreeMap<String, Vec<u8>>,
}

pub fn capture_workspace_baseline(roots: &[PathBuf]) -> WorkspaceBaseline {
    let mut files = BTreeMap::new();
    for root in roots {
        walk_baseline(root, root, &mut files);
    }
    WorkspaceBaseline { files }
}

#[allow(
    clippy::disallowed_methods,
    reason = "invoked inside blocking::unblock to snapshot working trees for incremental diffs"
)]
fn walk_baseline(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && SKIPPED_DIRS.contains(&name)
            {
                continue;
            }
            walk_baseline(base, &path, out);
        } else if ft.is_file()
            && let Ok(meta) = entry.metadata()
            && meta.len() <= MAX_DIFF_FILE_BYTES
            && let Ok(bytes) = std::fs::read(&path)
        {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(path.as_path())
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(rel, bytes);
        }
    }
}

/// Computes an incremental unified `git diff` between `before` and `after` working-tree snapshots.
#[must_use]
pub fn render_incremental_diff(before: &WorkspaceBaseline, after: &WorkspaceBaseline) -> String {
    use std::fmt::Write as _;

    let mut diff = String::new();
    let mut all_paths: Vec<&String> = before.files.keys().chain(after.files.keys()).collect();
    all_paths.sort();
    all_paths.dedup();

    for rel in all_paths {
        let old_bytes = before.files.get(rel);
        let new_bytes = after.files.get(rel);
        if old_bytes == new_bytes {
            continue;
        }
        let _ = writeln!(diff, "diff --git a/{rel} b/{rel}");
        match (old_bytes, new_bytes) {
            (None, Some(new_b)) => {
                if let Ok(new_text) = std::str::from_utf8(new_b) {
                    let text_diff = TextDiff::from_lines("", new_text);
                    let _ = write!(
                        diff,
                        "{}",
                        text_diff
                            .unified_diff()
                            .context_radius(3)
                            .header("/dev/null", &format!("b/{rel}"))
                    );
                } else {
                    let _ = writeln!(diff, "Binary files /dev/null and b/{rel} differ");
                }
            }
            (Some(old_b), None) => {
                if let Ok(old_text) = std::str::from_utf8(old_b) {
                    let text_diff = TextDiff::from_lines(old_text, "");
                    let _ = write!(
                        diff,
                        "{}",
                        text_diff
                            .unified_diff()
                            .context_radius(3)
                            .header(&format!("a/{rel}"), "/dev/null")
                    );
                } else {
                    let _ = writeln!(diff, "Binary files a/{rel} and /dev/null differ");
                }
            }
            (Some(old_b), Some(new_b)) => {
                match (std::str::from_utf8(old_b), std::str::from_utf8(new_b)) {
                    (Ok(old_text), Ok(new_text)) => {
                        let text_diff = TextDiff::from_lines(old_text, new_text);
                        let _ = write!(
                            diff,
                            "{}",
                            text_diff
                                .unified_diff()
                                .context_radius(3)
                                .header(&format!("a/{rel}"), &format!("b/{rel}"))
                        );
                    }
                    _ => {
                        let _ = writeln!(diff, "Binary files a/{rel} and b/{rel} differ");
                    }
                }
            }
            (None, None) => {}
        }
        if diff.len() >= MAX_DIFF_OUTPUT_CHARS {
            let cut = diff.floor_char_boundary(MAX_DIFF_OUTPUT_CHARS);
            diff.truncate(cut);
            diff.push_str("\n... (diff truncated)\n");
            break;
        }
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_diff_captures_only_delta_against_dirty_baseline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let file = repo.join("lib.rs");
        std::fs::write(&file, "fn a() {}\nfn b() {}\n").expect("write baseline");

        let before = capture_workspace_baseline(std::slice::from_ref(&repo));

        std::fs::write(&file, "fn a() {}\nfn b() { 42 }\n").expect("modify");
        std::fs::write(repo.join("added.rs"), "pub const X: i32 = 1;\n").expect("add");
        let after = capture_workspace_baseline(std::slice::from_ref(&repo));

        let diff = render_incremental_diff(&before, &after);
        assert!(diff.contains("diff --git a/lib.rs b/lib.rs"), "{diff}");
        assert!(diff.contains("-fn b() {}"), "{diff}");
        assert!(diff.contains("+fn b() { 42 }"), "{diff}");
        assert!(diff.contains("diff --git a/added.rs b/added.rs"), "{diff}");
        assert!(diff.contains("+pub const X: i32 = 1;"), "{diff}");
    }
}
