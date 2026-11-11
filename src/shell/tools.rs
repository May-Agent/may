//! The native tools of an agent on a shell: `terminal`, `monitor`, the three
//! job controls and, when the agent carries one, `compact`.
//!
//! Every agent registers them through [`ShellToolkit::register`], so a fork
//! presents the tool definitions of the request it starts from.

use aither_agent::{AgentBuilder, Hooks, context::HandoffSchema};
use aither_core::LanguageModel;
use aither_sandbox::{
    AnyShellWording, JobInputTool, JobKillTool, JobReadTool, MonitorSettings, MonitorTool,
    OutputPolicy, ShellToolSettings,
};
use std::sync::Arc;

use super::{MayShellWording, ShellBudgets, monitor_settings, shell_tool_settings};
use crate::harness::compact_tool::{CompactSignal, CompactTool};
use crate::ipc::SessionShell;

/// The identities and bounds the shell's tools are built with.
#[derive(Debug, Clone)]
pub struct ShellToolkit {
    tools: ShellToolSettings,
    monitor: MonitorSettings,
    output: OutputPolicy,
}

impl ShellToolkit {
    /// The toolkit describing the bounds `budgets` holds now.
    #[must_use]
    pub fn of(budgets: &ShellBudgets) -> Self {
        Self {
            tools: shell_tool_settings(budgets),
            monitor: monitor_settings(budgets),
            output: budgets.output(),
        }
    }

    /// Registers the tools of `shell` on `builder`, in the order every
    /// agent registers them; `compact` when `compact` holds its signal.
    pub fn register<M, H, C, S>(
        &self,
        builder: AgentBuilder<M, H, C, S>,
        shell: &SessionShell,
        compact: Option<Arc<CompactSignal>>,
    ) -> AgentBuilder<M, H, C, S>
    where
        M: LanguageModel + 'static,
        C: LanguageModel + 'static,
        H: Hooks,
        S: HandoffSchema,
    {
        let wording = AnyShellWording::new(MayShellWording);
        let jobs = shell.jobs();
        let builder = builder
            .tool(shell.tool())
            .tool(MonitorTool::new(shell.clone(), self.monitor.clone()))
            .tool(JobReadTool::new(
                self.tools.read.clone(),
                jobs.clone(),
                wording.clone(),
                self.output.clone(),
            ))
            .tool(JobInputTool::new(
                self.tools.input.clone(),
                jobs.clone(),
                wording.clone(),
            ))
            .tool(JobKillTool::new(self.tools.kill.clone(), jobs, wording));
        match compact {
            Some(signal) => builder.tool(CompactTool::new(signal)),
            None => builder,
        }
    }
}
