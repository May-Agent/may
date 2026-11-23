//! Request-time `ContextSource` for active swarm protocol alerts and unread hints.
//!
//! Injects the swarm rules once on entry, re-injects after context compaction
//! while the board remains active, and appends unread message hints without
//! waking idle sessions.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use aither_agent::{ContextBlock, ContextSource, Moment, SourceId};
use askama::Template;

use super::{ActiveBoard, SwarmService};
use crate::agent_link::AgentStatus;

const SOURCE_ID: &str = "swarm";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SeenEpoch {
    workdir: Option<String>,
    epoch: u64,
}

/// Shared handle keeping a warm agent's delivered `(workdir, board_epoch)`
/// synchronized across checkpoint restores and compactions.
#[derive(Debug, Clone, Default)]
pub struct SwarmSeenState {
    inner: Arc<Mutex<SeenEpoch>>,
}

impl SwarmSeenState {
    /// Creates an empty seen state (`epoch = 0`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restores the state persisted in `swarm_session_state` on checkpoint resume.
    ///
    /// Prevents repeating the protocol alert until compaction or a new `board_epoch`.
    pub fn restore(&self, workdir: Option<String>, epoch: u64) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.workdir = workdir;
        guard.epoch = epoch;
    }

    fn get(&self) -> SeenEpoch {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set(&self, workdir: Option<String>, epoch: u64) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.workdir = workdir;
        guard.epoch = epoch;
    }
}

#[derive(Template)]
#[template(path = "alert_swarm.txt", escape = "none")]
struct SwarmAlertTemplate<'a> {
    workdir: &'a str,
    board_epoch: u64,
    peers: &'a [AgentStatus],
}

#[derive(Template)]
#[template(path = "alert_swarm_dissolved.txt", escape = "none")]
struct SwarmDissolvedTemplate<'a> {
    workdir: &'a str,
}

#[derive(Template)]
#[template(path = "alert_swarm_unread.txt", escape = "none")]
struct SwarmUnreadTemplate {
    board_epoch: u64,
    unread_count: u64,
    first_seq: u64,
    last_seq: u64,
}

/// Request-time swarm protocol alert and unread-board indicator.
#[derive(Debug)]
pub struct SwarmConditions {
    swarm: SwarmService,
    session_id: String,
    seen: SwarmSeenState,
}

impl SwarmConditions {
    /// Builds the context source for `session_id` sharing `seen` with the host
    /// `MayAgent`.
    #[must_use]
    pub fn new(swarm: SwarmService, session_id: impl Into<String>, seen: SwarmSeenState) -> Self {
        Self {
            swarm,
            session_id: session_id.into(),
            seen,
        }
    }

    fn render_protocol_alert(&self, board: &ActiveBoard) -> ContextBlock {
        let peers: Vec<AgentStatus> = board
            .members
            .iter()
            .filter(|m| m.session_id != self.session_id)
            .cloned()
            .collect();
        let display_dir = crate::fs_utils::display_path(Path::new(&board.workdir));
        let text = SwarmAlertTemplate {
            workdir: &display_dir,
            board_epoch: board.board_epoch,
            peers: &peers,
        }
        .render()
        .unwrap_or_default()
        .trim_end()
        .to_string();
        ContextBlock::stable("system-reminder", text)
    }

    async fn evaluate(&self, reassembled: bool, commit: bool) -> Vec<ContextBlock> {
        if commit && let Err(error) = self.swarm.reconcile().await {
            tracing::warn!(session_id = %self.session_id, %error, "failed to reconcile swarm on request");
        }

        let prior = if reassembled {
            SeenEpoch::default()
        } else {
            self.seen.get()
        };

        let mut blocks = Vec::new();

        if let Some(board) = self.swarm.active_board_for(&self.session_id) {
            let target_epoch = board.board_epoch.max(1);
            let needs_protocol = prior.epoch != target_epoch
                || prior.workdir.as_deref() != Some(board.workdir.as_str());
            if needs_protocol {
                blocks.push(self.render_protocol_alert(&board));
                if commit {
                    self.seen.set(Some(board.workdir.clone()), target_epoch);
                    if let Err(error) = self
                        .swarm
                        .db()
                        .set_swarm_told(&self.session_id, Some(&board.workdir), target_epoch)
                        .await
                    {
                        tracing::warn!(session_id = %self.session_id, %error, "failed to persist swarm told epoch");
                    }
                }
            }

            match self
                .swarm
                .db()
                .swarm_unread_status(&board.workdir, &self.session_id)
                .await
            {
                Ok(Some(status)) if status.max_seq > status.cursor => {
                    let unread_count = status.max_seq - status.cursor;
                    let text = SwarmUnreadTemplate {
                        board_epoch: status.board_epoch,
                        unread_count,
                        first_seq: status.cursor + 1,
                        last_seq: status.max_seq,
                    }
                    .render()
                    .unwrap_or_default()
                    .trim_end()
                    .to_string();
                    blocks.push(ContextBlock::tail("system-reminder", text));
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(session_id = %self.session_id, %error, "failed to read swarm unread status");
                }
            }
        } else if !reassembled && prior.epoch != 0 {
            let raw_dir = prior.workdir.as_deref().unwrap_or(".");
            let display_dir = crate::fs_utils::display_path(Path::new(raw_dir));
            let text = SwarmDissolvedTemplate {
                workdir: &display_dir,
            }
            .render()
            .unwrap_or_default()
            .trim_end()
            .to_string();
            blocks.push(ContextBlock::stable("system-reminder", text));
            if commit {
                self.seen.set(None, 0);
                if let Err(error) = self
                    .swarm
                    .db()
                    .set_swarm_told(&self.session_id, None, 0)
                    .await
                {
                    tracing::warn!(session_id = %self.session_id, %error, "failed to clear swarm told epoch");
                }
            }
        } else if reassembled && commit {
            self.seen.set(None, 0);
            if let Err(error) = self
                .swarm
                .db()
                .set_swarm_told(&self.session_id, None, 0)
                .await
            {
                tracing::warn!(session_id = %self.session_id, %error, "failed to reset swarm told epoch after reassembly");
            }
        }

        blocks
    }
}

impl ContextSource for SwarmConditions {
    fn id(&self) -> SourceId {
        SourceId::new(SOURCE_ID)
    }

    async fn blocks(&mut self, moment: Moment) -> Vec<ContextBlock> {
        let Moment::Request { reassembled } = moment else {
            return Vec::new();
        };
        self.evaluate(reassembled, true).await
    }

    async fn peek(&self, moment: Moment) -> Vec<ContextBlock> {
        let Moment::Request { reassembled } = moment else {
            return Vec::new();
        };
        self.evaluate(reassembled, false).await
    }
}
