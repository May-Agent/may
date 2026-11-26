//! May's `debug` IPC command adapter binding `aither-dap` to the host [`DebugBroker`].

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

use aither_core::llm::{AgentIdentity, Tool, ToolContext};
use aither_dap::{DebugArgs, DebugToolError, DebugToolOutput};
use aither_sandbox::SandboxMode;

use crate::agent::pipe_tool::PriorJobs;
use crate::debug::broker::{DebugBroker, DebugCallContext, DebugOwner};
use crate::ipc::{Binding, SessionShell};
use crate::task_wake::SessionPrompts;

/// Terminal-bound `debug` IPC command backed by May's host [`DebugBroker`].
///
/// - Refuses invocations from `read_only` subagents (`explore`, `review`, `audit`, etc.).
/// - Scopes writable subagents to an isolated [`DebugOwner::Subagent`] slot tied
///   to that subagent run's own `Shell` teardown.
/// - Inherits `Caller.working_dir` and `Binding.launch.mode` so target processes
///   run inside the session's `Shell` sandbox without duplicate `Approver` prompts.
#[derive(Clone)]
pub struct MayDebugTool {
    broker: DebugBroker,
    owner: DebugOwner,
    shell: Option<SessionShell>,
    launch_mode: SandboxMode,
    session_prompts: Option<SessionPrompts>,
    prior_jobs: Option<PriorJobs>,
    read_only_refusal: Option<String>,
}

impl std::fmt::Debug for MayDebugTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MayDebugTool")
            .field("owner", &self.owner)
            .field("launch_mode", &self.launch_mode)
            .field("read_only_refusal", &self.read_only_refusal.is_some())
            .finish_non_exhaustive()
    }
}

/// Derives the [`DebugOwner`] slot for an agent identity.
///
/// Main-agent calls share [`DebugOwner::MainSession`]; subagent calls are scoped
/// strictly to [`DebugOwner::Subagent`] so they never collide with the main
/// session and tear down when the subagent run finishes.
#[must_use]
pub fn debug_owner_for_identity(
    session_id: String,
    identity: Option<&AgentIdentity>,
    fallback_suffix: impl FnOnce() -> String,
) -> DebugOwner {
    match identity {
        Some(AgentIdentity::Subagent(sub)) => {
            let name = sub.name();
            let subagent_key = sub.started_by().map_or_else(
                || format!("{name}-{}", fallback_suffix()),
                |origin| format!("{name}-{}", origin.call()),
            );
            DebugOwner::Subagent {
                session_id,
                subagent_key,
            }
        }
        Some(AgentIdentity::Main) | None => DebugOwner::MainSession(session_id),
    }
}

impl MayDebugTool {
    /// Builds a `debug` tool instance bound to a specific terminal [`Binding`].
    #[must_use]
    pub fn for_binding(
        broker: DebugBroker,
        session_id: String,
        binding: &Binding<'_>,
        read_only_subagents: &HashSet<String>,
        session_prompts: Option<SessionPrompts>,
        prior_jobs: Option<PriorJobs>,
    ) -> Self {
        let identity = binding
            .launch
            .call
            .as_ref()
            .map(aither_core::llm::CallIdentity::agent);
        let owner = debug_owner_for_identity(session_id, identity, || {
            binding
                .launch
                .launch
                .map_or_else(|| "0".to_owned(), |id| id.to_string())
        });
        let (read_only_refusal, prompts) = match identity {
            Some(AgentIdentity::Subagent(sub)) => {
                let name = sub.name();
                let refusal = read_only_subagents.contains(name).then(|| {
                    format!(
                        "`debug` is not available to read-only subagent `{name}`; run debugging from the main agent or a writable subagent"
                    )
                });
                (refusal, None)
            }
            Some(AgentIdentity::Main) | None => (None, session_prompts),
        };

        Self {
            broker,
            owner,
            shell: binding.shell.cloned(),
            launch_mode: binding.launch.mode,
            session_prompts: prompts,
            prior_jobs,
            read_only_refusal,
        }
    }
}

/// Transport-level call error (tool-level debug errors return inside [`DebugToolOutput`]).
#[derive(Debug, thiserror::Error)]
#[error("debug call failed: {0}")]
pub struct DebugCallError(String);

impl Tool for MayDebugTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("debug")
    }

    type Arguments = DebugArgs;
    type Res = DebugToolOutput;
    type Error = DebugCallError;

    async fn call(&self, args: Self::Arguments, cx: ToolContext) -> Result<Self::Res, Self::Error> {
        if let Some(refusal) = &self.read_only_refusal {
            return Ok(DebugToolOutput::from(Err(DebugToolError::request_failed(
                refusal.clone(),
            ))));
        }

        let ctx = DebugCallContext {
            owner: self.owner.clone(),
            shell: self.shell.clone(),
            working_dir: cx.working_dir().map(str::to_owned),
            launch_mode: self.launch_mode,
            session_prompts: self.session_prompts.clone(),
            prior_jobs: self.prior_jobs.clone(),
        };

        let result = self.broker.handle(ctx, args).await;
        Ok(DebugToolOutput::from(result))
    }
}

/// Collects the names of all read-only subagent definitions so [`MayDebugTool`]
/// refuses `debug` calls from read-only subagents (`explore`, `review`, `audit`, etc.).
#[must_use]
pub fn read_only_subagent_names(
    definitions: &std::collections::BTreeMap<
        String,
        crate::harness::subagent_file::SubagentDefinition,
    >,
) -> Arc<HashSet<String>> {
    let names = definitions
        .iter()
        .filter(|(_, def)| def.access == crate::harness::subagent_file::SubagentAccess::ReadOnly)
        .map(|(name, _)| name.clone())
        .collect();
    Arc::new(names)
}
