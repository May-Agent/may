//! The `explore` IPC command: a shortcut for running the bundled `explore`
//! subagent.
//!
//! The subagent's definition fixes its tier (fast) and its access (read-only:
//! it writes only its own scratch directory); like every subagent it has no
//! authority over `tasks.md` or issues. This command only fills in the name,
//! so the agent passes the brief the way it does to `subagent`.

use std::borrow::Cow;

use aither_agent::specialized::SubagentArgs;
use aither_core::llm::tool::ToolContext;
use aither_core::llm::{Tool, ToolResult};
use aither_sandbox::CommandUsage;
use schemars::JsonSchema;
use serde::Deserialize;

/// The subagent this command runs.
const EXPLORE: &str = "explore";

/// Run the read-only `explore` subagent on the fast model: it reads across
/// the workspace in a context of its own and returns only its conclusion.
///
/// Write the brief to a file, then run `explore --prompt-file <path>`. The
/// result carries an id that `subagent resume` continues.
#[derive(Debug, Clone, JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExploreArgs {
    /// Path to a file holding the brief. The file must exist and be non-empty.
    pub prompt_file: String,
}

/// The `explore` command, delegating to a `subagent` tool.
pub struct ExploreTool<T> {
    subagent: T,
}

impl<T> ExploreTool<T> {
    /// Runs the `explore` subagent through `subagent`.
    pub const fn new(subagent: T) -> Self {
        Self { subagent }
    }
}

impl<T> Tool for ExploreTool<T>
where
    T: Tool<Arguments = SubagentArgs, Res = ToolResult> + Send + Sync + 'static,
{
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("explore")
    }

    type Arguments = ExploreArgs;
    type Res = ToolResult;

    async fn call(&self, args: Self::Arguments, cx: ToolContext) -> aither_core::Result<Self::Res> {
        let call = SubagentArgs {
            name: EXPLORE.to_string(),
            prompt_file: args.prompt_file,
            id: None,
            agent: None,
            worktrees: Vec::new(),
            commit_worktrees: Vec::new(),
        };
        self.subagent.call(call, cx).await
    }
}

impl<T> CommandUsage for ExploreTool<T>
where
    T: CommandUsage<Arguments = SubagentArgs, Res = ToolResult> + Send + Sync + 'static,
{
    fn usage(&self) -> impl Future<Output = String> + Send {
        self.subagent.usage()
    }
}
