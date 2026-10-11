//! May agent implementation.

pub(crate) mod browser_tool;
mod environment;
mod hooks;
pub(crate) mod imagine;
mod may;
pub(crate) mod notify_tool;
mod permission;
mod provider;
pub(crate) mod register_tool;
pub mod runner;
mod runtime_context;
mod search_match;
pub mod session_tool;
pub mod ssh_authorizer;
pub(crate) mod task_tool;

pub use browser_tool::{
    BrowserArgs, BrowserBroker, BrowserRequest, BrowserResult, channel as browser_channel,
};
pub use environment::{ResolveAgentEnvironmentError, resolve_agent_environment_for_session};
pub use imagine::ImageGeneratorConfig;
pub use may::{AgentEnvironment, MayAgent, MayAgentConfig, MayAgentError, generate_session_title};
pub use notify_tool::{NotifyArgs, NotifyBroker, NotifyResult, channel as notify_channel};
pub use permission::{
    PermissionApprover, PermissionHistoryMode, PermissionHistoryModeParseError, PermissionRequest,
    PermissionRequestMode, PermissionRequestModeParseError,
};
pub use provider::{
    ResolvedProviderRuntime, build_fast_provider_chain, provider_candidate_count,
    resolve_provider_runtime,
};
pub use runner::{AgentHandle, AgentWorker, TuiEvent, spawn_worker};
pub use task_tool::{
    TaskBroker, TaskCreateRequest, TaskCreateResult, TaskRuntimeKind, channel as task_channel,
};
