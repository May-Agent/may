use std::borrow::Cow;

use crate::harness::tool_request::{
    ToolRequestBroker, ToolRequestQueue, channel as tool_request_channel,
};
use aither_core::llm::{Tool, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Source path in the current project to register.
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

/// What the model reads after a register command was handled.
#[derive(Debug, Template)]
#[template(path = "register_done.txt", escape = "none")]
struct RegisterDoneText<'a> {
    registered: &'a [String],
}

/// Unified broker/queue types for register commands.
pub type RegisterBroker = ToolRequestBroker<RegisterRequest, Result<RegisterResult, String>>;
pub type RegisterQueue = ToolRequestQueue<RegisterRequest, Result<RegisterResult, String>>;

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

/// Marker trait for register tool variants.
pub trait RegisterKind: Send + Sync + Clone + std::fmt::Debug + 'static {
    const TOOL_NAME: &'static str;
    const DESCRIPTION: &'static str;
}

/// Skills register variant.
#[derive(Debug, Clone)]
pub struct Skills;

impl RegisterKind for Skills {
    const TOOL_NAME: &'static str = "register-skills";
    const DESCRIPTION: &'static str = "Register a local directory as a profile skill; pending user approval, it becomes available to sessions.";
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

fn normalize_path(args: &RegisterPathArgs) -> aither_core::Result<String> {
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

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(K::DESCRIPTION)
    }

    type Arguments = RegisterPathArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        let broker = self.broker.as_ref().ok_or(RegisterToolError::Unavailable {
            tool_name: K::TOOL_NAME,
        })?;
        let path = normalize_path(&args)?;
        let request = RegisterRequest {
            session_id: self.session_id.clone(),
            path,
        };
        let response = broker.request(request).await?;
        let result = response.map_err(RegisterToolError::Rejected)?;

        let text = RegisterDoneText {
            registered: &result.registered,
        }
        .render()
        .expect("register done template must render");
        Ok(ToolResult::text(text))
    }
}

#[cfg(test)]
mod tests {
    use askama::Template;

    use super::RegisterDoneText;

    #[test]
    fn register_result_lists_what_was_registered() {
        let registered = vec!["skills/a".to_string(), "skills/b".to_string()];
        assert_eq!(
            RegisterDoneText {
                registered: &registered,
            }
            .render()
            .expect("register done template must render"),
            "Registered 2 item(s): skills/a, skills/b. The user must approve the registration before it takes effect."
        );
    }
}
