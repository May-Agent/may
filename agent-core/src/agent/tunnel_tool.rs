//! The `tunnel` IPC command: registers a reachable HTTP endpoint with the
//! daemon so it can serve it to the user wherever they view May.

use std::borrow::Cow;

use crate::harness::tool_request::{
    ToolRequestBroker, ToolRequestQueue, channel as tool_request_channel,
};
use aither_core::llm::{Tool, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Expose an HTTP endpoint through the daemon.
///
/// `tunnel http://localhost:3000` returns a word id; the endpoint is then
/// proxied under `/tunnel/<session>/<id>/` on every address the daemon serves, so the
/// link works however the user reaches May (localhost, tailnet, LAN). Emit
/// `<tunnel id="<id>" />` in the reply where the link should appear.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TunnelArgs {
    /// The URL the daemon should proxy, e.g. `http://localhost:3000`.
    /// The scheme defaults to `http` when omitted.
    pub url: String,
}

/// A tunnel request with the session it was made from, filled in by the tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelRequest {
    /// Session the endpoint belongs to; the daemon resolves the session's
    /// environment (host loopback vs container IP) from it.
    pub session_id: String,
    /// The requested target URL.
    pub url: String,
}

/// Outcome of a handled tunnel request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelResult {
    /// The word id the daemon serves the endpoint under.
    pub id: String,
    /// The normalized target URL being proxied.
    pub url: String,
    /// Path prefix the endpoint is proxied under, e.g. `/tunnel/<session>/<id>/`.
    pub path: String,
    /// Local URL on the daemon host.
    pub local_url: String,
}

/// Broker half of the tunnel-request channel: submits requests to the
/// session runtime.
pub type TunnelBroker = ToolRequestBroker<TunnelRequest, Result<TunnelResult, String>>;
/// Queue half of the tunnel-request channel: the session runtime drains
/// pending tunnel requests from it.
pub type TunnelQueue = ToolRequestQueue<TunnelRequest, Result<TunnelResult, String>>;

/// A `tunnel` call failed before it could return a tool result.
#[derive(Debug, thiserror::Error)]
pub enum TunnelToolError {
    /// The URL to expose was empty.
    #[error("tunnel url must not be empty")]
    EmptyUrl,
    /// The tunnel command is not wired in this runtime.
    #[error("tunnel command is unavailable in this runtime")]
    Unavailable,
    /// The tunnel service refused the request.
    #[error("tunnel request was rejected: {0}")]
    Rejected(String),
    /// The request could not reach the tunnel service.
    #[error("tunnel request failed: {0}")]
    Request(String),
}

/// Create a paired tunnel broker and queue.
#[must_use]
pub fn channel() -> (TunnelBroker, TunnelQueue) {
    tool_request_channel()
}

/// What the model reads after a tunnel was opened.
#[derive(Debug, Template)]
#[template(path = "tunnel_open.txt", escape = "none")]
struct TunnelOpenText {
    id: String,
    url: String,
    path: String,
    local_url: String,
}

/// The `tunnel` tool: exposing the session's serves publicly.
#[derive(Clone)]
pub struct TunnelTool {
    broker: Option<TunnelBroker>,
    session_id: String,
}

impl std::fmt::Debug for TunnelTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelTool")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl TunnelTool {
    /// A tunnel tool for `session_id`, queueing through `broker`.
    #[must_use]
    pub const fn new(session_id: String, broker: Option<TunnelBroker>) -> Self {
        Self { broker, session_id }
    }
}

impl Tool for TunnelTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("tunnel")
    }

    type Arguments = TunnelArgs;
    type Res = ToolResult;
    type Error = TunnelToolError;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: aither_core::llm::ToolContext,
    ) -> Result<Self::Res, Self::Error> {
        let broker = self.broker.as_ref().ok_or(TunnelToolError::Unavailable)?;
        let url = args.url.trim();
        if url.is_empty() {
            return Err(TunnelToolError::EmptyUrl);
        }
        let response = broker
            .request(TunnelRequest {
                session_id: self.session_id.clone(),
                url: url.to_string(),
            })
            .await
            .map_err(|error| TunnelToolError::Request(error.to_string()))?;
        let result = response.map_err(TunnelToolError::Rejected)?;

        let text = TunnelOpenText {
            id: result.id,
            url: result.url,
            path: result.path,
            local_url: result.local_url,
        }
        .render()
        .expect("tunnel open template must render");
        Ok(ToolResult::text(text))
    }
}
