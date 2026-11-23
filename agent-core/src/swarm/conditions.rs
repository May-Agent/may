//! Request-time `ContextSource` for active swarm protocol alerts and unread hints.
//!
//! Injects the swarm rules once on entry, re-injects after context compaction
//! while the board remains active, and appends unread message hints without
//! waking idle sessions. When federated peer transport is attached, evaluation
//! reads the in-memory SSE-mirrored board snapshot plus local SQLite cursor
//! state with zero cross-machine RTT.

use std::sync::Arc;
use std::sync::Mutex;

use aither_agent::{ContextBlock, ContextSource, Moment, SourceId};
use askama::Template;

use super::federation::{SwarmMember, display_group_key};
use super::{ActiveBoard, SwarmService};

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
    peers: &'a [SwarmMember],
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

    fn render_protocol_alert(&self, board: &ActiveBoard, local_machine: &str) -> ContextBlock {
        let peers: Vec<SwarmMember> = board
            .members
            .iter()
            .filter(|m| !(m.machine == local_machine && m.session_id == self.session_id))
            .cloned()
            .collect();
        let display_dir = display_group_key(&board.workdir);
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
        // Only run inline SQLite reconciliation when no cross-machine transport is
        // attached. With peer transport enabled, background reconciliation and
        // `/api/events?topics=swarm` SSE mirrors maintain the snapshot so model
        // requests pay zero network RTT.
        if commit
            && !self.swarm.has_transport()
            && let Err(error) = self.swarm.reconcile().await
        {
            tracing::warn!(session_id = %self.session_id, %error, "failed to reconcile swarm on request");
        }

        let prior = if reassembled {
            SeenEpoch::default()
        } else {
            self.seen.get()
        };

        let mut blocks = Vec::new();
        let local_machine = self.swarm.local_machine();

        if let Some(board) = self.swarm.active_board_for(&self.session_id) {
            let target_epoch = board.board_epoch.max(1);
            let epoch_advanced = prior.epoch != target_epoch
                || prior.workdir.as_deref() != Some(board.workdir.as_str());
            if epoch_advanced {
                blocks.push(self.render_protocol_alert(&board, &local_machine));
                if commit {
                    // If this daemon mirrors a remote Board Host and `board_epoch` bumped
                    // (e.g. after Board Host failover or recreation), reset the local read
                    // cursor when the persisted told_epoch differs so new `#1..` entries
                    // are not masked by an earlier epoch's cursor.
                    if !board.is_local_host
                        && let Ok(persisted) = self
                            .swarm
                            .db()
                            .get_swarm_session_state(&self.session_id)
                            .await
                        && (persisted.told_epoch != target_epoch
                            || persisted.told_workdir.as_deref() != Some(board.workdir.as_str()))
                    {
                        let _ = self
                            .swarm
                            .db()
                            .set_swarm_cursor(&board.workdir, &self.session_id, 0)
                            .await;
                    }
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

            let unread_opt = if board.is_local_host {
                let reader_key = self.swarm.local_cursor_key(&self.session_id);
                self.swarm
                    .db()
                    .swarm_unread_status(&board.workdir, &reader_key)
                    .await
                    .ok()
                    .flatten()
                    .map(|st| (st.board_epoch, st.cursor, st.max_seq))
            } else {
                self.swarm
                    .db()
                    .get_swarm_cursor(&board.workdir, &self.session_id)
                    .await
                    .ok()
                    .map(|cursor| (board.board_epoch, cursor, board.max_seq))
            };

            if let Some((board_epoch, cursor, max_seq)) = unread_opt
                && max_seq > cursor
            {
                let unread_count = max_seq - cursor;
                let text = SwarmUnreadTemplate {
                    board_epoch,
                    unread_count,
                    first_seq: cursor + 1,
                    last_seq: max_seq,
                }
                .render()
                .unwrap_or_default()
                .trim_end()
                .to_string();
                blocks.push(ContextBlock::tail("system-reminder", text));
            }
        } else if !reassembled && prior.epoch != 0 {
            let raw_dir = prior.workdir.as_deref().unwrap_or(".");
            let display_dir = display_group_key(raw_dir);
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
