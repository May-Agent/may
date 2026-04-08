use std::borrow::Cow;

use aither_agent::tool_request::{
    ToolRequestBroker, ToolRequestQueue, bounded_channel as bounded_tool_request_channel,
    channel as tool_request_channel,
};
use aither_core::llm::{Tool, ToolResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct RegisterPathArgs {
    /// Source path in the current project.
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub session_id: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResult {
    pub registered: Vec<String>,
}

/// Unified broker/queue types for register commands.
pub type RegisterBroker = ToolRequestBroker<RegisterRequest, Result<RegisterResult, String>>;
pub type RegisterQueue = ToolRequestQueue<RegisterRequest, Result<RegisterResult, String>>;

const SUBAGENT_QUEUE_CAPACITY: usize = 64;

#[derive(Debug, thiserror::Error)]
enum RegisterToolError {
    #[error("path must not be empty")]
    EmptyPath,
    #[error("{tool_name} command is unavailable in this runtime")]
    Unavailable { tool_name: &'static str },
    #[error("register request was rejected: {0}")]
    Rejected(String),
}

#[must_use]
pub fn skills_channel() -> (RegisterBroker, RegisterQueue) {
    tool_request_channel()
}

#[must_use]
pub fn subagents_channel() -> (RegisterBroker, RegisterQueue) {
    bounded_tool_request_channel(SUBAGENT_QUEUE_CAPACITY)
}

/// Marker trait for register tool variants.
pub trait RegisterKind: Send + Sync + Clone + std::fmt::Debug + 'static {
    const TOOL_NAME: &'static str;
}

/// Skills register variant.
#[derive(Debug, Clone)]
pub struct Skills;

impl RegisterKind for Skills {
    const TOOL_NAME: &'static str = "register-skills";
}

/// Subagents register variant.
#[derive(Debug, Clone)]
pub struct Subagents;

impl RegisterKind for Subagents {
    const TOOL_NAME: &'static str = "register-subagents";
}

/// Unified register tool generic over the kind of registration.
#[derive(Debug, Clone)]
pub struct RegisterTool<K: RegisterKind> {
    session_id: String,
    broker: Option<RegisterBroker>,
    _kind: std::marker::PhantomData<K>,
}

impl<K: RegisterKind> RegisterTool<K> {
    #[must_use]
    pub const fn new(session_id: String, broker: Option<RegisterBroker>) -> Self {
        Self {
            session_id,
            broker,
            _kind: std::marker::PhantomData,
        }
    }
}

fn normalize_path(args: RegisterPathArgs) -> aither_core::Result<String> {
    let path = args.path.trim();
    if path.is_empty() {
        return Err(RegisterToolError::EmptyPath.into());
    }
    Ok(path.to_string())
}

impl<K: RegisterKind> Tool for RegisterTool<K> {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(K::TOOL_NAME)
    }

    type Arguments = RegisterPathArgs;
    type Res = ToolResult;

    async fn call(&self, args: Self::Arguments) -> aither_core::Result<Self::Res> {
        let broker = self.broker.as_ref().ok_or(RegisterToolError::Unavailable {
            tool_name: K::TOOL_NAME,
        })?;
        let path = normalize_path(args)?;
        let request = RegisterRequest {
            session_id: self.session_id.clone(),
            path,
        };
        let response = broker.request(request).await?;
        let result = response.map_err(RegisterToolError::Rejected)?;

        ToolResult::json(&serde_json::json!({
            "ok": true,
            "registered": result.registered,
            "approval_required": true
        }))
    }
}
