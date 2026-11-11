//! May agent implementation.

pub(crate) mod acp_known;
pub mod acp_sandbox;
pub mod acp_subagent;
mod agents_md;
pub(crate) mod ask_user_tool;
pub(crate) mod browser_tool;
pub mod cache_miss;
pub(crate) mod ci_alert;
pub(crate) mod ci_tool;
pub mod command_cards;
pub mod command_env;
#[cfg(test)]
mod compact_tool_tests;
mod compaction_report;
#[cfg(test)]
mod compaction_tests;
mod compaction_watch;
#[cfg(target_os = "macos")]
pub mod computer_tool;
mod context_dump;
mod context_report;
mod environment;
pub(crate) mod explore_tool;
/// The `find` command: regex search over session history.
pub mod find_tool;
mod hooks;
#[cfg(test)]
mod host_load_tests;
#[cfg(test)]
pub(crate) mod idle_compaction_tests;
pub(crate) mod imagine;
pub(crate) mod issue_tool;
mod job_events;
pub mod keep_warm;
#[cfg(test)]
mod keep_warm_tests;
#[cfg(test)]
mod live_content_tests;
mod live_limits;
mod live_settings;
mod loop_watch;
mod may;
/// The text May's agents read for aither's alerts, notices and refusals.
pub mod notices;
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
/// Authorization for the SSH servers `terminal` commands name.
pub mod ssh_authorizer;
/// Live tracking of `subagent` command invocations.
pub mod subagent_spaces;
pub mod subagent_tracker;
pub(crate) mod task_tool;
pub mod tool_environment_cache;
pub(crate) mod tunnel_tool;
mod usage;
mod wall_clock;
pub(crate) mod webfetch_tool;
pub mod work_tracker;

pub use browser_tool::{
    BrowserArgs, BrowserBroker, BrowserRequest, BrowserResult, channel as browser_channel,
};
pub use compaction_report::CompactionReporter;
pub use compaction_watch::MayRun;
pub use context_dump::ContextDump;
pub use context_report::{ContextCategory, ContextCategoryRow, ContextReport};
pub use environment::{ResolveAgentEnvironmentError, resolve_agent_environment_for_session};
pub use imagine::ImageGeneratorConfig;
pub use job_events::{BackgroundWake, JobFact};
pub use live_limits::LiveLimits;
pub use live_settings::AgentLiveSettings;
pub use loop_watch::LoopLimits;
pub(crate) use may::side_slot;
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
pub use subagent_tracker::{RunCard, RunUsage, SubagentRunSnapshot, SubagentTracker, TrackedRun};
pub use task_tool::{
    TaskBroker, TaskCreateRequest, TaskCreateResult, TaskRuntimeKind, channel as task_channel,
};
pub use tool_environment_cache::{HomeBackend, ToolEnvironmentCache, ToolEnvironmentCacheError};
pub use usage::{
    CacheMissCauseRow, CacheMissRow, ModelUsageRow, SessionUsageReport, assemble_usage_report,
};
pub use wall_clock::{Interval, IntervalSet, WallClockSplit};
