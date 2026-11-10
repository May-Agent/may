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

pub mod bash;
/// Managed browser sessions backing the browser tool.
pub mod browser_service;
/// Bundled resources shipped with the binary.
pub mod bundle;
/// Provider prompt-cache lifetime.
pub mod cache_lifetime;
/// `ChatGPT` subscription OAuth flow.
pub mod chatgpt;
pub mod ci;
/// Client of the May daemon over its unix socket.
pub mod client;
/// Configuration loading and types.
pub mod command_output;
/// May's rules on what a terminal script may run.
pub mod command_policy;
/// How a terminal call's script is read before it runs.
pub mod command_reading;
/// One virtual desktop per session — computer use.
pub mod computer;
pub mod config;
/// Container runtime detection, images, and lifecycle.
pub mod container;
/// GitHub Copilot OAuth flow.
pub mod copilot;
#[cfg(unix)]
pub mod crash_signal;
/// The resident daemon that runs every agent loop and serves the UIs.
pub mod daemon;
/// SQLite persistence for sessions, messages, and app state.
pub mod db;
/// Live decode-speed measurement for streamed model output.
pub mod decode_meter;
mod file_tracker;
mod fs_utils;
/// xAI Grok subscription OAuth flow.
pub mod grok;
/// The one way git runs on the host.
pub mod host_git;
mod id;
/// Issue tracking: may issues by default, GitHub when configured.
pub mod issue;
/// The machine-wide jobserver token pool shared by every terminal.
pub mod jobserver;
mod live_response;
/// Location lookup for location-aware profiles.
pub mod location;
pub mod log_ring;
#[cfg(test)]
mod managed_environment_tests;
/// MCP server configuration, connections, and management.
pub mod mcp;
/// Profile memory extraction and injection.
pub mod memory;
/// Model-facing ids: session names and each session's pool of words.
pub mod model_ids;
/// Model selection and filtering helpers.
pub mod model_selector;
/// Model listing and metadata.
pub mod models;
pub use aither_sandbox::network_policy;
/// Signing of the commits agents make in their sandbox.
pub mod commit_signing;
/// Web-push notification delivery and mailbox.
pub mod notification;
/// `OpenRouter` OAuth login flow.
pub mod openrouter;
/// Catches a panic at the boundary of the operation that raised it.
pub mod panic_boundary;
/// Large pasted text rendered for the model.
pub mod pasted_text;
/// `pr` — the session's landing object: numbered, session-owned PRs.
pub mod pr;
/// Profile model and environment resolution.
pub mod profile;
/// `@`-mention extraction and prompt text building.
pub mod prompt_mentions;

mod quota;
/// Per-session repository clones inside the session sandbox.
pub mod repo_clone;
pub mod repo_gate;
pub mod repo_preflight;
/// repo-search daemon: code, session and issue search.
pub mod repo_search;
/// `/import-claude` and `/import-codex`: another agent's global rules and
/// skills brought into May.
pub mod rules_import;
/// Scheduled task execution.
pub mod scheduler;
/// Repository trust: the first-contact decision, resolved per repo.
pub mod trust;

/// The user's global sleep state.
/// May's IPC command bridge.
pub mod ipc;
pub mod secrets;
/// Session model, history, and on-disk layout.
pub mod session;
pub mod shell;
/// Agent skill discovery and management.
pub mod skill;
pub mod sleep;
/// Startup validation of configuration files.
pub mod startup;
/// The rows of `tasks.md` as clients see them.
pub mod task_rows;
/// Scheduled-task trigger definitions.
pub mod task_trigger;
/// The command a scheduled task's run uses to prompt its session.
pub mod task_wake;
/// `tasks.md`: its format and the harness that keeps it current.
pub mod tasks_md;
/// Terminal commands exposed to the agent.
pub mod terminal_commands;
/// Permission handling for terminal command execution.
pub mod terminal_permission_handler;
/// Tool request dispatch to session handlers.
pub mod tool_dispatch;
/// Miscellaneous tool helpers.
pub mod tools;
/// Terminal user interface.
pub mod tui;
/// Event bus publishing UI-facing state changes.
pub mod ui_event_bus;

pub(crate) mod video;
/// HTTP/SSE server backing the web UI.
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
pub use mcp::{MayMcpServerConfig, McpConfig, McpManager, McpServerInfo};
pub use model_selector::{ModelSelector, ModelSelectorParseError};
pub use profile::{HomeEnvironment, PinnedItem, ProfileEnvironment, ProfileInfo, ProfileSessions};
pub use session::{
    ChatHistory, ChatMessage, MayHomePaths, MessageRole, ProfileHomePaths, Session, SessionId,
    SessionManager, SessionMetadata, SessionPlanState, SessionStore, SessionSummary,
};
pub use skill::{SkillInfo, SkillManager, SkillSource};
pub use terminal_commands::{
    SharedTerminalCommandConfig, TerminalCommandsError, register_shared_terminal_commands,
};
pub use terminal_permission_handler::TerminalPermissionHandler;
pub use workspace::{Workspace, WorkspaceConfig};
pub use workspace_request::{WorkspaceRequest, WorkspaceRequestBroker, WorkspaceRequestQueue};
pub use xml::{XmlBlock, XmlBlockError};

/// Test support shared by the crate's `#[cfg(test)]` code.
#[cfg(test)]
mod test_support {
    /// An output store in `dir` with the default model-facing id allocator.
    pub async fn test_output_store(
        dir: impl Into<std::path::PathBuf>,
    ) -> aither_sandbox::OutputStore {
        aither_sandbox::OutputStore::new(dir, aither_core::AnyIdAllocator::default())
            .await
            .expect("outputs store")
    }
}
