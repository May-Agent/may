//! May agent implementation.

pub(crate) mod ask_user_tool;
pub(crate) mod browser_tool;
#[cfg(test)]
mod compaction_tests;
#[cfg(target_os = "macos")]
pub mod computer_tool;
mod context_dump;
mod context_report;
mod environment;
mod hooks;
#[cfg(test)]
mod idle_compaction_tests;
pub(crate) mod imagine;
pub(crate) mod issue_tool;
mod live_settings;
mod may;
pub(crate) mod notify_tool;
#[cfg(test)]
mod open_questions_tests;
mod permission;
mod provider;
pub(crate) mod register_tool;
pub mod runner;
mod runtime_context;
/// Session tool registration for the agent.
pub mod session_tool;
/// Authorization for `terminal` ssh-mode targets.
pub mod ssh_authorizer;
/// Live tracking of `subagent` command invocations.
pub mod subagent_tracker;
pub(crate) mod task_tool;
pub mod tool_environment_cache;

pub use browser_tool::{
    BrowserArgs, BrowserBroker, BrowserRequest, BrowserResult, channel as browser_channel,
};
pub use context_dump::ContextDump;
pub use context_report::{ContextCategory, ContextCategoryRow, ContextReport};
pub use environment::{ResolveAgentEnvironmentError, resolve_agent_environment_for_session};
pub use imagine::ImageGeneratorConfig;
pub use live_settings::AgentLiveSettings;
pub use may::{
    AgentEnvironment, MayAgent, MayAgentConfig, MayAgentError, MayAgentSessionInput,
    generate_session_title,
};
pub use notify_tool::{NotifyArgs, NotifyBroker, NotifyResult, channel as notify_channel};
pub use permission::{
    PermissionApprover, PermissionHistoryMode, PermissionHistoryModeParseError, PermissionRequest,
    PermissionRequestMode, PermissionRequestModeParseError,
};
pub use provider::{
    RequestParameterizedModel, ResolvedProviderRuntime, build_fast_provider_chain,
    provider_candidate_count, resolve_provider_runtime,
};
pub use runner::{
    AgentHandle, AgentWorker, ForeignTurn, RemoteTurn, RunLease, TuiEvent, spawn_idle_worker,
    spawn_worker,
};
pub use subagent_tracker::{
    SubagentHandles, SubagentRunSnapshot, SubagentTracker, TrackedSubagentTool, UsageObservedModel,
};
pub use task_tool::{
    TaskBroker, TaskCreateRequest, TaskCreateResult, TaskRuntimeKind, channel as task_channel,
};
pub use tool_environment_cache::{
    HomeBackend, ToolEnvironmentCache, ToolEnvironmentCacheError, ToolEnvironmentKey,
};
