//! May's IPC command bridge: the registry of the commands a shell's scripts
//! call back into, and the [`aither_sandbox::IpcProvider`] that serves them.
//!
//! The registry is May's; aither's shell asks the provider once per process
//! it starts, with what that launch is, and the router it gets back carries
//! the commands that launch may call: the call identity's agent tells the main
//! agent from a subagent, and the mode tells a sandboxed script from an
//! unsafe one.

use std::sync::Arc;

use aither_sandbox::Shell;
use executor_core::AnyExecutor;

pub mod ask;
mod bridge;
mod cli;
mod command_set;
mod payload;
mod registry;

/// The shell type May builds: aither's shell on the daemon's executor.
pub type SessionShell = Shell<Arc<AnyExecutor>>;

pub use bridge::{CommandBridge, ShellSlot};
pub use cli::{POSITIONAL_KEYWORD, cli_to_json, schema_to_help, validate_positional_layout};
pub use command_set::{AnyCommandSet, CommandSet};
pub use payload::{CommandEnvelope, CommandPayload, tool_output_to_payload};
pub use registry::{
    AnyCommandObserver, Binding, Caller, CommandCall, CommandObserver, CommandRegistry,
    CommandRegistryBuilder, CommandUsage, KeptExchangeReceiver, Role,
};
