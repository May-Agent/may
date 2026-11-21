//! The native tools of an agent on a shell: `terminal`, `monitor`, the three
//! job controls and, when the agent carries one, `compact`.
//!
//! Every agent registers them through [`ShellToolkit::register`], so a fork
//! presents the tool definitions of the request it starts from.

use aither_agent::{AgentBuilder, FileReferences, Hooks, context::HandoffSchema};
use aither_core::LanguageModel;
use aither_sandbox::{
    AnyShellWording, JobInputTool, JobKillTool, JobReadTool, MonitorSettings, MonitorTool,
    OutputPolicy, ShellToolSettings,
};
use aither_skills::SkillRegistry;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use super::{MayShellWording, ShellBudgets, monitor_settings, shell_tool_settings};
use crate::command_policy::FileReadEpochs;
use crate::harness::compact_tool::{CompactSignal, CompactTool};
use crate::ipc::SessionShell;

/// Session-relative workspace directories whose files tool outputs routinely
/// reference and compaction handoffs index in `Materials Already On Disk`.
const SESSION_ARTIFACT_PREFIXES: &[&str] = &["outputs/", "artifacts/", "files/", "pastes/"];

/// Extracts relative workspace file references (`outputs/`, `artifacts/`,
/// `files/`, `pastes/`) from shell and IPC command outputs so compaction
/// handoffs build a mechanical `Materials Already On Disk` index.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShellFileReferences;

impl FileReferences for ShellFileReferences {
    fn files(&self, output: &str) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut paths = Vec::new();
        for token in output.split_whitespace() {
            let trimmed = token
                .trim_start_matches(['[', '(', '<', '"', '\'', '`'])
                .trim_end_matches([']', ')', '>', '"', '\'', '`', ',', ';', ':', '.']);
            if !SESSION_ARTIFACT_PREFIXES
                .iter()
                .any(|prefix| trimmed.starts_with(prefix))
            {
                continue;
            }
            let candidate = Path::new(trimmed);
            if candidate.as_os_str().is_empty()
                || trimmed.ends_with('/')
                || candidate
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_)))
            {
                continue;
            }
            if seen.insert(trimmed.to_owned()) {
                paths.push(trimmed.to_owned());
            }
        }
        paths
    }
}

/// Readable roots for post-compaction restoration on `shell`.
///
/// Places `shell.working_dir()` first so relative paths (`outputs/`,
/// `artifacts/`, `tasks.md`) and spilled post-fork outputs resolve against the
/// agent's workspace, followed by every writable and readable grant currently
/// mounted into `shell`.
#[must_use]
pub fn shell_restore_roots(shell: &SessionShell) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    let working_dir = shell.working_dir().clone();
    seen.insert(working_dir.clone());
    roots.push(working_dir);
    let snapshot = shell.grants().snapshot();
    for path in snapshot.writable.iter().chain(&snapshot.readable) {
        if seen.insert(path.clone()) {
            roots.push(path.clone());
        }
    }
    roots
}

/// The identities and bounds the shell's tools are built with.
#[derive(Debug, Clone)]
pub struct ShellToolkit {
    tools: ShellToolSettings,
    monitor: MonitorSettings,
    output: OutputPolicy,
    skill_registry: Option<Arc<SkillRegistry>>,
    read_epochs: FileReadEpochs,
}

impl ShellToolkit {
    /// The toolkit describing the bounds `budgets` holds now.
    #[must_use]
    pub fn of(budgets: &ShellBudgets) -> Self {
        Self {
            tools: shell_tool_settings(budgets),
            monitor: monitor_settings(budgets),
            output: budgets.output(),
            skill_registry: None,
            read_epochs: FileReadEpochs::default(),
        }
    }

    /// Attaches `registry` so agents built through this toolkit can restore
    /// named skills after compaction.
    #[must_use]
    pub fn with_skill_registry(mut self, registry: Option<Arc<SkillRegistry>>) -> Self {
        self.skill_registry = registry;
        self
    }

    /// Attaches `read_epochs` so subagents built through this toolkit share
    /// the session's per-agent unchanged-read epoch table.
    #[must_use]
    pub fn with_read_epochs(mut self, read_epochs: FileReadEpochs) -> Self {
        self.read_epochs = read_epochs;
        self
    }

    /// Shared per-agent unchanged-read epoch table.
    #[must_use]
    pub const fn read_epochs(&self) -> &FileReadEpochs {
        &self.read_epochs
    }

    /// Registers the tools and post-compaction restoration wiring of `shell`
    /// on `builder`, in the order every agent registers them; `compact` when
    /// `compact` holds its signal.
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
        let mut builder = builder
            .restore_roots(shell_restore_roots(shell))
            .spill_dir(shell.output_store().dir().to_path_buf())
            .file_references(ShellFileReferences)
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
        if let Some(registry) = &self.skill_registry {
            builder = builder.skill_registry(registry.clone());
        }
        match compact {
            Some(signal) => builder.tool(CompactTool::new(signal)),
            None => builder,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_file_references_extracts_saved_outputs_and_artifacts_cleanly() {
        let output = "[Preview only. Full output (120 lines, 5400 bytes) saved to outputs/cmd-1.txt. Use head/tail/rg on that file to read the rest.]\nWrote `artifacts/slides.md`, (files/report.csv) and pastes/build.log; ignored outputs/../secret.txt and outputs/.";
        let extracted = ShellFileReferences.files(output);
        assert_eq!(
            extracted,
            [
                "outputs/cmd-1.txt",
                "artifacts/slides.md",
                "files/report.csv",
                "pastes/build.log",
            ]
        );
    }
}
