//! Verbatim context composition for `/context dump`.
//!
//! The [`ContextReport`] panel answers "how much"; this module answers
//! "what exactly". A dump is built from the live [`aither_agent::Context`]
//! — every named stable-prefix block in wire order, including staged
//! updates waiting for reassembly — plus the full assembled message list
//! a [`ContextWindowSnapshot`] measured on the same state. Rendering is a
//! markdown file the TUI writes to the session's `outputs/` directory, so
//! developers can read, grep, and diff context across prompt changes.
//!
//! [`ContextReport`]: super::context_report::ContextReport

use aither_agent::{ContextWindowMetrics, ContextWindowSnapshot};
use aither_core::llm::{Message as LlmMessage, Role};
use askama::Template;

/// Estimated tokens per rendered character — the heuristic the agent
/// itself uses for context budgeting.
const CHARS_PER_TOKEN: usize = 4;

const fn est_tokens(chars: usize) -> usize {
    chars / CHARS_PER_TOKEN
}

const fn role_label(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}

/// Which span of the assembled request a wire message belongs to.
#[derive(Debug, Clone, Copy)]
pub enum MessageSection {
    /// The stable system prefix — all named blocks joined into one message.
    Prefix,
    /// The active handoff document.
    Handoff,
    /// Recent conversation, durable system alerts included.
    Conversation,
    /// Live context rendered fresh for the next request.
    Tail,
}

impl MessageSection {
    /// Row label shown in the dump.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Prefix => "system prefix",
            Self::Handoff => "handoff",
            Self::Conversation => "conversation",
            Self::Tail => "live tail",
        }
    }
}

/// One named stable-prefix block, verbatim.
#[derive(Debug)]
pub struct ContextBlockDump {
    /// Block tag — iteration order of `system_blocks` is wire order.
    pub tag: String,
    /// Block content as it rides the wire.
    pub content: String,
    /// Staged replacement waiting for the next reassembly, if any.
    pub staged_update: Option<String>,
    /// `content` size.
    pub chars: usize,
    /// Estimated tokens.
    pub tokens: usize,
}

/// An assistant tool call, arguments verbatim.
#[derive(Debug)]
pub struct ContextToolCallDump {
    /// Called tool.
    pub name: String,
    /// Serialized call arguments.
    pub arguments: String,
    /// `name` + `arguments` size.
    pub chars: usize,
}

/// One provider-tagged reasoning block; the payload is opaque so the dump
/// records its size only.
#[derive(Debug)]
pub struct ContextReasoningDump {
    /// Provider that produced the reasoning.
    pub provider: String,
    /// Opaque payload size.
    pub payload_chars: usize,
}

/// One wire message with provenance metadata.
#[derive(Debug)]
pub struct ContextMessageDump {
    /// Position in the assembled request.
    pub index: usize,
    /// `user` / `assistant` / `system` / `tool`.
    pub role: &'static str,
    /// Which request span this message forms.
    pub section: MessageSection,
    /// Message content as it rides the wire.
    pub content: String,
    /// Attachment urls.
    pub attachments: Vec<String>,
    /// Tool result linkage on `tool` messages.
    pub tool_call_id: Option<String>,
    /// Assistant tool calls on this message.
    pub tool_calls: Vec<ContextToolCallDump>,
    /// Reasoning blocks recorded on this message.
    pub reasoning: Vec<ContextReasoningDump>,
    /// Rendered size: content plus tool-call payloads.
    pub chars: usize,
    /// Estimated tokens.
    pub tokens: usize,
}

/// A verbatim, developer-facing view of the assembled context window.
#[derive(Debug, Template)]
#[template(path = "context_dump.md", escape = "none")]
pub struct ContextDump {
    /// Session the dump was captured from.
    pub session_id: String,
    /// Selected model.
    pub model_id: String,
    /// Selected provider.
    pub provider_id: String,
    /// RFC 3339 capture time.
    pub captured_at: String,
    /// `ContextWindowPhase` rendered for display.
    pub phase_label: String,
    /// Usage fraction rendered for display.
    pub usage_pct: String,
    /// Window metrics at capture time.
    pub metrics: ContextWindowMetrics,
    /// Named stable-prefix blocks in wire order.
    pub blocks: Vec<ContextBlockDump>,
    /// Full assembled message list in wire order.
    pub messages: Vec<ContextMessageDump>,
    /// Joined size of the stable prefix.
    pub prefix_chars: usize,
    /// Estimated prefix tokens.
    pub prefix_tokens: usize,
}

impl ContextDump {
    /// Build a dump from the live context and a snapshot measured on the
    /// same state.
    pub(crate) fn assemble(
        model_id: &str,
        provider_id: &str,
        session_id: &str,
        context: &aither_agent::Context,
        snapshot: &ContextWindowSnapshot,
    ) -> Self {
        let blocks = context
            .system_blocks()
            .iter()
            .map(|(tag, content)| ContextBlockDump {
                tag: tag.clone(),
                chars: content.len(),
                tokens: est_tokens(content.len()),
                staged_update: context.staged_system_update(tag).map(str::to_string),
                content: content.clone(),
            })
            .collect::<Vec<_>>();

        // `build_request` lays messages out as: joined prefix, optional
        // handoff, conversation items, optional live tail.
        let prefix_messages = usize::from(!blocks.is_empty());
        let handoff_messages = usize::from(context.handoff().is_some());
        let conversation_start = prefix_messages + handoff_messages;
        let conversation_end = conversation_start + context.recent().len();

        let messages = snapshot
            .messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                let section = if index < prefix_messages {
                    MessageSection::Prefix
                } else if index < conversation_start {
                    MessageSection::Handoff
                } else if index < conversation_end {
                    MessageSection::Conversation
                } else {
                    MessageSection::Tail
                };
                message_dump(index, message, section)
            })
            .collect();

        let prefix_chars = blocks.iter().map(|block| block.chars).sum();

        Self {
            session_id: session_id.to_string(),
            model_id: model_id.to_string(),
            provider_id: provider_id.to_string(),
            captured_at: jiff::Timestamp::now().to_string(),
            phase_label: format!("{:?}", snapshot.phase),
            usage_pct: format!("{:.0}%", f64::from(snapshot.metrics.usage_fraction) * 100.0),
            metrics: snapshot.metrics.clone(),
            prefix_tokens: est_tokens(prefix_chars),
            prefix_chars,
            blocks,
            messages,
        }
    }

    /// Render the dump to markdown.
    ///
    /// # Panics
    ///
    /// Panics when the bundled template fails to render.
    #[must_use]
    pub fn render(&self) -> String {
        Template::render(self)
            .unwrap_or_else(|error| panic!("failed to render context dump: {error}"))
    }
}

