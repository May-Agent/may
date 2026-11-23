//! May's agent core: the `MayAgent` wrapper, the terminal-command surface,
//! containers, cloud machines, PRs, profiles, memory and the runtimes the
//! daemon and binary assemble into the application.

/// Agent construction, tools, and runtime glue.
pub mod agent;
/// Main agents on one machine discover each other and exchange messages.
pub mod agent_link;
/// Persistent storage for OAuth credentials.
pub use may_config::auth_store;

pub use may_config::bash;
/// Managed browser sessions backing the browser tool.
pub mod browser_service;
/// Bundled resources shipped with the binary.
pub use may_client::bundle;
/// Client of the May daemon over its unix socket.
pub use may_client::client;
/// The `search` tool: code search over a session's worktrees.
pub use may_client::code_search;
/// CI durations: the jobs, steps and tests of a session PR's checks.
pub use may_git_forge::ci;
/// `ChatGPT` subscription OAuth flow.
pub use may_harness::chatgpt;
/// Provider prompt-cache lifetime.
pub use may_protocol::cache_lifetime;
/// Cloud machines: providers, their catalogs and machine lifecycles.
pub mod cloud;
/// May's rules on what a terminal script may run.
pub use may_command_policy as command_policy;
pub use may_config::config;
/// One virtual desktop per session — computer use.
pub use may_media::computer;
/// Streamed command output for the model.
pub use may_protocol::command_output;
/// How a terminal call's script is read before it runs.
pub use may_protocol::command_reading;
/// Container runtime detection, images, and lifecycle.
pub mod container;
/// Forge coordinates and per-host authentication (GitHub, GitLab).
pub use may_config::forge;
pub use may_config::fs_utils;
/// The one way git runs on the host.
pub use may_config::host_git;
/// SQLite persistence for sessions, messages, and app state.
pub use may_db as db;
/// GitHub Copilot OAuth flow.
pub use may_harness::copilot;
#[cfg(unix)]
pub use may_harness::crash_signal;
/// xAI Grok subscription OAuth flow.
pub use may_harness::grok;
/// Live decode-speed measurement for streamed model output.
pub use may_protocol::decode_meter;
pub use may_protocol::file_tracker;
pub use may_protocol::id;
/// Issue tracking: may issues by default, GitHub when configured.
pub mod issue;
/// The machine-wide jobserver token pool shared by every terminal.
pub use may_config::jobserver;
pub use may_protocol::daemon_api::{
    ASK_USER_NOT_OPEN, DEFAULT_PORT, encode_path_segment, session_path,
};
pub use may_protocol::layout_consts::{CONTAINER_WORKSPACE, SSH_KIND};
pub use may_protocol::live_response;
/// Location lookup for location-aware profiles.
pub use may_protocol::location;
pub use may_protocol::log_ring;
#[cfg(test)]
mod managed_environment_tests;
/// MCP server configuration, connections, and management.
pub use may_harness::mcp;
/// Profile memory extraction and injection.
pub mod memory;
pub use aither_sandbox::network_policy;
/// Signing of the commits agents make in their sandbox.
pub use may_config::commit_signing;
/// Model-facing ids: session names and each session's pool of words.
pub use may_protocol::model_ids;
/// Model selection and filtering helpers.
pub use may_protocol::model_selector;
/// Model listing and metadata.
pub use may_providers as models;
/// Web-push notification delivery and mailbox.
pub mod notification;
/// `OpenRouter` OAuth login flow.
pub use may_config::openrouter;
/// Large pasted text rendered for the model.
pub use may_config::pasted_text;
/// Catches a panic at the boundary of the operation that raised it.
pub use may_protocol::panic_boundary;
/// `pr` — the session's landing object: numbered, session-owned PRs.
pub mod pr;
/// Profile model and environment resolution.
pub mod profile;
/// `@`-mention extraction and prompt text building.
pub mod prompt_mentions;

/// Per-session repository clones inside the session sandbox.
pub use may_client::repo_clone;
pub use may_protocol::quota;
pub mod repo_gate;
pub use may_harness::repo_preflight;
/// repo-search daemon: code, session and issue search.
pub use may_repo_search as repo_search;
/// `/import-claude` and `/import-codex`: another agent's global rules and
/// skills brought into May.
pub mod rules_import;
/// Repository trust: the first-contact decision, resolved per repo.
pub use may_client::trust;

