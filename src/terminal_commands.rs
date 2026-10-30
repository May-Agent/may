use std::path::PathBuf;
use std::sync::Arc;

use aither_agent::sandbox::builtin::AskCommand;
use aither_agent::specialized::{SubagentFileMount, SubagentTool};
use aither_agent::webfetch::WebFetchTool;
use aither_agent::websearch::WebSearchTool;
use aither_core::LanguageModel;
use aither_core::llm::{Tool, model::Parameters, tool::ToolDefinition};
use aither_sandbox::ToolRegistryBuilder;

use crate::agent::ImageGeneratorConfig;
use crate::agent::ask_user_tool::{AskUserTool, WithdrawAskUserTool};
use crate::agent::browser_tool::{BrowserBroker, BrowserTool};
use crate::agent::imagine::ImagineTool;
use crate::agent::issue_tool::{IssueTool, SubagentIssueTool};
use crate::agent::notify_tool::{NotifyBroker, NotifyTool};
use crate::agent::register_tool::{RegisterBroker, RegisterTool, Skills, Subagents};
use crate::agent::session_tool::SessionTool;
use crate::agent::subagent_tracker::{SubagentTracker, TrackedSubagentTool, UsageObservedModel};
use crate::agent::task_tool::{TaskBroker, TaskRuntimeKind, TaskTool};
use crate::embedding_provider::EmbeddingModelResolution;
use crate::mcp::load_active_mcp_connections;
use crate::repo_search::{CodeSearchTool, RepoSearch};
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
    /// Host directory the session works in; `search` defaults to its worktree.
    pub cwd: PathBuf,
    /// History database, which records the worktrees `search` is used in.
    pub db: Option<crate::db::ChatDb>,
    /// Runtime that holds the session's open `ask_user` questions, when the
    /// session has one.
    pub ask_user: Option<Arc<crate::session::SessionRuntimeService>>,
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
    /// Issue store for the `issue` command, resolved from the session
    /// workspace and its per-project configuration.
    pub issue_store: crate::issue::ResolvedIssueStore,
    /// Discovery and messaging between main agents, when the session has a
    /// persistent queue to deliver through.
    pub agent_messaging: Option<crate::agent_link::AgentMessaging>,
}

/// What [`register_shared_terminal_commands`] installed.
#[derive(Debug, Default)]
pub struct SharedTerminalCommands {
    /// `(name, short description)` pairs for every registered command, in
    /// registration order — rendered into the base-system `<commands>` index.
    pub descriptions: Vec<(String, String)>,
    /// Names of commands backed by MCP connections.
    pub mcp_command_names: Vec<String>,
    /// Where the main agent publishes its requests for the `subagent` command's
    /// `fork` kind; the main agent's builder must be given it.
    pub fork_source: aither_agent::ForkSource,
}

