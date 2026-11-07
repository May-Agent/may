//! May agent implementation.

pub(crate) mod acp_known;
pub mod acp_sandbox;
pub mod acp_subagent;
mod agents_md;
pub(crate) mod ask_user_tool;
pub(crate) mod browser_tool;
pub mod cache_miss;
#[cfg(test)]
mod compact_tool_tests;
#[cfg(test)]
mod compaction_tests;
#[cfg(target_os = "macos")]
pub mod computer_tool;
mod context_dump;
mod context_report;
mod environment;
mod hooks;
#[cfg(test)]
mod host_load_tests;
#[cfg(test)]
pub(crate) mod idle_compaction_tests;
pub(crate) mod imagine;
pub(crate) mod issue_tool;
#[cfg(test)]
mod live_content_tests;
mod live_settings;
mod may;
#[cfg(test)]
mod open_questions_tests;
pub(crate) mod permission;
pub(crate) mod pr_tool;
#[cfg(test)]
mod prompt_layout_tests;
mod provider;
/// Approval of the issues, pull requests and comments the agent publishes.
pub mod publish_approval;
pub(crate) mod register_tool;
pub(crate) mod repo_tool;
#[cfg(test)]
mod request_parameters_tests;
#[cfg(test)]
mod rules_import_session_tests;
pub mod runner;
mod runtime_context;
#[cfg(test)]
mod secret_redaction_tests;
/// Session tool registration for the agent.
pub mod session_tool;
/// Authorization for `terminal` ssh-mode targets.
pub mod ssh_authorizer;
/// Live tracking of `subagent` command invocations.
pub mod subagent_spaces;
pub mod subagent_tracker;
pub(crate) mod task_tool;
pub mod tool_environment_cache;
pub(crate) mod tunnel_tool;
mod usage;
mod wall_clock;
pub mod work_tracker;

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
pub use permission::{
    OnTimeout, PermissionApprover, PermissionHistoryMode, PermissionHistoryModeParseError,
    PermissionRequest, PermissionRequestMode, PermissionRequestModeParseError,
};
pub(crate) use provider::reasoning_text_requested;
pub use provider::{
    ModelSlot, ProviderLlm, RedactedModel, ResolvedProviderRuntime, build_fast_provider_chain,
    provider_candidate_count, resolve_provider_runtime,
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
pub use usage::{
    CacheMissCauseRow, CacheMissRow, ModelUsageRow, SessionUsageReport, assemble_usage_report,
};
pub use wall_clock::{Interval, IntervalSet, WallClockSplit};
