use std::borrow::Cow;

use aither_agent::tool_request::{
    ToolRequestBroker, ToolRequestQueue, channel as tool_request_channel,
};
use aither_core::llm::{Tool, ToolResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::xml::XmlBlock;

/// Send a push notification to subscribed clients.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct NotifyArgs {
    /// Notification title.
    pub title: String,
    /// Notification body text.
    pub body: String,
    /// Optional target session id for callback routing.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Optional hidden XML context injected on notification click.
    #[serde(default)]
    pub callback_context_xml: Option<XmlBlock>,
    /// Optional user-visible message sent on notification click.
    #[serde(default)]
    pub callback_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyResult {
    pub sent: usize,
}

pub type NotifyBroker = ToolRequestBroker<NotifyArgs, Result<NotifyResult, String>>;
pub type NotifyQueue = ToolRequestQueue<NotifyArgs, Result<NotifyResult, String>>;

#[derive(Debug, thiserror::Error)]
enum NotifyToolError {
    #[error("notify title must not be empty")]
    EmptyTitle,
    #[error("notify body must not be empty")]
    EmptyBody,
    #[error("notify callback requires both callback_context_xml and callback_message")]
    IncompleteCallback,
    #[error("notify command is unavailable in this runtime")]
    Unavailable,
    #[error("notify request was rejected: {0}")]
    Rejected(String),
}

#[must_use]
pub fn channel() -> (NotifyBroker, NotifyQueue) {
    tool_request_channel()
}

#[derive(Clone)]
pub struct NotifyTool {
    broker: Option<NotifyBroker>,
}

impl NotifyTool {
    #[must_use]
    pub const fn new(broker: Option<NotifyBroker>) -> Self {
        Self { broker }
    }

    fn normalize_args(args: NotifyArgs) -> aither_core::Result<NotifyArgs> {
        if args.title.trim().is_empty() {
            return Err(NotifyToolError::EmptyTitle.into());
        }
        if args.body.trim().is_empty() {
            return Err(NotifyToolError::EmptyBody.into());
        }

        let callback_context_xml = args.callback_context_xml;
        let callback_message = args
            .callback_message
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);

        let has_context = callback_context_xml.is_some();
        let has_message = callback_message.is_some();
        if has_context ^ has_message {
            return Err(NotifyToolError::IncompleteCallback.into());
        }

        Ok(NotifyArgs {
            title: args.title.trim().to_string(),
            body: args.body.trim().to_string(),
            session_id: args
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
            callback_context_xml,
            callback_message,
        })
    }
}

impl Tool for NotifyTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("notify")
    }

    type Arguments = NotifyArgs;
    type Res = ToolResult;

    async fn call(&self, args: Self::Arguments) -> aither_core::Result<Self::Res> {
        let broker = self.broker.as_ref().ok_or(NotifyToolError::Unavailable)?;
        let args = Self::normalize_args(args)?;
        let response = broker.request(args).await?;
        let result = response.map_err(NotifyToolError::Rejected)?;

        ToolResult::json(&serde_json::json!({
            "ok": true,
            "sent": result.sent
        }))
    }
}
