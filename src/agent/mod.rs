//! May agent implementation.

pub(crate) mod browser_tool;
mod context_report;
mod environment;
mod hooks;
pub(crate) mod imagine;
pub(crate) mod issue_tool;
mod may;
pub(crate) mod notify_tool;
mod permission;
mod provider;
pub(crate) mod register_tool;
pub mod runner;
mod runtime_context;
mod search_match;
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
pub use context_report::{ContextCategory, ContextCategoryRow, ContextReport};
pub use environment::{ResolveAgentEnvironmentError, resolve_agent_environment_for_session};
pub use imagine::ImageGeneratorConfig;
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
pub use runner::{AgentHandle, AgentWorker, TuiEvent, spawn_worker};
pub use subagent_tracker::{
    SubagentRunSnapshot, SubagentTracker, TrackedSubagentTool, UsageObservedModel,
};
pub use task_tool::{
    TaskBroker, TaskCreateRequest, TaskCreateResult, TaskRuntimeKind, channel as task_channel,
};
pub use tool_environment_cache::{
    HomeBackend, ToolEnvironmentCache, ToolEnvironmentCacheError, ToolEnvironmentKey,
};