fn message_dump(index: usize, message: &LlmMessage, section: MessageSection) -> ContextMessageDump {
    let tool_calls = message
        .tool_calls()
        .iter()
        .map(|call| {
            let arguments = call.arguments.to_string();
            ContextToolCallDump {
                chars: call.name.len() + arguments.len(),
                name: call.name.clone(),
                arguments,
            }
        })
        .collect::<Vec<_>>();
    let chars = message.content().len() + tool_calls.iter().map(|call| call.chars).sum::<usize>();
    ContextMessageDump {
        index,
        role: role_label(message.role()),
        section,
        chars,
        tokens: est_tokens(chars),
        content: message.content().to_string(),
        attachments: message
            .attachments()
            .iter()
            .map(|attachment| attachment.url().to_string())
            .collect(),
        tool_call_id: message.tool_call_id().map(str::to_string),
        tool_calls,
        reasoning: message
            .reasoning()
            .iter()
            .map(|reasoning| ContextReasoningDump {
                provider: reasoning.provider().to_string(),
                payload_chars: reasoning.payload().len(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aither_agent::{Block, ContextWindowPhase};

    fn test_context() -> aither_agent::Context {
        let mut context = aither_agent::Context::default();
        context.insert_system_text("memory", "remember this");
        context.insert_system_text("persona", "be brief");
        context.push(LlmMessage::user("hello"));
        context.push(LlmMessage::assistant("hi there"));
        context
    }

    fn test_metrics() -> ContextWindowMetrics {
        ContextWindowMetrics {
            usage_fraction: 0.0,
            context_window: 200_000,
            estimated_tokens: 0,
            chars_per_token: 4.0,
            system_block_count: 0,
            recent_message_count: 0,
            recent_system_message_count: 0,
            has_handoff: false,
            has_staged_system_updates: false,
            prompt_cache_epoch: 0,
        }
    }

    fn snapshot_for(context: &aither_agent::Context, tail: &[Block]) -> ContextWindowSnapshot {
        ContextWindowSnapshot {
            phase: ContextWindowPhase::Stable,
            metrics: test_metrics(),
            messages: context.build_request(tail),
        }
    }

    #[test]
    fn dump_decomposes_prefix_and_labels_message_sections() {
        let context = test_context();
        let tail = [Block::new("tasks", "tasks: one open")];
        let snapshot = snapshot_for(&context, &tail);

        let dump = ContextDump::assemble("gpt-5", "openai", "session-id", &context, &snapshot);

        assert_eq!(
            dump.blocks
                .iter()
                .map(|block| block.tag.as_str())
                .collect::<Vec<_>>(),
            ["memory", "persona"]
        );
        assert_eq!(dump.blocks[0].content, "remember this");
        assert!(dump.blocks[0].staged_update.is_none());

        assert_eq!(dump.messages.len(), 4);
        assert!(matches!(dump.messages[0].section, MessageSection::Prefix));
        assert_eq!(dump.messages[0].role, "system");
        assert_eq!(dump.messages[0].content, "remember this\nbe brief");
        assert!(matches!(
            dump.messages[1].section,
            MessageSection::Conversation
        ));
        assert_eq!(dump.messages[1].role, "user");
        assert!(matches!(
            dump.messages[2].section,
            MessageSection::Conversation
        ));
        assert!(matches!(dump.messages[3].section, MessageSection::Tail));
        assert_eq!(dump.messages[3].content, "tasks: one open");
    }

    #[test]
    fn dump_marks_handoff_between_prefix_and_conversation() {
        let mut context = test_context();
        context.set_handoff("handoff doc");
        let snapshot = snapshot_for(&context, &[]);

        let dump = ContextDump::assemble("gpt-5", "openai", "s", &context, &snapshot);

        assert_eq!(dump.messages.len(), 4);
        assert!(matches!(dump.messages[0].section, MessageSection::Prefix));
        assert!(matches!(dump.messages[1].section, MessageSection::Handoff));
        assert!(matches!(
            dump.messages[2].section,
            MessageSection::Conversation
        ));
        assert!(matches!(
            dump.messages[3].section,
            MessageSection::Conversation
        ));
    }

    #[test]
    fn dump_renders_without_panicking() {
        let context = test_context();
        let snapshot = snapshot_for(&context, &[]);
        let dump = ContextDump::assemble("gpt-5", "openai", "s", &context, &snapshot);
        let text = dump.render();
        assert!(text.contains("`memory`"));
        assert!(text.contains("remember this"));
        assert!(text.contains("hi there"));
    }
}
