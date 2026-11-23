//! May agent implementation.

pub mod acp_known;
pub mod acp_sandbox;
pub mod acp_subagent;
mod agents_md;
pub mod ask_user_tool;
pub mod browser_tool;
pub use may_protocol::cache_miss;
pub mod checkout_updates;
pub mod ci_alert;
pub mod ci_tool;
pub mod command_env;
#[cfg(test)]
mod compact_tool_tests;
mod compaction_report;
#[cfg(test)]
mod compaction_tests;
mod compaction_watch;
#[cfg(target_os = "macos")]
pub mod computer_tool;

mod environment;
pub mod explore_tool;
mod hooks;
#[cfg(test)]
mod host_load_tests;
#[cfg(any(test, feature = "testing"))]
pub mod idle_compaction_tests;
pub mod imagine;
pub mod issue_tool;
mod job_events;
pub mod keep_warm;
#[cfg(test)]
mod keep_warm_tests;
#[cfg(test)]
mod live_content_tests;
mod live_limits;

mod loop_watch;
mod may;
/// The `models` command and the listing it reads.
pub mod models_tool;
/// The text May's agents read for aither's alerts, notices and refusals.
pub mod notices;
#[cfg(test)]
mod open_questions_tests;
pub mod permission;
pub mod pr_tool;
#[cfg(test)]
mod prompt_layout_tests;
mod provider;
/// Approval of the issues, pull requests and comments the agent publishes.
pub mod publish_approval;
/// The `register` tool: registering files into the session.
pub mod register_tool;
/// The `repo` tool: repository trust and clones.
pub mod repo_tool;
#[cfg(test)]
mod request_parameters_tests;
#[cfg(test)]
mod rules_import_session_tests;
/// Runs an agent to completion.
pub mod runner;
mod runtime_context;
#[cfg(test)]
mod secret_redaction_tests;
pub mod serve_tool;
mod session_find;
mod session_model;
/// Session tool registration for the agent.
pub mod session_tool;
/// Authorization for the SSH servers `terminal` commands name.
pub mod ssh_authorizer;
pub mod subagent_spaces;
/// The `task` tool: the session's scheduled tasks.
pub mod task_tool;
/// The `tunnel` tool: exposing session serves publicly.
pub mod tunnel_tool;

/// The sleep source: detecting and reporting when the user sleeps and wakes.
pub mod wake;

pub mod webfetch_tool;
pub mod work_tracker;

pub use browser_tool::{
    BrowserArgs, BrowserBroker, BrowserRequest, BrowserResult, channel as browser_channel,
};
pub use compaction_report::CompactionReporter;
pub use compaction_watch::MayRun;
pub use environment::{ResolveAgentEnvironmentError, resolve_agent_environment_for_session};
pub use imagine::ImageGeneratorConfig;
pub use job_events::{
    BackgroundWake, DeliveredJobCompletion, JobFact, SessionJobHooks, render_job_ended,
};
pub use live_limits::LiveLimits;
pub use loop_watch::LoopLimits;
pub use may::{
    AgentEnvironment, MayAgent, MayAgentConfig, MayAgentError, MayAgentSessionInput,
    generate_session_title,
};
pub use may::{TurnSyncInput, side_slot};
pub use may_client::command_cards;
pub use may_client::subagent_tracker;
pub use may_client::subagent_tracker::{RunCard, RunUsage, SubagentTracker, TrackedRun};
pub use may_client::tool_environment_cache::{
    Acquired, HomeBackend, PreparationLease, ToolEnvironmentCache, ToolEnvironmentCacheError,
};
pub use may_config::agent_settings::*;
pub use may_protocol::context_dump::ContextDump;
pub use may_protocol::context_report::{ContextCategory, ContextCategoryRow, ContextReport};
pub use may_protocol::permission::*;
pub use may_protocol::permission::{
    PermissionHistoryModeParseError, PermissionRequestModeParseError,
};
pub use may_protocol::subagent_snapshot::SubagentRunSnapshot;
pub use permission::PermissionApprover;
pub use provider::{
    ModelSlot, ProviderLlm, Providers, RedactedError, RedactedModel, ResolvedProviderRuntime,
    build_fast_provider_chain, provider_candidate_count, resolve_provider_runtime,
};
pub use provider::{bounded, reasoning_text_requested};
pub use runner::{
    AgentHandle, AgentWorker, BackgroundWakes, ForeignTurn, RemoteTurn, RunLease, TuiEvent,
    spawn_idle_worker, spawn_worker,
};
pub use runtime_context::{SleepAlert, WakeAlert};
pub use session_model::{PushedModel, SessionModel};
pub use task_tool::{
    TaskBroker, TaskCreateRequest, TaskCreateResult, TaskRuntimeKind, channel as task_channel,
};

pub use may_protocol::session_usage::{ModelUsageRow, SessionUsageReport};
pub use may_protocol::wall_clock::{Interval, IntervalSet, WallClockSplit};

mod usage;
pub use may_protocol::cache_miss::{CacheMissCauseRow, CacheMissRow};
pub use usage::assemble_usage_report;
