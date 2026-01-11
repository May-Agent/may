//! Workspace request tool.
//!
//! Provides a `Tool` implementation for requesting write access to directories
//! outside the sandbox. The tool communicates with the TUI via channels.

use std::borrow::Cow;

use aither_core::llm::tool::{Tool, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};

/// Request access to a directory outside the sandbox.
///
/// This command asks for permission to read/write files in the specified
/// directory. The user will be prompted to approve or deny the request.
/// Use this when you need to modify files in the user's project directory
/// or other locations outside your sandbox.
#[derive(Debug, Clone, JsonSchema, Deserialize)]
pub struct RequestWorkspaceArgs {
    /// The directory path to request access to (absolute or relative to cwd).
    pub path: String,

    /// Why access is needed (shown to user during approval prompt).
    #[serde(default = "default_reason")]
    pub reason: String,
}

fn default_reason() -> String {
    "To modify files in this directory".to_string()
}

/// A workspace access request sent from the tool to the TUI.
#[derive(Debug)]
pub struct WorkspaceRequest {
    path: String,
    reason: String,
    response: oneshot::Sender<bool>,
}

impl WorkspaceRequest {
    /// The directory path requested.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Reason for the request.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Approve the workspace access request.
    pub fn approve(self) {
        let _ = self.response.send(true);
    }

    /// Deny the workspace access request.
    pub fn deny(self) {
        let _ = self.response.send(false);
    }
}

/// Sender for workspace requests (held by the tool).
pub type WorkspaceRequestSender = mpsc::UnboundedSender<WorkspaceRequest>;

/// Receiver for workspace requests (held by the TUI).
pub type WorkspaceRequestReceiver = mpsc::UnboundedReceiver<WorkspaceRequest>;

/// Create a new workspace request channel pair.
pub fn channel() -> (WorkspaceRequestSender, WorkspaceRequestReceiver) {
    mpsc::unbounded_channel()
}

/// Tool for requesting workspace access.
#[derive(Debug, Clone)]
pub struct RequestWorkspaceTool {
    sender: WorkspaceRequestSender,
}

impl RequestWorkspaceTool {
    /// Create a new workspace request tool.
    pub fn new(sender: WorkspaceRequestSender) -> Self {
        Self { sender }
    }
}

impl Tool for RequestWorkspaceTool {
    fn name(&self) -> Cow<'static, str> {
        "request_workspace".into()
    }

    type Arguments = RequestWorkspaceArgs;

    async fn call(&self, args: Self::Arguments) -> aither_core::Result<ToolOutput> {
        let (tx, rx) = oneshot::channel();

        self.sender
            .send(WorkspaceRequest {
                path: args.path.clone(),
                reason: args.reason.clone(),
                response: tx,
            })
            .map_err(|_| anyhow::anyhow!("TUI not available for workspace request"))?;

        match rx.await {
            Ok(true) => Ok(ToolOutput::text(format!(
                "Workspace access granted for: {}",
                args.path
            ))),
            Ok(false) => Err(anyhow::anyhow!("Workspace access denied by user")),
            Err(_) => Err(anyhow::anyhow!("Workspace request was cancelled")),
        }
    }
}

