//! The shell May gives its agents: the limits, output budgets, wording and
//! tool identities `aither_sandbox::Shell` is assembled with.

mod budgets;
mod settings;
mod wording;

pub use budgets::ShellBudgets;
pub use settings::{
    INPUT_TOOL, KILL_TOOL, MONITOR_TOOL, READ_TOOL, SHELL_TOOL, monitor_settings,
    shell_tool_settings,
};
pub use wording::MayShellWording;
