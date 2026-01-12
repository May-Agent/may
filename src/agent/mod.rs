//! May agent implementation.

mod may;
pub mod runner;

pub use may::{generate_session_title, MayAgent, MayAgentError};
pub use runner::{AgentHandle, AgentWorker, TuiEvent, spawn_worker};
