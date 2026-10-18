//! Session-scoped file-diff worker feeding the TUI chat stream.
//!
//! The tracker itself is synchronous git I/O, so it lives on a dedicated
//! thread that owns it exclusively: request messages (prompt marks at
//! dispatch, snapshot requests at `ToolCallEnd`, root additions on
//! workspace grants) arrive over one channel, produced diffs leave over
//! another. Serializing every request through a single worker keeps
//! per-call attribution and checkpoint ordering correct — two snapshots
//! can never race and double-report the same change, and a prompt mark
//! always precedes the checkpoints of the turn it begins.

use std::path::PathBuf;

use async_channel::{Receiver, Sender};

use crate::file_tracker::{FileChangeTracker, RootDiff, TrackedRootSpec};

/// A finished worker operation.
#[derive(Debug)]
pub enum Outcome {
    /// Snapshot produced diffs attributable to one tool call.
    Diff {
        /// Tool call the diff belongs to.
        tool_call_id: String,
        /// Per-root diffs produced by the snapshot (empty roots omitted).
        diffs: Vec<RootDiff>,
        /// `true` for the per-call `snapshot` at call end (authoritative),
        /// `false` for a live `preview` — previews apply only while the call
        /// still runs so a late preview cannot clobber the final diff.
        final_snapshot: bool,
    },
    /// A rewind restored worktrees to a prompt checkpoint. `diffs` carry
    /// the revert diff (pre-rewind → restored) per affected root; empty
    /// means no checkpoint applied or the tree already matched.
    Rewound {
        /// Checkpoint key that was restored (`prompt:<message row id>`).
        mark_id: String,
        /// Per-root revert diffs (empty roots omitted).
        diffs: Vec<RootDiff>,
    },
}

enum Request {
    /// Start tracking an additional directory.
    AddRoot(TrackedRootSpec),
    /// Snapshot all roots and attribute any diff to this tool call.
    Snapshot(String),
    /// Record the current tree of every root under this prompt mark
    /// (`prompt:<message row id>`), taken at prompt dispatch.
    Mark(String),
    /// Diff the pending (baseline → now) state for a call still running —
    /// baselines and checkpoints stay put.
    Preview(String),
    /// Restore all roots to the state recorded under this prompt mark,
    /// falling back to the earliest checkpoint of `fallback_call_ids`
    /// when the mark is absent.
    Rewind {
        /// Prompt mark to restore (`prompt:<message row id>`).
        mark_id: String,
        /// Tool calls of the rewound turns, used when no mark exists.
        fallback_call_ids: Vec<String>,
    },
}

/// Request side of the per-session diff worker. Dropping it ends the
/// worker thread once pending requests drain.
#[derive(Debug)]
pub struct FileDiffHandle {
    session_id: String,
    requests: Sender<Request>,
    results: Receiver<Outcome>,
}

impl FileDiffHandle {
    /// Session this worker is tracking.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Spawn the worker thread. Returns `None` when the OS refuses the
    /// thread; tracker setup failures are logged inside the worker and
    /// leave the result channel quiet.
    #[must_use]
    pub fn start(
        session_id: String,
        state_dir: PathBuf,
        roots: Vec<TrackedRootSpec>,
    ) -> Option<Self> {
        let (requests, request_rx) = async_channel::unbounded::<Request>();
        let (result_tx, results) = async_channel::unbounded::<Outcome>();
        let thread_name = format!("file-diff-{session_id}");
        std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let mut tracker = match FileChangeTracker::new(state_dir, roots) {
                    Ok(tracker) => tracker,
                    Err(error) => {
                        tracing::warn!(%error, "file diff tracking disabled");
                        return;
                    }
                };
                while let Ok(request) = request_rx.recv_blocking() {
                    match request {
                        Request::AddRoot(spec) => {
                            if let Err(error) = tracker.add_root(spec) {
                                tracing::warn!(%error, "failed to track workspace dir");
                            }
                        }
                        Request::Snapshot(tool_call_id) => {
                            let diffs = tracker.snapshot(&tool_call_id);
                            if !diffs.is_empty()
                                && result_tx
                                    .send_blocking(Outcome::Diff {
                                        tool_call_id,
                                        diffs,
                                        final_snapshot: true,
                                    })
                                    .is_err()
                            {
                                return;
                            }
                        }
                        Request::Mark(mark_id) => {
                            tracker.mark_prompt(&mark_id);
                        }
                        Request::Preview(tool_call_id) => {
                            let diffs = tracker.preview();
                            if result_tx
                                .send_blocking(Outcome::Diff {
                                    tool_call_id,
                                    diffs,
                                    final_snapshot: false,
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        Request::Rewind {
                            mark_id,
                            fallback_call_ids,
                        } => {
                            // Send the outcome even when nothing reverted:
                            // the TUI needs a reply either way to report
                            // "no checkpoint" back to the user.
                            let diffs =
                                tracker.rewind(&mark_id, &fallback_call_ids.into_iter().collect());
                            if result_tx
                                .send_blocking(Outcome::Rewound { mark_id, diffs })
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            })
            .ok()?;
        Some(Self {
            session_id,
            requests,
            results,
        })
    }

    /// Ask the worker to snapshot every root and attribute changes to
    /// `tool_call_id`. The channel is unbounded, so this never blocks.
    pub fn request_snapshot(&self, tool_call_id: &str) {
        if self
            .requests
            .try_send(Request::Snapshot(tool_call_id.to_string()))
            .is_err()
        {
            tracing::debug!("file diff worker is gone; snapshot dropped");
        }
    }

    /// Ask the worker for a live preview of the pending diff — baselines do
    /// not move, so this is safe to repeat while `tool_call_id` runs.
    pub fn request_preview(&self, tool_call_id: &str) {
        if self
            .requests
            .try_send(Request::Preview(tool_call_id.to_string()))
            .is_err()
        {
            tracing::debug!("file diff worker is gone; preview dropped");
        }
    }

    /// Ask the worker to record the current tree of every root under
    /// `mark_id` (`prompt:<message row id>`), taken at prompt dispatch.
    pub fn request_mark(&self, mark_id: &str) {
        if self
            .requests
            .try_send(Request::Mark(mark_id.to_string()))
            .is_err()
        {
            tracing::debug!("file diff worker is gone; prompt mark dropped");
        }
    }

    /// Ask the worker to restore all roots to the state recorded under
    /// `mark_id`, falling back to the earliest checkpoint of
    /// `fallback_call_ids` when the mark is absent.
    pub fn request_rewind(&self, mark_id: &str, fallback_call_ids: Vec<String>) {
        if self
            .requests
            .try_send(Request::Rewind {
                mark_id: mark_id.to_string(),
                fallback_call_ids,
            })
            .is_err()
        {
            tracing::debug!("file diff worker is gone; rewind dropped");
        }
    }

    /// Ask the worker to track an additional directory.
    pub fn request_add_root(&self, spec: TrackedRootSpec) {
        if self.requests.try_send(Request::AddRoot(spec)).is_err() {
            tracing::debug!("file diff worker is gone; add_root dropped");
        }
    }

    /// Drain all finished outcomes without blocking.
    #[must_use]
    pub fn drain(&self) -> Vec<Outcome> {
        let mut outcomes = Vec::new();
        while let Ok(outcome) = self.results.try_recv() {
            outcomes.push(outcome);
        }
        outcomes
    }
}