/// Register every shared IPC command on the terminal tool's command registry.
///
/// # Errors
/// Returns the registry error when a command fails to register.
pub async fn register_shared_terminal_commands<LLM>(
    builder: &mut ToolRegistryBuilder,
    config: SharedTerminalCommandConfig<LLM>,
) -> Result<SharedTerminalCommands, crate::mcp::McpConnectionLoadError>
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
        CodeSearchTool::new(
            RepoSearch::global(&config.may_dir),
            config.cwd.clone(),
            config.db.clone(),
        ),
    );
    describe_tool(
        &mut descriptions,
        &IssueTool::new(config.issue_store.clone()),
    );
    builder.configure_role_tools(
        IssueTool::new(config.issue_store.clone()),
        SubagentIssueTool::new(config.issue_store),
    );

    if let Some(runtime) = config.ask_user {
        let ask_user = AskUserTool::new(runtime.clone(), config.session_id.clone());
        describe_tool(&mut descriptions, &ask_user);
        builder.main_only(|reserved| reserved.configure_tool(ask_user));
        let withdraw = WithdrawAskUserTool::new(runtime, config.session_id.clone());
        describe_tool(&mut descriptions, &withdraw);
        builder.main_only(|reserved| reserved.configure_tool(withdraw));
    }

    if let Some(messaging) = config.agent_messaging {
        let agent = crate::agent_link::AgentTool::new(
            messaging,
            config.session_id.clone(),
            config.sandbox_dir.clone(),
            matches!(config.task_runtime, TaskRuntimeKind::Container)
                .then(|| crate::container::lifecycle::workspace_mount(config.sandbox_dir.clone())),
        );
        describe_tool(&mut descriptions, &agent);
        builder.main_only(|reserved| reserved.configure_tool(agent));
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

    let subagent_tool = SubagentTool::new(UsageObservedModel::new(config.llm))
        .with_runs(config.subagent_runs)
        .with_builtins()
        .with_base_dir(&config.sandbox_dir)
        .with_file_mounts([
            SubagentFileMount::new(PathBuf::from("."), config.sandbox_dir.clone()),
            SubagentFileMount::new(
                PathBuf::from("subagents"),
                config.sandbox_dir.join("subagents"),
            ),
            SubagentFileMount::new(PathBuf::from("skills"), config.sandbox_dir.join("skills")),
        ]);
    let subagent_tracker = config.subagent_tracker;
    let fork_source = subagent_tool.fork_source();
    describe_tool(&mut descriptions, &subagent_tool);
    // Built for each terminal that serves the registry, so a subagent launches
    // its own subagents with its own authority, never the main agent's.
    builder.configure_terminal_bound_tool("subagent", move |binding| {
        TrackedSubagentTool::new(
            subagent_tool.for_terminal(binding),
            subagent_tracker.clone(),
        )
    });

    let mut mcp_command_names = Vec::new();
    for connection in load_active_mcp_connections(&config.may_dir).await? {
        let definitions = aither_mcp::register_terminal_commands(builder, connection);
        descriptions.extend(definitions.into_iter().map(|definition| {
            mcp_command_names.push(definition.name().to_string());
            (
                definition.name().to_string(),
                short_description(definition.description()),
            )
        }));
    }

    Ok(SharedTerminalCommands {
        descriptions,
        mcp_command_names,
        fork_source,
    })
}

fn register_tool_with_description<T>(
    builder: &mut ToolRegistryBuilder,
    descriptions: &mut Vec<(String, String)>,
    tool: T,
) where
    T: Tool + Send + Sync + 'static,
    T::Arguments: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
{
    describe_tool(descriptions, &tool);
    builder.configure_tool(tool);
}

fn describe_tool<T>(descriptions: &mut Vec<(String, String)>, tool: &T)
where
    T: Tool,
    T::Arguments: schemars::JsonSchema + serde::de::DeserializeOwned,
{
    let definition = ToolDefinition::new(tool);
    descriptions.push((
        definition.name().to_string(),
        short_description(definition.description()),
    ));
}

