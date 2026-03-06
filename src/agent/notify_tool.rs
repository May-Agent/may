use std::borrow::Cow;

use aither_agent::tool_request::{
    ToolRequestBroker, ToolRequestQueue, channel as tool_request_channel,
};
use aither_core::llm::{Tool, ToolOutput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    pub callback_context_xml: Option<String>,
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
            return Err(anyhow::anyhow!("notify title must not be empty"));
        }
        if args.body.trim().is_empty() {
            return Err(anyhow::anyhow!("notify body must not be empty"));
        }

        let callback_context_xml = args
            .callback_context_xml
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let callback_message = args
            .callback_message
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);

        let has_context = callback_context_xml.is_some();
        let has_message = callback_message.is_some();
        if has_context ^ has_message {
            return Err(anyhow::anyhow!(
                "notify callback requires both callback_context_xml and callback_message"
            ));
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

    async fn call(&self, args: Self::Arguments) -> aither_core::Result<ToolOutput> {
        let broker = self
            .broker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("notify command is unavailable in this runtime"))?;
        let args = Self::normalize_args(args)?;
        let response = broker.request(args).await?;
        let result = response.map_err(anyhow::Error::msg)?;

        ToolOutput::json(&serde_json::json!({
            "ok": true,
            "sent": result.sent
        }))
    }
}
