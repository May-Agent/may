//! Verbatim request transcript for `/context dump`.
//!
//! Renders the assembled message list of a [`ContextWindowSnapshot`] —
//! the exact sequence the provider would receive on the next request —
//! as a readable transcript the TUI writes to the session's `outputs/`
//! directory. No decomposition or metrics: the file is the request.

use aither_agent::ContextWindowSnapshot;
use aither_core::llm::{Message as LlmMessage, Role};
use askama::Template;

const fn role_label(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}

/// An assistant tool call, arguments verbatim.
#[derive(Debug)]
pub struct ContextToolCallDump {
    /// Called tool.
    pub name: String,
    /// Serialized call arguments.
    pub arguments: String,
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

/// One wire message.
#[derive(Debug)]
pub struct ContextMessageDump {
    /// Position in the assembled request.
    pub index: usize,
    /// `user` / `assistant` / `system` / `tool`.
    pub role: &'static str,
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
}

/// The verbatim request transcript of the assembled context window.
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
    /// Full assembled message list in wire order.
    pub messages: Vec<ContextMessageDump>,
}

impl ContextDump {
    /// Build a dump from a snapshot of the live context.
    pub(crate) fn assemble(
        model_id: &str,
        provider_id: &str,
        session_id: &str,
        snapshot: &ContextWindowSnapshot,
    ) -> Self {
        Self {
            session_id: session_id.to_string(),
            model_id: model_id.to_string(),
            provider_id: provider_id.to_string(),
            captured_at: jiff::Timestamp::now().to_string(),
            messages: snapshot
                .messages
                .iter()
                .enumerate()
                .map(|(index, message)| message_dump(index, message))
                .collect(),
        }
    }

    /// Render the dump to a readable transcript.
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

fn message_dump(index: usize, message: &LlmMessage) -> ContextMessageDump {
    ContextMessageDump {
        index,
        role: role_label(message.role()),
        content: message.content().to_string(),
        attachments: message
            .attachments()
            .iter()
            .map(|attachment| attachment.url().to_string())
            .collect(),
        tool_call_id: message.tool_call_id().map(str::to_string),
        tool_calls: message
            .tool_calls()
            .iter()
            .map(|call| ContextToolCallDump {
                name: call.name.clone(),
                arguments: call.arguments.to_string(),
            })
            .collect(),
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
    use aither_agent::{ContextWindowMetrics, ContextWindowPhase};

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

    #[test]
    fn dump_renders_the_request_in_wire_order() {
        let mut context = aither_agent::Context::default();
        context.insert_system_text("memory", "remember this");
        context.insert_system_text("persona", "be brief");
        context.push(LlmMessage::user("hello"));
        context.push(LlmMessage::assistant("hi there"));
        let snapshot = ContextWindowSnapshot {
            phase: ContextWindowPhase::Stable,
            metrics: test_metrics(),
            messages: context.build_request(&[]),
        };

        let dump = ContextDump::assemble("gpt-5", "openai", "s", &snapshot);
        let text = dump.render();

        assert!(text.contains("[0] role: system"));
        assert!(text.contains("remember this"));
        assert!(text.contains("be brief"));
        assert!(text.contains("[1] role: user\nhello"));
        assert!(text.contains("[2] role: assistant\nhi there"));
        assert_eq!(dump.messages.len(), 3);
    }
}
