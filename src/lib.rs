//! May - A warm, boundaried AI assistant.
//!
//! May is a general-purpose AI assistant built on the aither-agent framework
//! with session management, persistent chat history, and sandboxed execution.

pub mod agent;
pub mod bundle;
pub mod config;
pub mod container;
pub mod copilot;
pub mod db;
mod fs_utils;
pub mod location;
pub mod mcp;
pub mod memory;
pub mod models;
pub mod notification;
pub mod notification_service;
pub mod profile;
pub mod scheduler;
pub mod session;
pub mod skill;
pub mod subagent;
pub mod task_trigger;
pub mod tasks;
pub mod todo_parser;
pub mod tui;
pub mod ui_server;
pub mod workspace;
pub use aither_agent::ask_user;
pub use aither_agent::workspace_request;

pub use agent::{
    AgentEnvironment, MayAgent, PermissionApprover, PermissionRequest, PermissionRequestMode,
    generate_session_title,
};
pub use config::MayConfig;
pub use db::ChatDb;
pub use mcp::{MayMcpServerConfig, McpConfig, McpManager, McpServerInfo};
pub use profile::{PinnedItem, ProfileEnvironment, ProfileInfo, ProfileSessions};
pub use session::{
    ChatHistory, ChatMessage, MessageRole, Session, SessionId, SessionManager, SessionMetadata,
    SessionStore, SessionSummary,
};
pub use skill::{SkillInfo, SkillManager, SkillSource};
pub use subagent::{SubagentInfo, SubagentManager};
pub use workspace::{Workspace, WorkspaceConfig};
pub use workspace_request::{WorkspaceRequest, WorkspaceRequestBroker, WorkspaceRequestQueue};
