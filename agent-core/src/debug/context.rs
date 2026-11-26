//! Post-compaction live-resource recall and turn-end cleanup checks for active debug sessions.

use aither_agent::{ContextBlock, ContextSource, Moment, SourceId};
use aither_dap::{DebugSessionState, DebugStatus};
use askama::Template;

use crate::debug::DebugBroker;

#[derive(Template)]
#[template(path = "debug_live.txt", escape = "none")]
struct DebugLiveText<'a> {
    adapter: &'a str,
    target_pid: Option<u32>,
    target: Option<&'a str>,
    state: &'a str,
    stop_location: Option<String>,
}

#[derive(Template)]
#[template(path = "debug_unattended.txt", escape = "none")]
struct DebugUnattendedText<'a> {
    adapter: &'a str,
    target_pid: Option<u32>,
    state: &'a str,
}

/// Main-session [`ContextSource`] preserving active debug session awareness
/// across context compaction/reassembly and raising an at-most-once turn-end
/// cleanup check when a debug session remains open.
#[derive(Debug)]
pub struct DebugLiveContext {
    broker: DebugBroker,
    session_id: String,
    /// Instance generation already reminded on `Moment::FinalTurn`, so the
    /// objection fires at most once per active debug session.
    reminded_generation: Option<u64>,
}

impl DebugLiveContext {
    /// Creates a live-resource context source bound to `session_id` on `broker`.
    #[must_use]
    pub const fn new(broker: DebugBroker, session_id: String) -> Self {
        Self {
            broker,
            session_id,
            reminded_generation: None,
        }
    }

    fn render_inventory(status: &DebugStatus) -> String {
        let adapter = status.adapter.as_deref().unwrap_or("unknown");
        let stop_location = status.stop_location.as_ref().map(ToString::to_string);
        DebugLiveText {
            adapter,
            target_pid: status.target_pid,
            target: status.target.as_deref(),
            state: status.state.as_str(),
            stop_location,
        }
        .render()
        .expect("debug live inventory template must render")
    }

    fn render_unattended(status: &DebugStatus) -> String {
        let adapter = status.adapter.as_deref().unwrap_or("unknown");
        DebugUnattendedText {
            adapter,
            target_pid: status.target_pid,
            state: status.state.as_str(),
        }
        .render()
        .expect("debug unattended template must render")
    }
}

impl ContextSource for DebugLiveContext {
    fn id(&self) -> SourceId {
        SourceId::new("live_debug_session")
    }

    async fn blocks(&mut self, moment: Moment) -> Vec<ContextBlock> {
        match moment {
            Moment::Request { reassembled: true } => {
                let Some((_, status)) = self.broker.active_session_status(&self.session_id).await
                else {
                    return Vec::new();
                };
                if matches!(
                    status.state,
                    DebugSessionState::Inactive | DebugSessionState::Terminated
                ) {
                    return Vec::new();
                }
                vec![ContextBlock::stable(
                    "live_debug_session",
                    Self::render_inventory(&status),
                )]
            }
            Moment::FinalTurn => {
                let Some((generation, status)) =
                    self.broker.active_session_status(&self.session_id).await
                else {
                    self.reminded_generation = None;
                    return Vec::new();
                };
                if matches!(
                    status.state,
                    DebugSessionState::Inactive | DebugSessionState::Terminated
                ) {
                    self.reminded_generation = None;
                    return Vec::new();
                }
                if self.reminded_generation == Some(generation) {
                    return Vec::new();
                }
                self.reminded_generation = Some(generation);
                vec![ContextBlock::stable(
                    "system-reminder",
                    Self::render_unattended(&status),
                )]
            }
            _ => Vec::new(),
        }
    }

    async fn peek(&self, moment: Moment) -> Vec<ContextBlock> {
        let Moment::Request { reassembled: true } = moment else {
            return Vec::new();
        };
        let Some((_, status)) = self.broker.active_session_status(&self.session_id).await else {
            return Vec::new();
        };
        if matches!(
            status.state,
            DebugSessionState::Inactive | DebugSessionState::Terminated
        ) {
            return Vec::new();
        }
        vec![ContextBlock::stable(
            "live_debug_session",
            Self::render_inventory(&status),
        )]
    }
}
