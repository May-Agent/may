//! May agent implementation.

mod may;
pub(crate) mod notify_tool;
pub(crate) mod register_tool;
pub mod runner;
pub mod session_tool;
pub mod ssh_authorizer;
pub(crate) mod task_tool;

pub use may::{
    AgentEnvironment, MayAgent, MayAgentConfig, MayAgentError, PermissionApprover,
    PermissionRequest, PermissionRequestMode, build_fast_provider_chain, generate_session_title,
    provider_candidate_count,
};
pub use runner::{AgentHandle, AgentWorker, TuiEvent, spawn_worker};