pub(crate) fn short_description(description: &str) -> String {
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

        fn profile(
            &self,
        ) -> impl std::future::Future<Output = Result<ModelProfile, MockError>> + Send {
            std::future::ready(Ok(ModelProfile::new(
                "mock",
                "test",
                "mock-model",
                "mock model",
                100_000,
            )))
        }
    }

    async fn test_runtime(dir: &std::path::Path) -> Arc<crate::session::SessionRuntimeService> {
        let db = crate::db::ChatDb::open(&dir.join("runtime.db"))
            .await
            .expect("chat db should open");
        Arc::new(crate::session::SessionRuntimeService::new(
            db,
            crate::ui_event_bus::UiEventBus::new(),
        ))
    }

    async fn test_issue_store(dir: &std::path::Path) -> crate::issue::ResolvedIssueStore {
        let db = crate::db::ChatDb::open(&dir.join("history.db"))
            .await
            .expect("chat db should open");
        crate::issue::ResolvedIssueStore::May(crate::issue::MayIssueStore::new(
            db,
            "test".to_string(),
        ))
    }

    #[tokio::test]
    async fn chat_specific_tools_are_terminal_commands_when_brokers_are_present() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().join("sessions/session-1");
        let artifacts_dir = session_dir.join("artifacts");
        let ask_user_runtime = test_runtime(temp.path()).await;
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
                cwd: PathBuf::from("."),
                db: None,
                ask_user: Some(ask_user_runtime),
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
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
                issue_store: test_issue_store(temp.path()).await,
                agent_messaging: None,
            },
        )
        .await
        .expect("commands should register")
        .descriptions;

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
                cwd: PathBuf::from("."),
                db: None,
                ask_user: None,
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                register_subagents_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
                issue_store: test_issue_store(temp.path()).await,
                agent_messaging: None,
            },
        )
        .await
        .expect("commands should register")
        .descriptions;

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
                cwd: PathBuf::from("."),
                db: None,
                ask_user: None,
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                register_subagents_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                subagent_tracker: SubagentTracker::default(),
                request_parameters,
                issue_store: test_issue_store(temp.path()).await,
                agent_messaging: None,
            },
        )
        .await
        .expect("commands should register")
        .descriptions;

        let registry = builder.build(session_dir.join("outputs"));
        assert!(!registry.is_tool_configured("websearch"));
        assert!(!registry.is_tool_configured("webfetch"));
        assert!(!descriptions.iter().any(|(name, _)| name == "websearch"));
        assert!(!descriptions.iter().any(|(name, _)| name == "webfetch"));
    }

    #[tokio::test]
    async fn a_subagent_registry_reads_issues_and_refuses_ask_user_and_issue_writes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().join("sessions/session-1");
        let ask_user_runtime = test_runtime(temp.path()).await;
        let mut builder = ToolRegistryBuilder::new();
        register_shared_terminal_commands(
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
                cwd: PathBuf::from("."),
                db: None,
                ask_user: Some(ask_user_runtime.clone()),
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                register_subagents_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
                issue_store: test_issue_store(temp.path()).await,
                agent_messaging: None,
            },
        )
        .await
        .expect("commands should register");
        let main = builder.build(session_dir.join("outputs"));
        let subagent = main.for_subagent();

        // `subagent` is built per terminal, so a subagent launches with its own
        // authority; it is not reserved for the main agent.
        assert!(!main.is_reserved("subagent"));
        assert!(!subagent.is_reserved("subagent"));
        assert!(subagent.is_tool_configured("subagent"));

        // The main agent creates an issue; the subagent can read it.
        let created = main
            .query_tool_handler("issue", &["create".to_string(), "seed".to_string()])
            .await;
        assert!(created.ok, "{created:?}");
        let viewed = subagent
            .query_tool_handler("issue", &["view".to_string(), "1".to_string()])
            .await;
        assert!(viewed.ok, "{viewed:?}");
        let listed = subagent
            .query_tool_handler("issue", &["list".to_string()])
            .await;
        assert!(listed.ok, "{listed:?}");

        // Every write subcommand is refused from the subagent registry.
        for args in [
            vec!["create", "x"],
            vec!["edit", "1", "--title", "x"],
            vec!["comment", "1", "x"],
            vec!["close", "1"],
            vec!["label", "1", "--add", "bug"],
            vec!["blocked-by", "1", "--add", "2"],
            vec!["sub-issue", "1", "--add", "2"],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_string).collect();
            let refused = subagent.query_tool_handler("issue", &args).await;
            assert!(!refused.ok, "{args:?}: {refused:?}");
            assert!(
                refused
                    .error
                    .as_deref()
                    .is_some_and(|text| text.contains("reserved for the main agent")),
                "{args:?}: {refused:?}"
            );
        }
        let issues = main
            .query_tool_handler(
                "issue",
                &["list".to_string(), "--state".to_string(), "all".to_string()],
            )
            .await;
        assert!(issues.ok, "{issues:?}");

        // `ask_user` is refused to the subagent and stays available to the main agent.
        assert!(!main.is_reserved("ask_user"));
        assert!(subagent.is_reserved("ask_user"));
        let refused = subagent
            .query_tool_handler("ask_user", &["--question".to_string(), "q".to_string()])
            .await;
        assert!(!refused.ok, "{refused:?}");
        assert!(
            refused
                .error
                .as_deref()
                .is_some_and(|text| text.contains("reserved for the main agent")),
            "{refused:?}"
        );
        assert!(
            ask_user_runtime
                .db()
                .open_ask_user_questions("session-1")
                .await
                .expect("open questions load")
                .is_empty(),
            "the refused call must not reach the user"
        );
    }

    async fn agent_list(
        registry: &aither_sandbox::ToolRegistry,
    ) -> aither_sandbox::CommandEnvelope {
        registry
            .query_tool_handler("agent", &["list".to_string()])
            .await
    }

    async fn agent_send(
        registry: &aither_sandbox::ToolRegistry,
    ) -> aither_sandbox::CommandEnvelope {
        let args = ["send", "peer-agent", "note.txt"].map(str::to_string);
        registry.query_tool_handler("agent", &args).await
    }

    #[tokio::test]
    async fn agent_list_and_send_are_refused_to_a_subagent_and_work_for_the_main_agent() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().join("sb");
        std::fs::create_dir_all(&session_dir).expect("sandbox dir");
        std::fs::write(session_dir.join("note.txt"), "are you free?").expect("prompt file");
        let issue_store = test_issue_store(temp.path()).await;
        let db = crate::db::ChatDb::open(&temp.path().join("history.db"))
            .await
            .expect("chat db should open");
        for (id, title) in [("main", "main-agent"), ("peer", "peer-agent")] {
            db.create_session(id, Some(title), None)
                .await
                .expect("session is created");
        }
        let runtime_service = std::sync::Arc::new(crate::session::SessionRuntimeService::new(
            db,
            crate::ui_event_bus::UiEventBus::new(),
        ));
        let messaging = crate::agent_link::AgentMessaging {
            directory: crate::agent_link::AgentDirectory::new(temp.path().join("agents")),
            runtime_service: runtime_service.clone(),
        };

        // The peer is a second main agent listening on the same machine.
        let (woken_tx, woken) = async_channel::unbounded();
        let peer_listeners =
            crate::agent_link::AgentListeners::new(tokio::runtime::Handle::current());
        for (id, wake_tx) in [("main", None), ("peer", Some(woken_tx))] {
            peer_listeners
                .ensure(
                    &temp.path().join("agents"),
                    crate::agent_link::AgentIdentity {
                        session_id: id.to_string(),
                        cwd: temp.path().to_path_buf(),
                        sandbox_dir: temp.path().join(id),
                    },
                    runtime_service.clone(),
                    move || {
                        if let Some(wake_tx) = &wake_tx {
                            wake_tx.try_send(()).expect("test keeps the receiver");
                        }
                    },
                )
                .await
                .expect("agent listens");
        }

        let mut builder = ToolRegistryBuilder::new();
        register_shared_terminal_commands(
            &mut builder,
            SharedTerminalCommandConfig {
                llm: MockLlm,
                ask_flagship: None,
                ask_balanced: None,
                ask_fast: None,
                session_id: "main".to_string(),
                may_dir: temp.path().to_path_buf(),
                sandbox_dir: session_dir.clone(),
                search_embedder: EmbeddingModelResolution::Disabled,
                cwd: PathBuf::from("."),
                db: None,
                ask_user: None,
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                register_subagents_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
                issue_store,
                agent_messaging: Some(messaging),
            },
        )
        .await
        .expect("commands should register");
        let main = builder.build(session_dir.join("outputs"));
        let subagent = main.for_subagent();

        assert!(!main.is_reserved("agent"));
        assert!(subagent.is_reserved("agent"));
        for refused in [agent_list(&subagent).await, agent_send(&subagent).await] {
            assert!(!refused.ok, "{refused:?}");
            assert!(
                refused
                    .error
                    .as_deref()
                    .is_some_and(|text| text.contains("reserved for the main agent")),
                "{refused:?}"
            );
        }
        assert!(
            woken.is_empty(),
            "a refused send must not reach the recipient"
        );

        let listed = agent_list(&main).await;
        assert!(listed.ok, "{listed:?}");
        let Some(aither_sandbox::CommandPayload::Text { content: text }) = listed.payload else {
            panic!("agent list returns text: {listed:?}");
        };
        assert!(text.contains("main-agent (you)"), "{text}");
        assert!(text.contains("peer-agent"), "{text}");

        let sent = agent_send(&main).await;
        assert!(sent.ok, "{sent:?}");
        tokio::time::timeout(std::time::Duration::from_secs(5), woken.recv())
            .await
            .expect("waited 5s for the peer to be woken by the main agent's send")
            .expect("wake channel stays open");
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
                cwd: PathBuf::from("."),
                db: None,
                ask_user: None,
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                register_subagents_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
                issue_store: test_issue_store(temp.path()).await,
                agent_messaging: None,
            },
        )
        .await
        .expect("commands should register")
        .descriptions;

        let registry = builder.build(session_dir.join("outputs"));
        assert!(registry.is_tool_configured("websearch"));
        assert!(registry.is_tool_configured("webfetch"));
        assert!(descriptions.iter().any(|(name, _)| name == "websearch"));
        assert!(descriptions.iter().any(|(name, _)| name == "webfetch"));
    }
}
