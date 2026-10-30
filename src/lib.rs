//! May - A warm, boundaried AI assistant.
//!
//! May is a general-purpose AI assistant built on the aither-agent framework
//! with session management, persistent chat history, and sandboxed execution.

/// Agent construction, tools, and runtime glue.
pub mod agent;
/// Main agents on one machine discover each other and exchange messages.
pub mod agent_link;
/// Persistent storage for OAuth credentials.
pub mod auth_store;
/// Managed browser sessions backing the browser tool.
pub mod browser_service;
/// Bundled resources shipped with the binary.
pub mod bundle;
/// Provider prompt-cache lifetime.
pub mod cache_lifetime;
/// `ChatGPT` subscription OAuth flow.
pub mod chatgpt;
/// Configuration loading and types.
pub mod command_output;
pub mod config;
/// Container runtime detection, images, and lifecycle.
pub mod container;
/// GitHub Copilot OAuth flow.
pub mod copilot;
/// SQLite persistence for sessions, messages, and app state.
pub mod db;
/// Live decode-speed measurement for streamed model output.
pub mod decode_meter;
/// Embedding provider resolution for retrieval tools.
pub mod embedding_provider;
mod file_tracker;
mod fs_utils;
mod heel;
/// Issue tracking: may issues by default, GitHub when configured.
pub mod issue;
mod live_response;
/// Location lookup for location-aware profiles.
pub mod location;
pub mod log_ring;
/// MCP server configuration, connections, and management.
pub mod mcp;
/// Profile memory extraction and injection.
pub mod memory;
/// Model selection and filtering helpers.
pub mod model_selector;
/// Model listing and metadata.
pub mod models;
pub use aither_sandbox::network_policy;
/// Web-push notification delivery and mailbox.
pub mod notification;
/// Large pasted text rendered for the model.
pub mod pasted_text;
/// Profile model and environment resolution.
pub mod profile;
/// `@`-mention extraction and prompt text building.
pub mod prompt_mentions;
/// Scheduled task execution.
pub mod scheduler;

pub mod secrets;
/// Session model, history, and on-disk layout.
pub mod session;
/// Agent skill discovery and management.
pub mod skill;
/// Startup validation of configuration files.
pub mod startup;
/// Subagent registration and dispatch.
pub mod subagent;
/// Scheduled-task trigger definitions.
pub mod task_trigger;
/// Background task types and execution.
pub mod tasks;
/// Terminal commands exposed to the agent.
pub mod terminal_commands;
/// Permission handling for terminal command execution.
pub mod terminal_permission_handler;
/// Todo checklist parsing from agent output.
pub mod todo_parser;
/// Tool request dispatch to session handlers.
pub mod tool_dispatch;
/// Miscellaneous tool helpers.
pub mod tools;
/// Terminal user interface.
pub mod tui;
/// Event bus publishing UI-facing state changes.
pub mod ui_event_bus;
/// HTTP/SSE server backing the web UI.
pub mod ui_server;
/// Workspace request handling.
pub mod workspace;
/// XML block serialization for prompt/context payloads.
pub mod xml;
pub use aither_agent::ask_user;
pub use aither_agent::workspace_request;

pub use agent::{
    AgentEnvironment, ContextCategory, ContextCategoryRow, ContextReport, MayAgent,
    PermissionApprover, PermissionHistoryMode, PermissionRequest, PermissionRequestMode,
    ResolveAgentEnvironmentError, ResolvedProviderRuntime, TaskRuntimeKind, generate_session_title,
    resolve_agent_environment_for_session, resolve_provider_runtime,
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
