//! Session-scoped file-diff worker feeding the TUI chat stream.
//!
//! The tracker itself is synchronous git I/O, so it lives on a dedicated
//! thread that owns it exclusively: request messages (snapshot requests at
//! `ToolCallEnd`, root additions on workspace grants) arrive over one
//! channel, produced diffs leave over another. Serializing every snapshot
//! through a single worker keeps per-call attribution correct — two
//! snapshots can never race and double-report the same change.

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
    },
    /// A rewind restored worktrees to a checkpoint. `diffs` carry the
    /// revert diff (pre-rewind → restored) per affected root; empty means
    /// the checkpoint was missing or the tree already matched.
    Rewound {
        /// Tool call whose checkpoint was restored.
        tool_call_id: String,
        /// Per-root revert diffs (empty roots omitted).
        diffs: Vec<RootDiff>,
    },
}

enum Request {
    /// Start tracking an additional directory.
    AddRoot(TrackedRootSpec),
    /// Snapshot all roots and attribute any diff to this tool call.
    Snapshot(String),
    /// Restore all roots to their state before this tool call.
    Rewind(String),
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
                                    })
                                    .is_err()
                            {
                                return;
                            }
                        }
                        Request::Rewind(tool_call_id) => {
                            // Send the outcome even when nothing reverted:
                            // the TUI needs a reply either way to report
                            // "no checkpoint" back to the user.
                            let diffs = tracker.rewind(&tool_call_id);
                            if result_tx
                                .send_blocking(Outcome::Rewound {
                                    tool_call_id,
                                    diffs,
                                })
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

    /// Ask the worker to restore all roots to their state before
    /// `tool_call_id`.
    pub fn request_rewind(&self, tool_call_id: &str) {
        if self
            .requests
            .try_send(Request::Rewind(tool_call_id.to_string()))
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