/// Session model, history, and on-disk layout.
pub use may_client::session;
pub use may_config::secrets;
/// What May's agents do around the loop.
pub use may_harness::harness;
/// May's IPC command bridge.
pub use may_harness::ipc;
pub mod shell;
/// Agent skill discovery and management.
pub use may_client::skill;
pub mod sleep;
/// Startup validation of configuration files.
pub mod startup;
/// The `subagent` command and the agents it runs.
pub mod subagent;
/// Scheduled-task trigger definitions.
pub use may_harness::task_trigger;
/// The rows of `tasks.md` as clients see them.
pub use may_protocol::task_rows;
/// The command a scheduled task's run uses to prompt its session.
pub mod task_wake;
/// `tasks.md`: its format and the harness that keeps it current.
pub use may_harness::tasks_md;
/// Terminal commands exposed to the agent.
pub mod terminal_commands;
/// Permission handling for terminal command execution.
pub mod terminal_permission_handler;
/// Event bus publishing UI-facing state changes.
pub use may_client::ui_event_bus;
/// HTTP/SSE server backing the web UI.
/// Workspace request handling.
pub use may_client::workspace;
/// Self-update from CI artifacts (dev builds) or GitHub releases.
pub use may_harness::update;
/// The encoded video pipeline a stream carries.
pub use may_media::video;
/// On-device OCR and conditional downscaling for text-dense images.
pub use may_media::vision_ocr;
/// XML block serialization for prompt/context payloads.
pub use may_protocol::xml;
/// Miscellaneous tool helpers.
pub use may_tool_dist as tools;

pub use harness::ask_user;
pub use harness::workspace_request;

#[cfg(any(test, feature = "testing"))]
pub mod test_sandbox;

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
    SharedServices, SharedTerminalCommandConfig, TerminalCommandsError,
    register_shared_terminal_commands,
};
pub use terminal_permission_handler::TerminalPermissionHandler;
pub use workspace::{Workspace, WorkspaceConfig};
pub use workspace_request::{WorkspaceRequest, WorkspaceRequestBroker, WorkspaceRequestQueue};
pub use xml::{XmlBlock, XmlBlockError};

#[cfg(test)]
mod commit_signing_tests;
#[cfg(test)]
mod ipc_bridge_tests;
#[cfg(test)]
mod runaway_tests;
/// The `vision_ocr` test that drives `may-agent`'s `ChildHooks`.
#[cfg(all(test, feature = "vision-ocr", target_os = "macos"))]
mod vision_ocr_tests;

/// Test support shared by the crate's `#[cfg(test)]` code.
#[cfg(test)]
mod test_support {
    #[allow(unused_imports)]
    pub use may_protocol::test_support::{
        ApiReply, ApiRequest, ApiRequests, install_global_executor, serve_api,
    };

    /// An output store in `dir` with the default model-facing id allocator.
    pub async fn test_output_store(
        dir: impl Into<std::path::PathBuf>,
    ) -> aither_sandbox::OutputStore {
        aither_sandbox::OutputStore::new(
            dir,
            aither_core::AnyIdAllocator::default(),
            crate::shell::ShellBudgets::of(&crate::config::ShellSettings::default()).output(),
            crate::shell::MayShellWording::any(),
        )
        .await
        .expect("outputs store")
    }

    /// The launch of a sandboxed script the main agent's call started.
    pub fn main_launch() -> aither_sandbox::LaunchInfo {
        aither_sandbox::LaunchInfo::new(
            aither_sandbox::LaunchLease::fresh().id(),
            None,
            aither_sandbox::SandboxMode::Sandboxed,
        )
    }

    /// The launch of a sandboxed script a subagent's call started.
    pub fn subagent_launch() -> aither_sandbox::LaunchInfo {
        let identity = aither_core::llm::CallIdentity::new(
            aither_core::llm::ToolCallId::new("call"),
            aither_core::llm::AgentIdentity::Subagent(aither_core::llm::SubagentIdentity::new(
                "worker", None,
            )),
        );
        aither_sandbox::LaunchInfo::new(
            aither_sandbox::LaunchLease::fresh().id(),
            Some(identity),
            aither_sandbox::SandboxMode::Sandboxed,
        )
    }

    /// A built command registry and the store its outputs go to: the view a
    /// script's `launch` has of the commands, called without a shell.
    pub struct TestCommands {
        registry: crate::ipc::CommandRegistry,
        outputs: aither_sandbox::OutputStore,
    }

    impl TestCommands {
        /// `builder`'s registry with outputs stored in `dir`.
        pub async fn build(
            builder: crate::ipc::CommandRegistryBuilder,
            dir: impl Into<std::path::PathBuf>,
        ) -> Self {
            Self {
                registry: builder.build(),
                outputs: test_output_store(dir).await,
            }
        }

        /// The registry, for the questions only it answers.
        pub const fn registry(&self) -> &crate::ipc::CommandRegistry {
            &self.registry
        }

        /// Calls `name` with `args` as the script `launch` describes.
        pub async fn call(
            &self,
            launch: &aither_sandbox::LaunchInfo,
            name: &str,
            args: &[&str],
        ) -> crate::ipc::CommandEnvelope {
            let args: Vec<String> = args.iter().map(ToString::to_string).collect();
            self.registry
                .call(launch, None, &self.outputs, name, &args, None)
                .await
        }
    }
}

#[cfg(test)]
mod client_tests;
#[cfg(test)]
mod db_tests;
#[cfg(test)]
mod open_questions_tests;
