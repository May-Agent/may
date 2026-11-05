//! May agent implementation.

pub(crate) mod acp_known;
pub mod acp_subagent;
mod agents_md;
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
pub(crate) mod idle_compaction_tests;
pub(crate) mod imagine;
pub(crate) mod issue_tool;
mod live_settings;
mod may;
pub(crate) mod notify_tool;
#[cfg(test)]
mod open_questions_tests;
mod permission;
pub(crate) mod pr_tool;
mod provider;
pub(crate) mod register_tool;
pub(crate) mod repo_tool;
pub mod runner;
mod runtime_context;
#[cfg(test)]
mod secret_redaction_tests;
/// Session tool registration for the agent.
pub mod session_tool;
/// Authorization for `terminal` ssh-mode targets.
pub mod ssh_authorizer;
/// Live tracking of `subagent` command invocations.
pub mod subagent_tracker;
pub(crate) mod task_tool;
pub mod tool_environment_cache;
pub(crate) mod tunnel_tool;
mod usage;

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
pub(crate) use provider::reasoning_text_requested;
pub use provider::{
    ProviderLlm, RedactedModel, RequestParameterizedModel, ResolvedProviderRuntime,
    build_fast_provider_chain, provider_candidate_count, resolve_provider_runtime,
};
pub use runner::{
    AgentHandle, AgentWorker, BackgroundWakes, ForeignTurn, RemoteTurn, RunLease, TuiEvent,
    spawn_idle_worker, spawn_worker,
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
pub use usage::{ModelUsageRow, SessionUsageReport, assemble_usage_report};
