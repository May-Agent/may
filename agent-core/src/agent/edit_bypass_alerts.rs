//! One-shot system alerts when a terminal script bypasses `edit` to modify an
//! existing file while hashline mode is enabled.

use aither_agent::{AgentPath, ContextBlock, ContextSource, Moment, SourceId};
use askama::Template;

use crate::command_policy::{EditBypassNotices, EditBypassUsage};

const TAG: &str = "hashline_edit_bypass_alert";

#[derive(Template)]
#[template(path = "alert_hashline_edit_bypass.txt", escape = "none")]
struct EditBypassAlertText<'a> {
    items: &'a [EditBypassUsage],
}

fn alert_blocks(items: &[EditBypassUsage]) -> Vec<ContextBlock> {
    if items.is_empty() {
        return Vec::new();
    }
    vec![ContextBlock::stable(
        TAG,
        EditBypassAlertText { items }
            .render()
            .expect("the hashline edit bypass alert template renders"),
    )]
}

/// Hands pending hashline `edit`-bypass notices to one agent once on its next
/// request, dropping them once delivered and never replaying across compaction.
#[derive(Debug, Clone)]
pub struct EditBypassAlerts {
    notices: EditBypassNotices,
    agent: AgentPath,
}

impl EditBypassAlerts {
    /// Creates an alert source draining main-agent notices from `notices`.
    #[must_use]
    pub const fn new(notices: EditBypassNotices) -> Self {
        Self::for_agent(notices, AgentPath::main())
    }

    /// Creates an alert source draining only `agent`'s notices from `notices`.
    #[must_use]
    pub const fn for_agent(notices: EditBypassNotices, agent: AgentPath) -> Self {
        Self { notices, agent }
    }
}

impl ContextSource for EditBypassAlerts {
    fn id(&self) -> SourceId {
        SourceId::new(TAG)
    }

    #[allow(clippy::unused_async_trait_impl, reason = "the trait method is async")]
    async fn blocks(&mut self, moment: Moment) -> Vec<ContextBlock> {
        match moment {
            Moment::Request { .. } => alert_blocks(&self.notices.take_all_for(&self.agent)),
            _ => Vec::new(),
        }
    }

    #[allow(clippy::unused_async_trait_impl, reason = "the trait method is async")]
    async fn peek(&self, moment: Moment) -> Vec<ContextBlock> {
        match moment {
            Moment::Request { .. } => alert_blocks(&self.notices.peek_all_for(&self.agent)),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aither_core::llm::tool::{AgentIdentity, CallIdentity, SubagentIdentity, ToolCallId};

    #[tokio::test]
    async fn emits_once_per_violation_drops_on_compaction_and_re_emits_on_repeat() {
        let notices = EditBypassNotices::default();
        let mut source = EditBypassAlerts::new(notices.clone());
        let moment = Moment::Request { reassembled: false };

        assert_eq!(
            source.blocks(moment).await,
            [] as [aither_agent::ContextBlock; 0]
        );

        notices.record(EditBypassUsage::new("sed -i", Some("src/lib.rs")));

        // Peek observes pending alerts without draining them.
        let peeked = source.peek(moment).await;
        assert_eq!(peeked.len(), 1);
        assert!(peeked[0].block().content().contains("`sed -i`"));
        assert!(peeked[0].block().content().contains("`src/lib.rs`"));

        // Non-request moments do not emit or drain the alert.
        assert_eq!(
            source.blocks(Moment::ToolTurnEnd).await,
            [] as [aither_agent::ContextBlock; 0]
        );
        assert_eq!(
            source.blocks(Moment::FinalTurn).await,
            [] as [aither_agent::ContextBlock; 0]
        );

        // First request delivers the one-shot alert and drains the queue.
        let first = source.blocks(moment).await;
        assert_eq!(first.len(), 1);
        let content = first[0].block().content();
        assert!(content.contains("`sed -i`"), "{content}");
        assert!(content.contains("`src/lib.rs`"), "{content}");
        assert!(content.contains("`edit`"), "{content}");
        assert!(content.contains("[path#TAG]"), "{content}");
        assert!(content.contains("`search`"), "{content}");

        // Drained after delivery.
        assert_eq!(
            source.blocks(moment).await,
            [] as [aither_agent::ContextBlock; 0]
        );

        // Compaction / reassembly never replays a consumed alert.
        assert_eq!(
            source.blocks(Moment::Request { reassembled: true }).await,
            [] as [aither_agent::ContextBlock; 0]
        );

        // A new violation emits a fresh alert.
        notices.record(EditBypassUsage::new("apply_patch", None));
        let second = source.blocks(moment).await;
        assert_eq!(second.len(), 1);
        let second_content = second[0].block().content();
        assert!(second_content.contains("`apply_patch`"), "{second_content}");
        assert_eq!(
            source.blocks(moment).await,
            [] as [aither_agent::ContextBlock; 0]
        );
    }

    #[tokio::test]
    async fn isolates_main_agent_and_subagent_alerts() {
        let notices = EditBypassNotices::default();
        let main_call = CallIdentity::new(ToolCallId::new("c1"), AgentIdentity::Main);
        let sub_identity =
            AgentIdentity::Subagent(SubagentIdentity::new("general", Some(main_call)));
        let sub_path = AgentPath::of(&sub_identity);

        let mut main_source = EditBypassAlerts::new(notices.clone());
        let mut sub_source = EditBypassAlerts::for_agent(notices.clone(), sub_path.clone());
        let moment = Moment::Request { reassembled: false };

        notices.record_for(&sub_path, EditBypassUsage::new("python3 -c", Some("a.py")));
        assert_eq!(
            main_source.blocks(moment).await,
            [] as [aither_agent::ContextBlock; 0]
        );

        let sub_blocks = sub_source.blocks(moment).await;
        assert_eq!(sub_blocks.len(), 1);
        assert!(sub_blocks[0].block().content().contains("`python3 -c`"));
        assert_eq!(
            sub_source.blocks(moment).await,
            [] as [aither_agent::ContextBlock; 0]
        );

        notices.record(EditBypassUsage::new("tee", Some("b.py")));
        assert_eq!(
            sub_source.blocks(moment).await,
            [] as [aither_agent::ContextBlock; 0]
        );

        let main_blocks = main_source.blocks(moment).await;
        assert_eq!(main_blocks.len(), 1);
        assert!(main_blocks[0].block().content().contains("`tee`"));
    }
}
