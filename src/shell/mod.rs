//! The shell May gives its agents: the limits, output budgets, wording and
//! tool identities `aither_sandbox::Shell` is assembled with, and the sandbox
//! it runs in.

mod approval;
mod assembly;
mod budgets;
mod environment;
mod settings;
mod tools;
mod wording;

pub use approval::ServerApproval;
pub use assembly::{ShellAssembly, ShellAssemblyError};
pub use budgets::ShellBudgets;
pub use environment::{HostSession, SessionEnvironmentError};
pub use settings::{
    INPUT_TOOL, KILL_TOOL, MONITOR_TOOL, READ_TOOL, SHELL_TOOL, monitor_settings,
    shell_tool_settings,
};
pub use tools::ShellToolkit;
pub use wording::MayShellWording;
