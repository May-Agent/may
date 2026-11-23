//! Dynamic `board` IPC command exposed via [`CommandSet`] only while a session
//! belongs to an active swarm board.

use std::path::PathBuf;

use aither_core::llm::{CallIdentity, Tool, ToolContext, ToolResult, tool::ToolDefinition};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{SwarmError, SwarmService};
use crate::agent_link::{AgentLinkError, resolve_prompt_path_async};
use crate::db::SwarmReadMode;
use crate::harness::language::LanguageGuard;
use crate::ipc::CommandSet;

/// Read or post messages on this checkout's shared swarm coordination board.
///
/// `board` reads unread entries, `board post <file>` posts, and `board members` lists peers.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BoardArgs {
    /// Read new messages posted after this session's cursor (or from `--since` / `--all`).
    Read {
        /// Read messages starting at this sequence number (`>= 1`).
        #[serde(default)]
        since: Option<u64>,
        /// Read from sequence `1` instead of only unread messages.
        #[serde(default)]
        all: bool,
        /// Maximum number of messages to return in this page.
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Post the contents of a file inside your session sandbox to the swarm board.
    ///
    /// Include `@<name>` or `@<session-id>` in the file to wake specific peers.
    Post {
        /// Path inside your session sandbox holding the message text to post.
        #[schemars(extend("x-positional" = true))]
        prompt_file: String,
    },
    /// List the working sessions currently attached to this checkout's swarm board.
    Members,
}

impl Default for BoardArgs {
    fn default() -> Self {
        Self::Read {
            since: None,
            all: false,
            limit: None,
        }
    }
}

/// Internal `Tool` implementation providing the schema, `--help`, and execution
/// for `board`.
#[derive(Clone)]
pub struct BoardTool {
    swarm: SwarmService,
    session_id: String,
    sandbox_dir: PathBuf,
    workspace_mount: Option<aither_sandbox::MountRoot>,
    languages: Option<LanguageGuard>,
}

impl std::fmt::Debug for BoardTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoardTool")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl BoardTool {
    /// Builds the `board` tool for `session_id`.
    #[must_use]
    pub const fn new(
        swarm: SwarmService,
        session_id: String,
        sandbox_dir: PathBuf,
        workspace_mount: Option<aither_sandbox::MountRoot>,
    ) -> Self {
        Self {
            swarm,
            session_id,
            sandbox_dir,
            workspace_mount,
            languages: None,
        }
    }

    /// Refuses board posts outside the working language `guard` enforces.
    #[must_use]
    pub fn with_languages(mut self, guard: LanguageGuard) -> Self {
        self.languages = Some(guard);
        self
    }

    async fn execute(&self, args: BoardArgs) -> Result<ToolResult, String> {
        let output = match args {
            BoardArgs::Read { since, all, limit } => {
                let mode = if all {
                    SwarmReadMode::All
                } else if let Some(seq) = since {
                    SwarmReadMode::Since(seq)
                } else {
                    SwarmReadMode::Unread
                };
                self.swarm
                    .read_board(&self.session_id, mode, limit)
                    .await
                    .map_err(|e| e.to_string())?
            }
            BoardArgs::Post { prompt_file } => {
                let path = resolve_prompt_path_async(
                    &self.sandbox_dir,
                    self.workspace_mount.as_ref(),
                    &prompt_file,
                )
                .await
                .map_err(|e| e.to_string())?;
                let body = async_fs::read_to_string(&path).await.map_err(|source| {
                    AgentLinkError::PromptFile {
                        path: path.clone(),
                        source,
                    }
                    .to_string()
                })?;
                if body.trim().is_empty() {
                    return Err(SwarmError::EmptyMessage(path).to_string());
                }
                if let Some(guard) = &self.languages {
                    guard
                        .check_working(body.trim(), crate::harness::language::WorkingText::Internal)
                        .map_err(|refusal| {
                            crate::agent::notices::language_refused(refusal).to_string()
                        })?;
                }
                self.swarm
                    .post_message(&self.session_id, body.trim_end())
                    .await
                    .map_err(|e| e.to_string())?
            }
            BoardArgs::Members => self
                .swarm
                .list_members(&self.session_id)
                .await
                .map_err(|e| e.to_string())?,
        };
        Ok(ToolResult::text(output))
    }
}

impl Tool for BoardTool {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("board")
    }

    type Arguments = BoardArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> aither_core::Result<Self::Res> {
        self.execute(args).await.map_err(aither_core::Error::msg)
    }
}

/// Dynamic [`CommandSet`] exposing `board` while a session is in an active swarm.
///
/// Active requires `[experimental] swarm = true` and `> 1` working sessions in
/// the same checkout.
#[derive(Clone)]
pub struct BoardCommandSet {
    tool: BoardTool,
    definition: ToolDefinition,
}

impl std::fmt::Debug for BoardCommandSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoardCommandSet")
            .field("session_id", &self.tool.session_id)
            .finish_non_exhaustive()
    }
}

impl BoardCommandSet {
    /// Builds the dynamic command set for `tool`.
    #[must_use]
    pub fn new(tool: BoardTool) -> Self {
        let definition = ToolDefinition::new(&tool);
        Self { tool, definition }
    }
}

impl CommandSet for BoardCommandSet {
    fn definitions(&self) -> Vec<ToolDefinition> {
        if self
            .tool
            .swarm
            .active_board_for(&self.tool.session_id)
            .is_some()
        {
            vec![self.definition.clone()]
        } else {
            Vec::new()
        }
    }

    async fn call(
        &self,
        name: &str,
        _call: Option<CallIdentity>,
        arguments: Value,
    ) -> Result<ToolResult, String> {
        if name != "board" {
            return Err(format!("unknown swarm command '{name}'"));
        }
        let args: BoardArgs = if arguments.is_null()
            || arguments.as_object().is_some_and(serde_json::Map::is_empty)
        {
            BoardArgs::default()
        } else {
            serde_json::from_value(arguments)
                .map_err(|error| format!("invalid `board` arguments: {error}"))?
        };
        self.tool.execute(args).await
    }
}
