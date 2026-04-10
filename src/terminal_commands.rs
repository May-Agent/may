use std::path::PathBuf;

use aither_agent::sandbox::builtin::AskCommand;
use aither_agent::specialized::{SubagentFileMount, SubagentTool};
use aither_agent::webfetch::WebFetchTool;
use aither_agent::websearch::WebSearchTool;
use aither_core::LanguageModel;
use aither_core::llm::{Tool, tool::ToolDefinition};
use aither_sandbox::{TerminalToolFactory, ToolRegistryBuilder};
use aither_search::{SearchEmbeddingResolution, SearchTool};

use crate::agent::browser_tool::{BrowserBroker, BrowserTool};
use crate::agent::notify_tool::{NotifyBroker, NotifyTool};
use crate::agent::register_tool::{RegisterBroker, RegisterTool, Skills, Subagents};
use crate::agent::session_tool::SessionTool;
use crate::agent::task_tool::{TaskBroker, TaskRuntimeKind, TaskTool};
use crate::embedding_provider::{EmbeddingModelResolution, MayEmbeddingModel};
use crate::mcp::load_active_mcp_connections;
use crate::workspace_request::{RequestWorkspaceTool, WorkspaceRequestBroker};

#[derive(Clone)]
pub struct SharedTerminalCommandConfig<LLM> {
    pub llm: LLM,
    pub session_id: String,
    pub may_dir: PathBuf,
    pub sandbox_dir: PathBuf,
    pub search_embedder: EmbeddingModelResolution,
    pub task_runtime: TaskRuntimeKind,
    pub task_broker: Option<TaskBroker>,
    pub notify_broker: Option<NotifyBroker>,
    pub browser_broker: Option<BrowserBroker>,
    pub register_skills_broker: Option<RegisterBroker>,
    pub register_subagents_broker: Option<RegisterBroker>,
    pub workspace_request_broker: Option<WorkspaceRequestBroker>,
    pub terminal_tool_factory: Option<TerminalToolFactory>,
}

pub async fn register_shared_terminal_commands<LLM>(
    builder: &mut ToolRegistryBuilder,
    config: SharedTerminalCommandConfig<LLM>,
) -> Result<Vec<(String, String)>, crate::mcp::McpConnectionLoadError>
where
    LLM: LanguageModel + Clone + 'static,
{
    let mut descriptions = Vec::new();

    register_tool_with_description(builder, &mut descriptions, WebSearchTool::default());
    register_tool_with_description(builder, &mut descriptions, WebFetchTool::new());
    register_tool_with_description(
        builder,
        &mut descriptions,
        AskCommand::new(config.llm.clone()),
    );
    register_tool_with_description(
        builder,
        &mut descriptions,
        SessionTool::new(
            config.may_dir.clone(),
            config.session_id.clone(),
            config.search_embedder.clone(),
        ),
    );
    register_tool_with_description(
        builder,
        &mut descriptions,
        SearchTool::new(
            config.sandbox_dir.clone(),
            to_search_embedding_resolution(config.search_embedder.clone()),
        ),
    );

    if let Some(task_broker) = config.task_broker {
        register_tool_with_description(
            builder,
            &mut descriptions,
            TaskTool::new(
                config.session_id.clone(),
                config.task_runtime,
                Some(task_broker),
            ),
        );
    }

    if let Some(notify_broker) = config.notify_broker {
        register_tool_with_description(
            builder,
            &mut descriptions,
            NotifyTool::new(Some(notify_broker)),
        );
    }

    if let Some(browser_broker) = config.browser_broker {
        register_tool_with_description(
            builder,
            &mut descriptions,
            BrowserTool::new(config.session_id.clone(), Some(browser_broker)),
        );
    }

    if let Some(register_skills_broker) = config.register_skills_broker {
        register_tool_with_description(
            builder,
            &mut descriptions,
            RegisterTool::<Skills>::new(config.session_id.clone(), Some(register_skills_broker)),
        );
    }

    if let Some(register_subagents_broker) = config.register_subagents_broker {
        register_tool_with_description(
            builder,
            &mut descriptions,
            RegisterTool::<Subagents>::new(
                config.session_id.clone(),
                Some(register_subagents_broker),
            ),
        );
    }

    if let Some(workspace_request_broker) = config.workspace_request_broker {
        register_tool_with_description(
            builder,
            &mut descriptions,
            RequestWorkspaceTool::new(config.session_id.clone(), workspace_request_broker),
        );
    }

    let subagent_tool = {
        let tool = SubagentTool::new(config.llm)
            .with_builtins()
            .with_base_dir(&config.sandbox_dir);
        let tool = tool.with_file_mounts([
            SubagentFileMount::new(PathBuf::from("."), config.sandbox_dir.clone()),
            SubagentFileMount::new(
                PathBuf::from("subagents"),
                config.sandbox_dir.join("subagents"),
            ),
            SubagentFileMount::new(PathBuf::from("skills"), config.sandbox_dir.join("skills")),
        ]);
        if let Some(factory) = config.terminal_tool_factory {
            tool.with_terminal_tool_factory(factory)
        } else {
            tool
        }
    };
    register_tool_with_description(builder, &mut descriptions, subagent_tool);

    for connection in load_active_mcp_connections(&config.may_dir).await? {
        let definitions = aither_mcp::register_terminal_commands(builder, connection);
        descriptions.extend(definitions.into_iter().map(|definition| {
            (
                definition.name().to_string(),
                short_description(definition.description()),
            )
        }));
    }

    Ok(descriptions)
}

fn to_search_embedding_resolution(
    resolution: EmbeddingModelResolution,
) -> SearchEmbeddingResolution<MayEmbeddingModel> {
    match resolution {
        EmbeddingModelResolution::Disabled => SearchEmbeddingResolution::Disabled,
        EmbeddingModelResolution::Failed(error) => {
            SearchEmbeddingResolution::Failed(error.to_string())
        }
        EmbeddingModelResolution::Ready(model) => SearchEmbeddingResolution::Ready(model),
    }
}

fn register_tool_with_description<T>(
    builder: &mut ToolRegistryBuilder,
    descriptions: &mut Vec<(String, String)>,
    tool: T,
) where
    T: Tool + Send + Sync + 'static,
    T::Arguments: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
{
    let definition = ToolDefinition::new(&tool);
    descriptions.push((
        definition.name().to_string(),
        short_description(definition.description()),
    ));
    builder.configure_tool(tool);
}

fn short_description(description: &str) -> String {
    description
        .split('.')
        .next()
        .unwrap_or(description)
        .trim()
        .to_string()
}
