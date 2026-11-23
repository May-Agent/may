//! The `send_message` command: a running subagent's mid-task follow-up to
//! the main agent that launched it.
//!
//! The message takes the path a parked run's report takes: it is stored in
//! the session's pending-message queue as hidden context, so a main agent
//! mid-turn receives it at its next turn boundary and an idle session is
//! woken by it — frequent-idle alerts included. The run itself keeps going;
//! the final report remains the last turn response once the task is done
//! and no background job runs. The main agent itself has no parent to
//! report to: its call is refused.

use std::borrow::Cow;

use aither_core::llm::{AgentIdentity, Tool, ToolContext, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::agent_link::{AttributeText, UntrustedText};
use crate::task_wake::{SessionPrompts, TaskWakeError};

/// The arguments of `send_message`: the message the parent reads.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendMessageArgs {
    /// The follow-up the main agent reads at its next turn boundary.
    pub message: String,
}

/// Why `send_message` could not deliver.
#[derive(Debug, thiserror::Error)]
pub enum SendMessageError {
    /// The call did not come from a subagent.
    #[error(
        "`send_message` is how a running subagent reports to the agent that launched it; the main agent has no parent to message"
    )]
    NotSubagent,
    /// The runtime has no session queue to deliver through.
    #[error("`send_message` is unavailable in this runtime")]
    Unavailable,
    /// The message was empty.
    #[error("`send_message` takes a non-empty message")]
    Empty,
    /// Storing or dispatching the message failed.
    #[error(transparent)]
    Wake(#[from] TaskWakeError),
}

/// The `send_message` command, bound to the session its calls report to.
#[derive(Debug, Clone)]
pub struct SendMessageTool {
    prompts: Option<SessionPrompts>,
    session_id: String,
}

/// The hidden-context wrapper a message arrives in.
#[derive(Template)]
#[template(path = "subagent/message.txt", escape = "none")]
struct SubagentMessage<'a> {
    subagent: AttributeText<'a>,
    body: UntrustedText<'a>,
}

/// What a delivered message tells the sender.
#[derive(Template)]
#[template(path = "subagent/message_sent.txt", escape = "none")]
struct MessageSent;

impl SendMessageTool {
    /// The command for the session `session_id`, delivering through
    /// `prompts`.
    #[must_use]
    pub const fn new(prompts: Option<SessionPrompts>, session_id: String) -> Self {
        Self {
            prompts,
            session_id,
        }
    }
}

impl Tool for SendMessageTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("send_message")
    }

    type Arguments = SendMessageArgs;
    type Res = ToolResult;
    type Error = SendMessageError;

    async fn call(&self, args: Self::Arguments, cx: ToolContext) -> Result<Self::Res, Self::Error> {
        let Some(identity) = cx.identity() else {
            return Err(SendMessageError::NotSubagent);
        };
        let AgentIdentity::Subagent(subagent) = identity.agent() else {
            return Err(SendMessageError::NotSubagent);
        };
        let message = args.message.trim();
        if message.is_empty() {
            return Err(SendMessageError::Empty);
        }
        let prompts = self.prompts.as_ref().ok_or(SendMessageError::Unavailable)?;
        let text = SubagentMessage {
            subagent: AttributeText(subagent.name()),
            body: UntrustedText(message),
        }
        .render()
        .expect("the subagent message template renders typed input");
        prompts.deliver_hidden(&self.session_id, text).await?;
        Ok(ToolResult::text(
            MessageSent
                .render()
                .expect("the message-sent template renders typed input"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use aither_core::llm::{CallIdentity, SubagentIdentity, ToolCallId};

    use super::*;
    use crate::db::ChatDb;
    use crate::session::{MayHomePaths, SessionRuntimeService};
    use crate::ui_event_bus::UiEventBus;

    fn call_of(agent: AgentIdentity) -> CallIdentity {
        CallIdentity::new(ToolCallId::new("call-1"), agent)
    }

    fn subagent_cx() -> ToolContext {
        ToolContext::new().with_identity(call_of(AgentIdentity::Subagent(SubagentIdentity::new(
            "worker",
            Some(call_of(AgentIdentity::Main)),
        ))))
    }

    async fn session(
        temp: &tempfile::TempDir,
    ) -> (
        std::sync::Arc<SessionRuntimeService>,
        SessionPrompts,
        async_channel::Receiver<String>,
    ) {
        let db = ChatDb::open(&temp.path().join("history.db"))
            .await
            .expect("db opens");
        db.create_session("s1", Some("Main"), None)
            .await
            .expect("session is created");
        let runtime = std::sync::Arc::new(SessionRuntimeService::new(
            db,
            UiEventBus::new(),
            MayHomePaths::new(temp.path().join(".may")),
        ));
        let (wake_tx, wake_rx) = async_channel::unbounded();
        (
            runtime.clone(),
            SessionPrompts::new(runtime, wake_tx),
            wake_rx,
        )
    }

    /// A call the main agent makes is refused: it has no parent to report to.
    #[tokio::test]
    async fn the_main_agent_cannot_send_itself_a_message() {
        let tool = SendMessageTool::new(None, "s1".to_string());
        let cx = ToolContext::new().with_identity(call_of(AgentIdentity::Main));
        let error = tool
            .call(
                SendMessageArgs {
                    message: "hello".to_string(),
                },
                cx,
            )
            .await
            .expect_err("the main agent is refused");
        assert!(matches!(error, SendMessageError::NotSubagent), "{error}");
    }

    /// A subagent's message is queued as hidden context — a busy parent reads
    /// it at its next turn boundary — and the session is woken for an idle
    /// one.
    #[tokio::test]
    async fn a_subagents_message_is_queued_and_wakes_the_session() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (runtime, prompts, wake_rx) = session(&temp).await;
        let tool = SendMessageTool::new(Some(prompts), "s1".to_string());

        let result = tool
            .call(
                SendMessageArgs {
                    message: "found the bug in parser.rs".to_string(),
                },
                subagent_cx(),
            )
            .await
            .expect("the message delivers");
        let answer = result.as_text().unwrap_or_default();
        assert!(answer.contains("Delivered"), "{answer}");
        assert_eq!(
            wake_rx.try_recv().as_deref(),
            Ok("s1"),
            "the session is woken"
        );
        let summary = runtime.get("s1").await.expect("the summary reads");
        assert!(
            summary.has_pending_messages(),
            "the message waits in the session's queue"
        );
    }

    /// A runtime without a session queue answers `unavailable`, and an empty
    /// message is refused.
    #[tokio::test]
    async fn a_runtime_without_a_queue_and_an_empty_message_are_refused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (_runtime, prompts, _wake) = session(&temp).await;
        let tool = SendMessageTool::new(Some(prompts), "s1".to_string());
        let error = tool
            .call(
                SendMessageArgs {
                    message: " ".to_string(),
                },
                subagent_cx(),
            )
            .await
            .expect_err("an empty message is refused");
        assert!(matches!(error, SendMessageError::Empty), "{error}");

        let tool = SendMessageTool::new(None, "s1".to_string());
        let error = tool
            .call(
                SendMessageArgs {
                    message: "still running".to_string(),
                },
                subagent_cx(),
            )
            .await
            .expect_err("no queue is refused");
        assert!(matches!(error, SendMessageError::Unavailable), "{error}");
    }
}
