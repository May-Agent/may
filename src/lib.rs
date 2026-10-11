//! May - A warm, boundaried AI assistant.
//!
//! May is a general-purpose AI assistant built on the aither-agent framework
//! with session management, persistent chat history, and sandboxed execution.

pub mod agent;
pub mod auth_store;
pub mod browser_service;
pub mod bundle;
pub mod chatgpt;
pub mod config;
pub mod container;
pub mod copilot;
pub mod db;
pub mod embedding_provider;
mod fs_utils;
mod heel;
mod live_response;
pub mod location;
pub mod mcp;
pub mod memory;
pub mod model_selector;
pub mod models;
pub use aither_sandbox::network_policy;
pub mod notification;
pub mod profile;
pub mod prompt_mentions;
pub mod scheduler;
pub mod session;
pub mod skill;
pub mod subagent;
pub mod task_trigger;
pub mod tasks;
pub mod terminal_commands;
pub mod terminal_permission_handler;
pub mod todo_parser;
pub mod tool_dispatch;
pub mod tui;
pub mod ui_event_bus;
pub mod ui_server;
pub mod workspace;
pub mod xml;
pub use aither_agent::ask_user;
pub use aither_agent::workspace_request;

pub use agent::{
    AgentEnvironment, MayAgent, PermissionApprover, PermissionHistoryMode, PermissionRequest,
    PermissionRequestMode, ResolveAgentEnvironmentError, ResolvedProviderRuntime, TaskRuntimeKind,
    generate_session_title, resolve_agent_environment_for_session, resolve_provider_runtime,
};
pub use config::MayConfig;
pub use db::ChatDb;
pub use heel::configure_heel_binary;
pub use mcp::{MayMcpServerConfig, McpConfig, McpManager, McpServerInfo};
pub use model_selector::{ModelSelector, ModelSelectorParseError};
pub use profile::{HomeEnvironment, PinnedItem, ProfileEnvironment, ProfileInfo, ProfileSessions};
pub use session::{
    ChatHistory, ChatMessage, MayHomePaths, MessageRole, ProfileHomePaths, Session, SessionId,
    SessionManager, SessionMetadata, SessionPlanState, SessionStore, SessionSummary,
};
pub use skill::{SkillInfo, SkillManager, SkillSource};
pub use subagent::{SubagentInfo, SubagentManager};
pub use terminal_commands::{SharedTerminalCommandConfig, register_shared_terminal_commands};
pub use terminal_permission_handler::TerminalPermissionHandler;
pub use workspace::{Workspace, WorkspaceConfig};
pub use workspace_request::{WorkspaceRequest, WorkspaceRequestBroker, WorkspaceRequestQueue};
pub use xml::{XmlBlock, XmlBlockError};
