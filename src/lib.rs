//! May - A warm, boundaried AI assistant.
//!
//! May is a general-purpose AI assistant built on the aither-agent framework
//! with session management, persistent chat history, and sandboxed execution.

pub mod agent;
pub mod bundle;
pub mod config;
pub mod copilot;
mod fs_util;
pub mod mcp;
pub mod models;
pub mod session;
pub mod tui;
pub mod workspace;
pub use aither_agent::ask_user;
pub use aither_agent::workspace_request;

pub use agent::{generate_session_title, MayAgent};
pub use config::MayConfig;
pub use session::{
    ChatHistory, ChatMessage, MessageRole, Session, SessionId, SessionManager, SessionMetadata, SessionStore,
    SessionSummary,
};
pub use workspace::{Workspace, WorkspaceConfig};
pub use workspace_request::{WorkspaceRequest, WorkspaceRequestBroker, WorkspaceRequestQueue};
pub use mcp::{McpConfig, McpManager, McpServerInfo, MayMcpServerConfig};
