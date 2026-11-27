//! One-shot system alerts when a managed-bash script invokes host `grep`,
//! `awk`, or `find` instead of the preferred managed tools.

use aither_agent::{ContextBlock, ContextSource, Moment, SourceId};
use askama::Template;

use crate::command_policy::{HostCommandNotices, HostCommandUsage};

const TAG: &str = "host_command_alert";

#[derive(Template)]
#[template(path = "alert_host_command.txt", escape = "none")]
struct HostCommandAlertText<'a> {
    items: &'a [HostCommandUsage],
}

fn alert_blocks(items: &[HostCommandUsage]) -> Vec<ContextBlock> {
    if items.is_empty() {
        return Vec::new();
    }
    vec![ContextBlock::stable(
        TAG,
        HostCommandAlertText { items }
            .render()
            .expect("the host command alert template renders"),
    )]
}

/// Hands pending `grep`/`awk`/`find` usage notices to the model once on the
/// next request, dropping them once delivered.
#[derive(Debug, Clone)]
pub struct HostCommandAlerts(HostCommandNotices);

impl HostCommandAlerts {
    /// Creates an alert source draining `notices`.
    #[must_use]
    pub const fn new(notices: HostCommandNotices) -> Self {
        Self(notices)
    }
}

impl ContextSource for HostCommandAlerts {
    fn id(&self) -> SourceId {
        SourceId::new(TAG)
    }

    #[allow(clippy::unused_async_trait_impl, reason = "the trait method is async")]
    async fn blocks(&mut self, moment: Moment) -> Vec<ContextBlock> {
        match moment {
            Moment::Request { .. } => alert_blocks(&self.0.take_all()),
            _ => Vec::new(),
        }
    }

    #[allow(clippy::unused_async_trait_impl, reason = "the trait method is async")]
    async fn peek(&self, moment: Moment) -> Vec<ContextBlock> {
        match moment {
            Moment::Request { .. } => alert_blocks(&self.0.peek_all()),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emits_once_per_violation_and_re_emits_on_repeat() {
        let notices = HostCommandNotices::default();
        let mut source = HostCommandAlerts::new(notices.clone());
        let moment = Moment::Request { reassembled: false };

        let blocks = source.blocks(moment).await;
        assert!(blocks.is_empty(), "{blocks:?}");

        notices.record(HostCommandUsage {
            name: "grep",
            replacement: "`rg`",
        });
        let first = source.blocks(moment).await;
        assert_eq!(first.len(), 1);
        assert!(
            first[0].block().content().contains("`grep`"),
            "{}",
            first[0].block().content()
        );
        assert!(
            first[0].block().content().contains("`rg`"),
            "{}",
            first[0].block().content()
        );

        // One-shot: drained after delivery.
        let blocks = source.blocks(moment).await;
        assert!(blocks.is_empty(), "{blocks:?}");

        // New violation produces a new alert.
        notices.record(HostCommandUsage {
            name: "find",
            replacement: "`fd`",
        });
        let second = source.blocks(moment).await;
        assert_eq!(second.len(), 1);
        assert!(
            second[0].block().content().contains("`find`"),
            "{}",
            second[0].block().content()
        );
    }
}
