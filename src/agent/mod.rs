//! May agent implementation.

pub(crate) mod browser_tool;
mod command_words;
mod environment;
mod may;
pub(crate) mod notify_tool;
pub(crate) mod register_tool;
pub mod runner;
mod search_match;
pub(crate) mod search_tool;
mod search_tool_tests;
pub mod session_tool;
pub mod ssh_authorizer;
pub(crate) mod task_tool;

pub use browser_tool::{
    BrowserArgs, BrowserBroker, BrowserRequest, BrowserResult, channel as browser_channel,
};
pub use environment::{ResolveAgentEnvironmentError, resolve_agent_environment_for_session};
pub use may::{
    AgentEnvironment, MayAgent, MayAgentConfig, MayAgentError, PermissionApprover,
    PermissionHistoryMode, PermissionRequest, PermissionRequestMode, ResolvedProviderRuntime,
    build_fast_provider_chain, generate_session_title, provider_candidate_count,
    resolve_provider_runtime,
};
pub use notify_tool::{NotifyArgs, NotifyBroker, NotifyResult, channel as notify_channel};
pub use runner::{AgentHandle, AgentWorker, TuiEvent, spawn_worker};
pub use task_tool::{
    TaskBroker, TaskCreateRequest, TaskCreateResult, TaskRuntimeKind, channel as task_channel,
};
