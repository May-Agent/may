use std::path::PathBuf;

use aither_agent::sandbox::builtin::AskCommand;
use aither_agent::specialized::{SubagentFileMount, SubagentTool};
use aither_agent::webfetch::WebFetchTool;
use aither_agent::websearch::WebSearchTool;
use aither_core::LanguageModel;
use aither_core::llm::{Tool, model::Parameters, tool::ToolDefinition};
use aither_sandbox::{TerminalToolFactory, ToolRegistryBuilder};
use aither_search::{SearchEmbeddingResolution, SearchTool};

use crate::agent::ImageGeneratorConfig;
use crate::agent::browser_tool::{BrowserBroker, BrowserTool};
use crate::agent::imagine::ImagineTool;
use crate::agent::notify_tool::{NotifyBroker, NotifyTool};
use crate::agent::register_tool::{RegisterBroker, RegisterTool, Skills, Subagents};
use crate::agent::session_tool::SessionTool;
use crate::agent::subagent_tracker::{SubagentTracker, TrackedSubagentTool, UsageObservedModel};
use crate::agent::task_tool::{TaskBroker, TaskRuntimeKind, TaskTool};
use crate::ask_user::{AskUserBroker, AskUserTool};
use crate::embedding_provider::{EmbeddingModelResolution, MayEmbeddingModel};
use crate::mcp::load_active_mcp_connections;
use crate::workspace_request::{RequestWorkspaceTool, WorkspaceRequestBroker};

/// Configuration for the `imagine` (image generation) IPC command.
#[derive(Debug, Clone)]
pub struct ImagineCommandConfig {
    /// Generator settings; `None` disables the command.
    pub generator: Option<ImageGeneratorConfig>,
    /// Directory generated artifacts are written to.
    pub artifacts_dir: PathBuf,
}

/// Dependencies every shared terminal IPC command needs from the host runtime.
#[derive(Debug, Clone)]
pub struct SharedTerminalCommandConfig<LLM> {
    /// LLM handle used by commands that need a model.
    pub llm: LLM,
    /// Tier override for `ask --model flagship`; falls back to `llm`.
    pub ask_flagship: Option<LLM>,
    /// Tier override for `ask --model balanced`; falls back to `llm`.
    pub ask_balanced: Option<LLM>,
    /// Tier override for `ask --model fast` (the default tier); falls back to `llm`.
    pub ask_fast: Option<LLM>,
    /// Session the commands run on behalf of.
    pub session_id: String,
    /// `~/.may` data directory.
    pub may_dir: PathBuf,
    /// Session sandbox working directory.
    pub sandbox_dir: PathBuf,
    /// Resolved embedding model for semantic search.
    pub search_embedder: EmbeddingModelResolution,
    /// Broker for `ask_user` prompts, when interactive.
    pub ask_user_broker: Option<AskUserBroker>,
    /// Image-generation config, when enabled.
    pub imagine: Option<ImagineCommandConfig>,
    /// Task runtime backing the `task` command.
    pub task_runtime: TaskRuntimeKind,
    /// Broker for scheduled-task commands.
    pub task_broker: Option<TaskBroker>,
    /// Broker for `notify` notifications.
    pub notify_broker: Option<NotifyBroker>,
    /// Broker for `browser` commands.
    pub browser_broker: Option<BrowserBroker>,
    /// Broker for skill registration.
    pub register_skills_broker: Option<RegisterBroker>,
    /// Broker for subagent registration.
    pub register_subagents_broker: Option<RegisterBroker>,
    /// Broker for workspace-access requests.
    pub workspace_request_broker: Option<WorkspaceRequestBroker>,
    /// Factory producing per-call terminal tools.
    pub terminal_tool_factory: Option<TerminalToolFactory>,
    /// Registry the `subagent` command reports in-flight runs to;
    /// cloneable handle the host uses to list or cancel them.
    pub subagent_runs: aither_agent::specialized::SubagentRuns,
    /// Request parameters the agent will run with. Provider-hosted web tools
    /// (`Parameters::hosted_web_search` / `hosted_web_fetch`) suppress the
    /// redundant `websearch` / `webfetch` commands so the model has a single
    /// search path.
    pub request_parameters: Parameters,
    /// Tracker the `subagent` command reports each invocation to — the host
    /// UI reads live per-run stats off it.
    pub subagent_tracker: SubagentTracker,
}

