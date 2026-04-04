use std::borrow::Cow;

use aither_agent::tool_request::{
    ToolRequestBroker, ToolRequestQueue, channel as tool_request_channel,
};
use aither_core::llm::{Tool, ToolResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::browser_service::BrowserPageSnapshot;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum BrowserArgs {
    /// Show managed browser runtime status.
    Status,
    /// Start the managed browser runtime.
    Start,
    /// Stop the managed browser runtime.
    Stop,
    /// Return the managed browser CDP endpoint.
    CdpEndpoint,
    /// Open a URL in the May-managed browser and return a readable page snapshot.
    Read { url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserRequest {
    pub session_id: String,
    pub args: BrowserArgs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserResult {
    pub running: bool,
    pub cdp_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<BrowserPageSnapshot>,
}

pub type BrowserBroker = ToolRequestBroker<BrowserRequest, Result<BrowserResult, String>>;
pub type BrowserQueue = ToolRequestQueue<BrowserRequest, Result<BrowserResult, String>>;

#[must_use]
pub fn channel() -> (BrowserBroker, BrowserQueue) {
    tool_request_channel()
}

#[derive(Debug, Clone)]
pub struct BrowserTool {
    session_id: String,
    broker: Option<BrowserBroker>,
}

impl BrowserTool {
    #[must_use]
    pub const fn new(session_id: String, broker: Option<BrowserBroker>) -> Self {
        Self { session_id, broker }
    }
}

impl Tool for BrowserTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("browser")
    }

    type Arguments = BrowserArgs;
    type Res = ToolResult;

    async fn call(&self, args: Self::Arguments) -> aither_core::Result<Self::Res> {
        let broker = self
            .broker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("browser command is unavailable in this runtime"))?;
        let response = broker
            .request(BrowserRequest {
                session_id: self.session_id.clone(),
                args,
            })
            .await?;
        let result = response.map_err(anyhow::Error::msg)?;
        ToolResult::json(&serde_json::json!({
            "ok": true,
            "running": result.running,
            "cdp_endpoint": result.cdp_endpoint,
            "page": result.page,
        }))
    }
}
