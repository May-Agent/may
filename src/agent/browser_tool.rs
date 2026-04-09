use std::borrow::Cow;

use aither_agent::tool_request::{
    ToolRequestBroker, ToolRequestQueue, channel as tool_request_channel,
};
use aither_core::llm::{IntoToolResult, Tool, ToolResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::browser_service::{
    BrowserPageSnapshot, BrowserReadDetail, BrowserReadFormat, BrowserReadOptions,
    BrowserScreenshotMode,
};

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum BrowserArgs {
    /// Show managed browser runtime status.
    Status,
    /// Start the managed browser runtime.
    #[serde(rename = "open")]
    Start,
    /// Stop the managed browser runtime.
    #[serde(rename = "close")]
    Stop,
    /// Return the managed browser CDP endpoint. Requires `open` first.
    CdpEndpoint,
    /// Open a URL in the May-managed browser and inspect it with structured text,
    /// accessibility output, and an optional screenshot artifact. Requires
    /// `open` first; call `close` when browser work is finished.
    Read {
        url: String,
        #[serde(default)]
        detail: BrowserReadDetail,
        #[serde(default)]
        screenshot: BrowserScreenshotMode,
        #[serde(default)]
        format: BrowserReadFormat,
    },
}

impl BrowserArgs {
    #[must_use]
    pub fn read_options(
        detail: BrowserReadDetail,
        screenshot: BrowserScreenshotMode,
    ) -> BrowserReadOptions {
        BrowserReadOptions { detail, screenshot }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum BrowserToolError {
    #[error("browser command is unavailable in this runtime")]
    UnavailableInRuntime,
    #[error("{message}")]
    RequestFailed { message: String },
}

impl BrowserToolError {
    #[must_use]
    pub fn request_failed(message: impl Into<String>) -> Self {
        Self::RequestFailed {
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BrowserToolOutput(Result<BrowserResult, BrowserToolError>);

impl From<Result<BrowserResult, BrowserToolError>> for BrowserToolOutput {
    fn from(value: Result<BrowserResult, BrowserToolError>) -> Self {
        Self(value)
    }
}

impl IntoToolResult for BrowserToolOutput {
    fn into_tool_result(self) -> aither_core::Result<ToolResult> {
        match self.0 {
            Ok(result) => ToolResult::json(&result),
            Err(error) => Ok(ToolResult::error(error.to_string())),
        }
    }
}

pub type BrowserBroker = ToolRequestBroker<BrowserRequest, Result<BrowserResult, BrowserToolError>>;
pub type BrowserQueue = ToolRequestQueue<BrowserRequest, Result<BrowserResult, BrowserToolError>>;

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
    type Res = BrowserToolOutput;

    async fn call(&self, args: Self::Arguments) -> aither_core::Result<Self::Res> {
        let Some(broker) = self.broker.as_ref() else {
            return Ok(BrowserToolOutput::from(Err(
                BrowserToolError::UnavailableInRuntime,
            )));
        };
        let response = broker
            .request(BrowserRequest {
                session_id: self.session_id.clone(),
                args,
            })
            .await?;
        Ok(BrowserToolOutput::from(response))
    }
}
