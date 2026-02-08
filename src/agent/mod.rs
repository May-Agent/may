//! May agent implementation.

mod may;
pub mod runner;

pub use may::{MayAgent, MayAgentError, generate_session_title};
pub use runner::{AgentHandle, AgentWorker, TuiEvent, spawn_worker};
