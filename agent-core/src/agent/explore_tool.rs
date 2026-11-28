//! The `explore` IPC command: a shortcut for running the bundled `explore`
//! subagent.
//!
//! The subagent's definition fixes its tier (fast) and its access (read-only:
//! it writes only its own scratch directory); like every subagent it has no
//! authority over `tasks.md` or issues. This command only fills in the name,
//! so the agent passes the brief the way it does to `subagent`.

use std::borrow::Cow;

use aither_core::llm::tool::ToolContext;
use aither_core::llm::{Tool, ToolResult};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::ipc::CommandUsage;
use crate::subagent::SubagentArgs;

/// The subagent this command runs.
const EXPLORE: &str = "explore";

/// Run the read-only `explore` subagent on the fast model in its own context.
///
/// It investigates across the workspace and returns only its conclusion with
/// `file:line` references, keeping search noise out of your context. Pass the
/// brief inline (`explore "<brief>"` or `explore --prompt "<brief>"`) for
/// direct one-turn calls, or `--prompt-file <path>` for long multi-line
/// briefs. The result carries an id that `subagent resume` continues.
#[derive(Debug, Clone, JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExploreArgs {
    /// Inline brief describing what to investigate. Mutually exclusive with
    /// `--prompt-file`; use `--prompt-file` for long multi-line briefs.
    #[serde(default)]
    #[schemars(extend("x-positional" = true))]
    pub prompt: Option<String>,
    /// Path to a file holding the brief. Mutually exclusive with `prompt`.
    #[serde(default)]
    pub prompt_file: Option<String>,
}

/// The `explore` command, delegating to a `subagent` tool.
#[derive(Debug)]
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
    type Error = T::Error;

    async fn call(&self, args: Self::Arguments, cx: ToolContext) -> Result<Self::Res, Self::Error> {
        let call = SubagentArgs {
            name: EXPLORE.to_string(),
            prompt: args.prompt,
            prompt_file: args.prompt_file,
            id: None,
            reason: None,
            agent: None,
            model: None,
            tier: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::cli_to_json;

    #[test]
    fn explore_cli_accepts_positional_flag_and_file_briefs() {
        let schema = serde_json::to_value(schemars::schema_for!(ExploreArgs)).expect("schema");

        let positional: ExploreArgs = serde_json::from_value(
            cli_to_json(&schema, &["trace compaction".into()]).expect("positional"),
        )
        .expect("deserialize positional");
        assert_eq!(positional.prompt.as_deref(), Some("trace compaction"));
        assert_eq!(positional.prompt_file, None);

        let flagged: ExploreArgs = serde_json::from_value(
            cli_to_json(&schema, &["--prompt".into(), "trace compaction".into()]).expect("flag"),
        )
        .expect("deserialize flag");
        assert_eq!(flagged.prompt.as_deref(), Some("trace compaction"));
        assert_eq!(flagged.prompt_file, None);

        let from_file: ExploreArgs = serde_json::from_value(
            cli_to_json(&schema, &["--prompt-file".into(), "tmp/brief.md".into()]).expect("file"),
        )
        .expect("deserialize file");
        assert_eq!(from_file.prompt, None);
        assert_eq!(from_file.prompt_file.as_deref(), Some("tmp/brief.md"));
    }

    #[test]
    fn subagent_cli_accepts_positional_flag_and_file_briefs() {
        let schema = serde_json::to_value(schemars::schema_for!(SubagentArgs)).expect("schema");

        let inline: SubagentArgs = serde_json::from_value(
            cli_to_json(
                &schema,
                &[
                    "general".into(),
                    "fix clippy warnings".into(),
                    "--tier".into(),
                    "fast".into(),
                ],
            )
            .expect("inline subagent"),
        )
        .expect("deserialize inline");
        assert_eq!(inline.name, "general");
        assert_eq!(inline.prompt.as_deref(), Some("fix clippy warnings"));
        assert_eq!(inline.prompt_file, None);
        assert_eq!(inline.tier, Some(crate::subagent::Tier::Fast));

        let resume: SubagentArgs = serde_json::from_value(
            cli_to_json(
                &schema,
                &[
                    "resume".into(),
                    "--id".into(),
                    "amber".into(),
                    "also inspect tests".into(),
                ],
            )
            .expect("resume subagent"),
        )
        .expect("deserialize resume");
        assert_eq!(resume.name, "resume");
        assert_eq!(resume.id.as_deref(), Some("amber"));
        assert_eq!(resume.prompt.as_deref(), Some("also inspect tests"));

        let approve: SubagentArgs = serde_json::from_value(
            cli_to_json(&schema, &["approve".into(), "--id".into(), "req-1".into()])
                .expect("approve subagent"),
        )
        .expect("deserialize approve");
        assert_eq!(approve.name, "approve");
        assert_eq!(approve.id.as_deref(), Some("req-1"));
        assert_eq!(approve.prompt, None);
        assert_eq!(approve.prompt_file, None);
    }
}
