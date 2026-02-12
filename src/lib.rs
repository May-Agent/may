//! May - A warm, boundaried AI assistant.
//!
//! May is a general-purpose AI assistant built on the aither-agent framework
//! with session management, persistent chat history, and sandboxed execution.

pub mod agent;
pub mod bundle;
pub mod config;
pub mod copilot;
pub mod db;
pub mod mcp;
pub mod models;
pub mod session;
pub mod skill;
pub mod tui;
pub mod ui_server;
pub mod workspace;
pub use aither_agent::ask_user;
pub use aither_agent::workspace_request;

pub use agent::{
    MayAgent, PermissionApprover, PermissionRequest, PermissionRequestMode, generate_session_title,
};
pub use config::MayConfig;
pub use mcp::{MayMcpServerConfig, McpConfig, McpManager, McpServerInfo};
pub use session::{
    ChatHistory, ChatMessage, MessageRole, Session, SessionId, SessionManager, SessionMetadata,
    SessionStore, SessionSummary,
};
pub use skill::{SkillInfo, SkillManager, SkillSource};
pub use db::ChatDb;
pub use workspace::{Workspace, WorkspaceConfig};
pub use workspace_request::{WorkspaceRequest, WorkspaceRequestBroker, WorkspaceRequestQueue};