/// Register every shared IPC command on the terminal tool's command registry.
///
/// # Errors
/// Returns the registry error when a command fails to register.
pub async fn register_shared_terminal_commands<LLM>(
    builder: &mut ToolRegistryBuilder,
    config: SharedTerminalCommandConfig<LLM>,
) -> Result<Vec<(String, String)>, crate::mcp::McpConnectionLoadError>
where
    LLM: LanguageModel + Clone + 'static,
{
    let mut descriptions = Vec::new();

    if !config.request_parameters.hosted_web_search() {
        register_tool_with_description(builder, &mut descriptions, WebSearchTool::default());
    }
    if !config.request_parameters.hosted_web_fetch() {
        register_tool_with_description(builder, &mut descriptions, WebFetchTool::new());
    }
    let ask_command = AskCommand::new(config.ask_fast.unwrap_or_else(|| config.llm.clone()))
        .with_flagship(config.ask_flagship.unwrap_or_else(|| config.llm.clone()))
        .with_balanced(config.ask_balanced.unwrap_or_else(|| config.llm.clone()));
    register_tool_with_description(builder, &mut descriptions, ask_command);
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

    if let Some(ask_user_broker) = config.ask_user_broker {
        register_tool_with_description(
            builder,
            &mut descriptions,
            AskUserTool::new(config.session_id.clone(), ask_user_broker),
        );
    }

    if let Some(imagine) = config.imagine {
        register_tool_with_description(
            builder,
            &mut descriptions,
            ImagineTool {
                generator: imagine.generator,
                artifacts_dir: imagine.artifacts_dir,
            },
        );
    }

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
            NotifyTool::new(Some(notify_broker)).for_session(config.session_id.clone()),
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
        let tool = SubagentTool::new(UsageObservedModel::new(config.llm))
            .with_runs(config.subagent_runs)
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
        let tool = if let Some(factory) = config.terminal_tool_factory {
            tool.with_terminal_tool_factory(factory)
        } else {
            tool
        };
        TrackedSubagentTool::new(tool, config.subagent_tracker)
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

#[cfg(test)]
mod tests {
    use std::fmt;

    use aither_core::LanguageModel;
    use aither_core::llm::{Event, LLMRequest, model::Profile as ModelProfile};
    use futures_lite::Stream;

    use super::*;
    use crate::ask_user;

    #[derive(Clone)]
    struct MockLlm;

    #[derive(Debug)]
    struct MockError;

    impl fmt::Display for MockError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("mock error")
        }
    }

    impl std::error::Error for MockError {}

    impl LanguageModel for MockLlm {
        type Error = MockError;

        fn respond(
            &self,
            _request: LLMRequest,
        ) -> impl Stream<Item = Result<Event, Self::Error>> + Send {
            futures_lite::stream::empty()
        }

        fn profile(&self) -> impl std::future::Future<Output = ModelProfile> + Send {
            std::future::ready(ModelProfile::new(
                "mock",
                "test",
                "mock-model",
                "mock model",
                100_000,
            ))
        }
    }

    #[tokio::test]
    async fn chat_specific_tools_are_terminal_commands_when_brokers_are_present() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().join("sessions/session-1");
        let artifacts_dir = session_dir.join("artifacts");
        let (ask_user_broker, _ask_user_queue) = ask_user::channel();
        let mut builder = ToolRegistryBuilder::new();

        let descriptions = register_shared_terminal_commands(
            &mut builder,
            SharedTerminalCommandConfig {
                llm: MockLlm,
                ask_flagship: None,
                ask_balanced: None,
                ask_fast: None,
                session_id: "session-1".to_string(),
                may_dir: temp.path().to_path_buf(),
                sandbox_dir: session_dir.clone(),
                search_embedder: EmbeddingModelResolution::Disabled,
                ask_user_broker: Some(ask_user_broker),
                imagine: Some(ImagineCommandConfig {
                    generator: None,
                    artifacts_dir,
                }),
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                register_subagents_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                terminal_tool_factory: None,
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
            },
        )
        .await
        .expect("commands should register");

        let registry = builder.build(session_dir.join("outputs"));
        assert!(registry.is_tool_configured("ask_user"));
        assert!(registry.is_tool_configured("imagine"));
        assert!(
            descriptions
                .iter()
                .any(|(name, _description)| name == "ask_user")
        );
        assert!(
            descriptions
                .iter()
                .any(|(name, _description)| name == "imagine")
        );
    }

    #[tokio::test]
    async fn chat_specific_tools_are_absent_without_live_chat_brokers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().join("sessions/session-1");
        let mut builder = ToolRegistryBuilder::new();

        let descriptions = register_shared_terminal_commands(
            &mut builder,
            SharedTerminalCommandConfig {
                llm: MockLlm,
                ask_flagship: None,
                ask_balanced: None,
                ask_fast: None,
                session_id: "session-1".to_string(),
                may_dir: temp.path().to_path_buf(),
                sandbox_dir: session_dir.clone(),
                search_embedder: EmbeddingModelResolution::Disabled,
                ask_user_broker: None,
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                register_subagents_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                terminal_tool_factory: None,
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
            },
        )
        .await
        .expect("commands should register");

        let registry = builder.build(session_dir.join("outputs"));
        assert!(!registry.is_tool_configured("ask_user"));
        assert!(!registry.is_tool_configured("imagine"));
        assert!(
            !descriptions
                .iter()
                .any(|(name, _description)| name == "ask_user")
        );
        assert!(
            !descriptions
                .iter()
                .any(|(name, _description)| name == "imagine")
        );
    }

    #[tokio::test]
    async fn web_commands_are_suppressed_when_hosted_tools_are_enabled() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().join("sessions/session-1");
        let mut builder = ToolRegistryBuilder::new();

        let mut request_parameters = Parameters::default();
        request_parameters.native_tools.claude.web_search = true;
        request_parameters.native_tools.claude.web_fetch = true;

        let descriptions = register_shared_terminal_commands(
            &mut builder,
            SharedTerminalCommandConfig {
                llm: MockLlm,
                ask_flagship: None,
                ask_balanced: None,
                ask_fast: None,
                session_id: "session-1".to_string(),
                may_dir: temp.path().to_path_buf(),
                sandbox_dir: session_dir.clone(),
                search_embedder: EmbeddingModelResolution::Disabled,
                ask_user_broker: None,
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                register_subagents_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                terminal_tool_factory: None,
                subagent_tracker: SubagentTracker::default(),
                request_parameters,
            },
        )
        .await
        .expect("commands should register");

        let registry = builder.build(session_dir.join("outputs"));
        assert!(!registry.is_tool_configured("websearch"));
        assert!(!registry.is_tool_configured("webfetch"));
        assert!(!descriptions.iter().any(|(name, _)| name == "websearch"));
        assert!(!descriptions.iter().any(|(name, _)| name == "webfetch"));
    }

    #[tokio::test]
    async fn web_commands_are_registered_without_hosted_tools() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().join("sessions/session-1");
        let mut builder = ToolRegistryBuilder::new();

        let descriptions = register_shared_terminal_commands(
            &mut builder,
            SharedTerminalCommandConfig {
                llm: MockLlm,
                ask_flagship: None,
                ask_balanced: None,
                ask_fast: None,
                session_id: "session-1".to_string(),
                may_dir: temp.path().to_path_buf(),
                sandbox_dir: session_dir.clone(),
                search_embedder: EmbeddingModelResolution::Disabled,
                ask_user_broker: None,
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                register_subagents_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                terminal_tool_factory: None,
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
            },
        )
        .await
        .expect("commands should register");

        let registry = builder.build(session_dir.join("outputs"));
        assert!(registry.is_tool_configured("websearch"));
        assert!(registry.is_tool_configured("webfetch"));
        assert!(descriptions.iter().any(|(name, _)| name == "websearch"));
        assert!(descriptions.iter().any(|(name, _)| name == "webfetch"));
    }
}
