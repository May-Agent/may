use std::path::PathBuf;
use std::sync::Arc;

use aither_agent::sandbox::builtin::AskCommand;
use aither_agent::specialized::{SubagentFileMount, SubagentTool};
use aither_agent::webfetch::WebFetchTool;
use aither_core::LanguageModel;
use aither_core::llm::{Tool, model::Parameters, tool::ToolDefinition};
use aither_sandbox::ToolRegistryBuilder;
use askama::Template;
use executor_core::DefaultExecutor;

use crate::agent::ask_user_tool::{AskUserTool, WithdrawAskUserTool};
use crate::agent::browser_tool::{BrowserBroker, BrowserTool};
use crate::agent::imagine::ImagineTool;
use crate::agent::issue_tool::{IssueChangeNotifier, IssueTool, SubagentIssueTool};
use crate::agent::notify_tool::{NotifyBroker, NotifyTool};
use crate::agent::pr_tool::{PrArgs, PrCommand};
use crate::agent::register_tool::{RegisterBroker, RegisterTool, Skills};
use crate::agent::repo_tool::{RepoContext, RepoTool, SubagentRepoTool};
use crate::agent::session_tool::SessionTool;
use crate::agent::subagent_tracker::{SubagentTracker, TrackedSubagentTool, UsageObservedModel};
use crate::agent::task_tool::{TaskBroker, TaskRuntimeKind, TaskTool};
use crate::agent::{ImageGeneratorConfig, ModelSlot};
use crate::bundle::Bundle;
use crate::issue::provenance::IssueProvenance;
use crate::issue::search::IssueSearch;
use crate::mcp::load_active_mcp_connections;
use crate::repo_search::indexer::SearchIndexer;
use crate::repo_search::{CodeSearchTool, RepoSearch, RepoSearchCollections};
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
    pub ask_flagship: Option<ModelSlot<LLM>>,
    /// Tier override for `ask --model balanced`; falls back to `llm`.
    pub ask_balanced: Option<ModelSlot<LLM>>,
    /// Tier override for `ask --model fast` (the default tier); falls back to `llm`.
    pub ask_fast: Option<ModelSlot<LLM>>,
    /// Session the commands run on behalf of.
    pub session_id: String,
    /// `~/.may` data directory.
    pub may_dir: PathBuf,
    /// Session sandbox working directory.
    pub sandbox_dir: PathBuf,
    /// Host directory the session works in; `search` defaults to its worktree.
    pub cwd: PathBuf,
    /// History database, which records the worktrees `search` is used in.
    pub db: crate::db::ChatDb,
    /// Runtime that holds the session's open `ask_user` questions and
    /// publishes its UI events (`issue` writes among them), when the session
    /// has one.
    pub ask_user: Option<Arc<crate::session::SessionRuntimeService>>,
    /// Image-generation config, when enabled.
    pub imagine: Option<ImagineCommandConfig>,
    /// Task runtime backing the `task` command.
    pub task_runtime: TaskRuntimeKind,
    /// Broker for scheduled-task commands.
    pub task_broker: Option<TaskBroker>,
    /// Broker for `notify` notifications.
    pub notify_broker: Option<NotifyBroker>,
    /// Broker for `tunnel` registrations.
    pub tunnel_broker: Option<crate::agent::tunnel_tool::TunnelBroker>,
    /// Broker for `browser` commands.
    pub browser_broker: Option<BrowserBroker>,
    /// Broker for skill registration.
    pub register_skills_broker: Option<RegisterBroker>,
    /// Broker for workspace-access requests.
    pub workspace_request_broker: Option<WorkspaceRequestBroker>,
    /// Registry the `subagent` command reports in-flight runs to;
    /// cloneable handle the host uses to list or cancel them.
    pub subagent_runs: aither_agent::specialized::SubagentRuns,
    /// Request parameters the agent will run with. Provider-hosted URL fetch
    /// (`Parameters::hosted_web_fetch`) suppresses the redundant `webfetch`
    /// command. Search is always provider-hosted; there is no `websearch`
    /// command.
    pub request_parameters: Parameters,
    /// Tracker the `subagent` command reports each invocation to — the host
    /// UI reads live per-run stats off it.
    pub subagent_tracker: SubagentTracker,
    /// Issue store for the `issue` command, resolved from the session
    /// workspace and its per-project configuration.
    pub issue_store: crate::issue::ResolvedIssueStore,
    /// The main agent's `tasks.md` inbox: `issue`, `pr`, `ask_user` and
    /// `subagent` record the state changes its rows track on it.
    pub task_events: aither_agent::TaskEvents,
    /// Per-project workspace config — `pr` reads `[[projects]] checks` off it.
    pub workspace_config: crate::workspace::WorkspaceConfig,
    /// Discovery and messaging between main agents, when the session has a
    /// persistent queue to deliver through.
    pub agent_messaging: Option<crate::agent_link::AgentMessaging>,
    /// The session's language guard: `ask_user`, `issue`, `pr` and `agent`
    /// refuse text outside the languages it enforces.
    pub languages: Option<aither_agent::LanguageGuard>,
    /// Connected ACP agents, registered as subagent types; `None` outside
    /// the daemon.
    pub acp: Option<crate::agent::acp_subagent::AcpAgents>,
    /// The host git whose configuration is the user's: it names the identity
    /// and key commits in the sandbox are signed with, and runs every host
    /// git `pr` needs.
    pub git: crate::host_git::HostGit,
    /// Daemon handles for the `computer` command — present when the
    /// session may own a virtual desktop (macOS, local environment).
    #[cfg(target_os = "macos")]
    pub computer: Option<crate::agent::computer_tool::ComputerHandles>,
}

/// What [`register_shared_terminal_commands`] installed.
#[derive(Debug, Default)]
pub struct SharedTerminalCommands {
    /// `(name, short description)` pairs for every command registered by
    /// name, in registration order; the command table lists them ahead of the
    /// MCP connections' commands, which it reads from the connections.
    pub descriptions: Vec<(String, String)>,
    /// Where the main agent publishes its requests for the `subagent` command's
    /// `fork` kind; the main agent's builder must be given it.
    pub fork_source: aither_agent::ForkSource,
}

/// Why the shared terminal commands could not be registered.
#[derive(Debug, thiserror::Error)]
pub enum TerminalCommandsError {
    /// An MCP connection failed to load.
    #[error(transparent)]
    Mcp(#[from] crate::mcp::McpConnectionLoadError),
    /// A connected ACP agent cannot be a subagent type.
    #[error(transparent)]
    Acp(#[from] crate::config::ConfigError),
    /// The sandbox's git configuration could not be written, so its commits
    /// would go out unsigned.
    #[error(transparent)]
    Signing(#[from] crate::commit_signing::SigningError),
}

/// Register every shared IPC command on the terminal tool's command registry.
///
/// # Errors
/// Returns the registry error when a command fails to register.
///
/// # Panics
/// The registered sandboxed `gh` panics if its compiled-in refusal template
/// fails to render.
pub async fn register_shared_terminal_commands<LLM>(
    builder: &mut ToolRegistryBuilder,
    config: SharedTerminalCommandConfig<LLM>,
) -> Result<SharedTerminalCommands, TerminalCommandsError>
where
    LLM: LanguageModel + Clone + 'static,
{
    let mut descriptions = Vec::new();

    if !config.request_parameters.hosted_web_fetch() {
        register_tool_with_description(builder, &mut descriptions, WebFetchTool::new());
    }
    // The model of each tier; a tier nothing is configured for runs on the
    // session's model.
    let session_model = ModelSlot::new(config.llm.clone(), config.request_parameters.clone());
    let flagship = config.ask_flagship.unwrap_or_else(|| session_model.clone());
    let balanced = config.ask_balanced.unwrap_or_else(|| session_model.clone());
    let fast = config.ask_fast.unwrap_or(session_model);
    let fast_llm = fast.llm.clone();
    // The main agent is declared on the advanced tier (the terminal agent
    // builder has no setter for it) and a fork must resolve to the model the
    // main agent runs on, so the advanced slot of `subagent` is the session's
    // own model.
    let subagent_flagship = config.llm.clone();
    let screen_llm = fast_llm.clone();
    let ask_command = AskCommand::new(fast_llm.clone())
        .with_parameters(fast.parameters.clone())
        .with_flagship(flagship.llm, flagship.parameters)
        .with_balanced(balanced.llm.clone(), balanced.parameters.clone());
    register_tool_with_description(builder, &mut descriptions, ask_command);
    register_tool_with_description(
        builder,
        &mut descriptions,
        SessionTool::new(
            config.may_dir.clone(),
            config.session_id.clone(),
            Some(SearchIndexer::new(
                config.db.clone(),
                RepoSearchCollections::new(RepoSearch::global(&config.may_dir)),
            )),
        ),
    );
    register_tool_with_description(
        builder,
        &mut descriptions,
        CodeSearchTool::new(
            RepoSearch::global(&config.may_dir),
            config.cwd.clone(),
            Some(config.db.clone()),
            Some(crate::repo_clone::SessionRepos::new(
                &config.sandbox_dir,
                config.session_id.clone(),
                config.db.clone(),
            )),
            config.workspace_config.clone(),
        ),
    );
    let issue_search = IssueSearch::new(
        config.db.clone(),
        RepoSearchCollections::new(RepoSearch::global(&config.may_dir)),
    );
    let provenance = IssueProvenance::new(config.db.clone(), config.session_id.clone());
    let issue_tool = IssueTool::new(config.issue_store.clone(), provenance.clone())
        .with_search(issue_search.clone());
    let issue_targets = || {
        crate::issue::target::IssueTargets::new(
            config.db.clone(),
            config.cwd.clone(),
            crate::issue::github::GitHubRest::new(crate::issue::GhCliTokenSource),
        )
    };
    let mut issue_tool = issue_tool
        .with_targets(issue_targets())
        .with_task_events(config.task_events.clone());
    if let Some(guard) = &config.languages {
        issue_tool = issue_tool.with_languages(guard.clone());
    }
    if let Some(runtime) = config.ask_user.clone() {
        issue_tool = issue_tool.with_changes(IssueChangeNotifier::new(runtime));
    }
    let mut subagent_issue_tool = SubagentIssueTool::new(config.issue_store, provenance)
        .with_search(issue_search)
        .with_targets(issue_targets());
    if let Some(messaging) = &config.agent_messaging {
        let owners = crate::issue::owner::IssueOwners::new(messaging.directory.clone());
        issue_tool = issue_tool.with_owners(owners.clone());
        subagent_issue_tool = subagent_issue_tool.with_owners(owners);
    }
    describe_tool(&mut descriptions, &issue_tool);
    builder.configure_role_tools(issue_tool, subagent_issue_tool);
    // Issue and PR changes reach GitHub through `issue` and `pr` alone. The command exists
    // in sandboxed modes only, so an approved unsafe command resolves `gh` on
    // the host's PATH and runs the real program.
    builder.sandboxed_only(|sandboxed| {
        sandboxed.configure_raw_handler(
            "gh",
            "Refused in the sandbox; use `issue` or `pr`",
            |_call, _args| {
                Box::pin(async {
                    Err(GhRefused.render().expect("the gh refusal template renders"))
                })
            },
        );
    });

    // Commits made in the sandbox are signed by the harness on the host: the
    // sandbox's git configuration names this command as its signer. A
    // container's `HOME` is its mount of the sandbox directory, so the same
    // files serve it with the signer's in-container path. The command is
    // registered but never described: git calls it, the model never does.
    let signing = crate::commit_signing::CommitSigning::new(
        config.git.clone(),
        &config.may_dir,
        config.session_id.clone(),
    );
    let container_mount = matches!(config.task_runtime, TaskRuntimeKind::Container)
        .then(|| crate::container::lifecycle::workspace_mount(config.sandbox_dir.clone()));
    signing
        .install_sandbox_git(&config.sandbox_dir, container_mount.as_ref())
        .await?;
    builder.configure_tool(crate::commit_signing::SignCommitTool::new(signing.clone()));

    let screen: Arc<crate::pr::ScreenFn> = Arc::new(move |title: &str, body: &str| {
        let llm = screen_llm.clone();
        let (title, body) = (title.to_string(), body.to_string());
        Box::pin(async move { crate::pr::screen_with_llm(&llm, &title, &body).await })
    });
    let pr_context = crate::pr::PrContext {
        db: Some(config.db.clone()),
        session_id: config.session_id.clone(),
        session_root: config.sandbox_dir.clone(),
        scratch_root: crate::session::MayHomePaths::new(config.may_dir.clone()).repo_staging_dir(),
        cwd: config.cwd.clone(),
        workspace_config: config.workspace_config.clone(),
        screen: Some(screen),
        signing,
        github_api: crate::pr::PrContext::default_github_api(),
        task_events: Some(config.task_events.clone()),
    };
    let mut pr_command = PrCommand::new(pr_context);
    if let Some(guard) = &config.languages {
        pr_command = pr_command.with_languages(guard.clone());
    }
    describe_arguments::<PrArgs>(&mut descriptions, "pr");
    // Built per terminal: git inside a session clone and the project checks
    // run on that terminal's sandbox.
    builder.configure_terminal_bound_tool("pr", move |binding| pr_command.for_terminal(binding));

    let repo_context = RepoContext {
        cwd: config.cwd.clone(),
        session_root: config.sandbox_dir.clone(),
        workspace_config: config.workspace_config.clone(),
        db: config.db.clone(),
        session_id: config.session_id.clone(),
        scratch_root: crate::session::MayHomePaths::new(config.may_dir.clone()).repo_staging_dir(),
        host: config.git.clone(),
        host_paths: !matches!(config.task_runtime, TaskRuntimeKind::Container),
    };
    let repo_tool = RepoTool::new(repo_context.clone());
    let subagent_repo_tool = SubagentRepoTool::new(repo_context);
    describe_tool(&mut descriptions, &repo_tool);
    builder.configure_role_tools(repo_tool, subagent_repo_tool);

    if let Some(runtime) = config.ask_user {
        let mut ask_user = AskUserTool::new(runtime.clone(), config.session_id.clone())
            .with_task_events(config.task_events.clone());
        if let Some(guard) = &config.languages {
            ask_user = ask_user.with_languages(guard.clone());
        }
        describe_tool(&mut descriptions, &ask_user);
        // The user's answers are decisions the summary must not paraphrase.
        builder.kept_through_compaction(|kept| {
            kept.main_only(|reserved| reserved.configure_tool(ask_user));
        });
        let withdraw = WithdrawAskUserTool::new(runtime, config.session_id.clone())
            .with_task_events(config.task_events.clone());
        describe_tool(&mut descriptions, &withdraw);
        builder.main_only(|reserved| reserved.configure_tool(withdraw));
    }

    #[cfg(target_os = "macos")]
    if let Some(handles) = config.computer {
        register_tool_with_description(
            builder,
            &mut descriptions,
            crate::agent::computer_tool::ComputerTool::new(config.session_id.clone(), handles),
        );
    }

    if let Some(messaging) = config.agent_messaging {
        let mut agent = crate::agent_link::AgentTool::new(
            messaging,
            config.session_id.clone(),
            config.sandbox_dir.clone(),
            matches!(config.task_runtime, TaskRuntimeKind::Container)
                .then(|| crate::container::lifecycle::workspace_mount(config.sandbox_dir.clone())),
        );
        if let Some(guard) = &config.languages {
            agent = agent.with_languages(guard.clone());
        }
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
        // A task belongs to the session's main agent: the prompt its runs
        // inject wakes that agent, so a subagent cannot create one.
        let task = TaskTool::new(
            config.session_id.clone(),
            config.task_runtime,
            Some(task_broker),
        );
        describe_tool(&mut descriptions, &task);
        builder.main_only(|reserved| reserved.configure_tool(task));
    }

    if let Some(notify_broker) = config.notify_broker {
        register_tool_with_description(
            builder,
            &mut descriptions,
            NotifyTool::new(Some(notify_broker)).for_session(config.session_id.clone()),
        );
    }

    if let Some(tunnel_broker) = config.tunnel_broker {
        register_tool_with_description(
            builder,
            &mut descriptions,
            crate::agent::tunnel_tool::TunnelTool::new(
                config.session_id.clone(),
                Some(tunnel_broker),
            ),
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

    if let Some(workspace_request_broker) = config.workspace_request_broker {
        register_tool_with_description(
            builder,
            &mut descriptions,
            RequestWorkspaceTool::new(config.session_id.clone(), workspace_request_broker),
        );
    }

    // A subagent runs on the model of its definition's tier: the models `ask`
    // offers.
    let mut subagent_tool = SubagentTool::new(
        UsageObservedModel::new(subagent_flagship),
        Bundle::subagents_dir(&config.may_dir),
    )
    .with_request_parameters(config.request_parameters.clone())
    .with_balanced_model(UsageObservedModel::new(balanced.llm), balanced.parameters)
    .with_fast_model(UsageObservedModel::new(fast_llm), fast.parameters)
    .with_runs(config.subagent_runs)
    .with_task_events(config.task_events.clone())
    .with_base_dir(&config.sandbox_dir)
    .with_file_mounts([
        SubagentFileMount::new(PathBuf::from("."), config.sandbox_dir.clone()),
        SubagentFileMount::new(PathBuf::from("skills"), config.sandbox_dir.join("skills")),
    ]);
    if let Some(guard) = &config.languages {
        subagent_tool = subagent_tool.with_languages(guard.clone());
    }
    if let Some(acp) = &config.acp {
        subagent_tool = acp.register(subagent_tool)?;
    }
    let subagent_tracker = config.subagent_tracker;
    let fork_source = subagent_tool.fork_source();
    describe_tool(&mut descriptions, &subagent_tool);
    // Built for the main agent's terminal alone: a subagent has no `subagent`
    // command.
    builder.main_exclusive(|main| {
        main.configure_terminal_bound_usage_tool("subagent", move |binding| {
            TrackedSubagentTool::new(
                subagent_tool.for_terminal(binding),
                subagent_tracker.clone(),
            )
        });
    });

    // An MCP server's tools are a command set: the registry reads its tool
    // list at every lookup, so a tool the server adds mid-session runs at
    // once, and the agent's command table announces it.
    for connection in load_active_mcp_connections(&DefaultExecutor, &config.may_dir).await? {
        builder.command_set(connection);
    }

    Ok(SharedTerminalCommands {
        descriptions,
        fork_source,
    })
}

/// What the sandboxed `gh` command answers with.
#[derive(askama::Template)]
#[template(path = "gh_refused.txt", escape = "none")]
struct GhRefused;

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

/// Catalog entry for a terminal-bound command `name`, which has no instance
/// until a terminal binds it: its description is the rustdoc of its
/// arguments, as for any command.
fn describe_arguments<A: schemars::JsonSchema>(
    descriptions: &mut Vec<(String, String)>,
    name: &str,
) {
    let schema = schemars::schema_for!(A);
    let description = schema
        .get("description")
        .and_then(serde_json::Value::as_str)
        .expect("a command's arguments carry its description");
    descriptions.push((name.to_string(), short_description(description)));
}

/// The first sentence of `description` on one line, for the command catalog.
///
/// A sentence ends at a `.` followed by whitespace or the end of the text,
/// outside a code span, so `` `computer <action> ...` `` or a version
/// number does not cut it short.
pub(crate) fn short_description(description: &str) -> String {
    // Both markers are ASCII, so a byte scan finds them on char boundaries.
    let bytes = description.as_bytes();
    let mut in_code = false;
    let mut end = description.len();
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'`' => in_code = !in_code,
            b'.' if !in_code && bytes.get(index + 1).is_none_or(u8::is_ascii_whitespace) => {
                end = index;
                break;
            }
            _ => {}
        }
    }
    description[..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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

    impl aither_core::llm::Classify for MockError {
        fn failure(&self) -> aither_core::llm::Failure {
            aither_core::llm::Failure::permanent()
        }
    }

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

    async fn test_db(dir: &std::path::Path) -> crate::db::ChatDb {
        crate::db::ChatDb::open(&dir.join("history.db"))
            .await
            .expect("chat db should open")
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
                #[cfg(target_os = "macos")]
                computer: None,
                llm: MockLlm,
                ask_flagship: None,
                ask_balanced: None,
                ask_fast: None,
                session_id: "session-1".to_string(),
                may_dir: temp.path().to_path_buf(),
                sandbox_dir: session_dir.clone(),
                cwd: PathBuf::from("."),
                db: test_db(temp.path()).await,
                ask_user: Some(ask_user_runtime),
                imagine: Some(ImagineCommandConfig {
                    generator: None,
                    artifacts_dir,
                }),
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                tunnel_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
                issue_store: test_issue_store(temp.path()).await,
                task_events: aither_agent::TaskEvents::new(),
                workspace_config: crate::workspace::WorkspaceConfig::default(),
                agent_messaging: None,
                languages: None,
                acp: None,
                git: crate::host_git::HostGit::from_file("/dev/null"),
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
                #[cfg(target_os = "macos")]
                computer: None,
                llm: MockLlm,
                ask_flagship: None,
                ask_balanced: None,
                ask_fast: None,
                session_id: "session-1".to_string(),
                may_dir: temp.path().to_path_buf(),
                sandbox_dir: session_dir.clone(),
                cwd: PathBuf::from("."),
                db: test_db(temp.path()).await,
                ask_user: None,
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                tunnel_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
                issue_store: test_issue_store(temp.path()).await,
                task_events: aither_agent::TaskEvents::new(),
                workspace_config: crate::workspace::WorkspaceConfig::default(),
                agent_messaging: None,
                languages: None,
                acp: None,
                git: crate::host_git::HostGit::from_file("/dev/null"),
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
    async fn websearch_is_never_registered_as_an_ipc_command() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().join("sessions/session-1");
        let mut builder = ToolRegistryBuilder::new();

        let descriptions = register_shared_terminal_commands(
            &mut builder,
            SharedTerminalCommandConfig {
                #[cfg(target_os = "macos")]
                computer: None,
                llm: MockLlm,
                ask_flagship: None,
                ask_balanced: None,
                ask_fast: None,
                session_id: "session-1".to_string(),
                may_dir: temp.path().to_path_buf(),
                sandbox_dir: session_dir.clone(),
                cwd: PathBuf::from("."),
                db: test_db(temp.path()).await,
                ask_user: None,
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                tunnel_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
                issue_store: test_issue_store(temp.path()).await,
                task_events: aither_agent::TaskEvents::new(),
                workspace_config: crate::workspace::WorkspaceConfig::default(),
                agent_messaging: None,
                languages: None,
                acp: None,
                git: crate::host_git::HostGit::from_file("/dev/null"),
            },
        )
        .await
        .expect("commands should register")
        .descriptions;

        let registry = builder.build(session_dir.join("outputs"));
        assert!(!registry.is_tool_configured("websearch"));
        assert!(!descriptions.iter().any(|(name, _)| name == "websearch"));
        assert!(registry.is_tool_configured("webfetch"));
    }

    #[tokio::test]
    async fn webfetch_is_suppressed_when_hosted_fetch_is_enabled() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().join("sessions/session-1");
        let mut builder = ToolRegistryBuilder::new();

        let mut request_parameters = Parameters::default();
        request_parameters.native_tools.claude.web_fetch = true;

        let descriptions = register_shared_terminal_commands(
            &mut builder,
            SharedTerminalCommandConfig {
                #[cfg(target_os = "macos")]
                computer: None,
                llm: MockLlm,
                ask_flagship: None,
                ask_balanced: None,
                ask_fast: None,
                session_id: "session-1".to_string(),
                may_dir: temp.path().to_path_buf(),
                sandbox_dir: session_dir.clone(),
                cwd: PathBuf::from("."),
                db: test_db(temp.path()).await,
                ask_user: None,
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                tunnel_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                subagent_tracker: SubagentTracker::default(),
                request_parameters,
                issue_store: test_issue_store(temp.path()).await,
                task_events: aither_agent::TaskEvents::new(),
                workspace_config: crate::workspace::WorkspaceConfig::default(),
                agent_messaging: None,
                languages: None,
                acp: None,
                git: crate::host_git::HostGit::from_file("/dev/null"),
            },
        )
        .await
        .expect("commands should register")
        .descriptions;

        let registry = builder.build(session_dir.join("outputs"));
        assert!(!registry.is_tool_configured("webfetch"));
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
                #[cfg(target_os = "macos")]
                computer: None,
                llm: MockLlm,
                ask_flagship: None,
                ask_balanced: None,
                ask_fast: None,
                session_id: "session-1".to_string(),
                may_dir: temp.path().to_path_buf(),
                sandbox_dir: session_dir.clone(),
                cwd: PathBuf::from("."),
                db: test_db(temp.path()).await,
                ask_user: Some(ask_user_runtime.clone()),
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                tunnel_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
                issue_store: test_issue_store(temp.path()).await,
                task_events: aither_agent::TaskEvents::new(),
                workspace_config: crate::workspace::WorkspaceConfig::default(),
                agent_messaging: None,
                languages: None,
                acp: None,
                git: crate::host_git::HostGit::from_file("/dev/null"),
            },
        )
        .await
        .expect("commands should register");
        let main = builder.build(session_dir.join("outputs"));
        let subagent = main.for_subagent(None, None);

        // A subagent's terminal has no `subagent` command.
        assert!(main.is_tool_configured("subagent"));
        assert!(!subagent.is_tool_configured("subagent"));

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
        // The peer is a second session the daemon sees; the wake callback
        // stands in for the daemon dispatching its queued message.
        let (woken_tx, woken) = async_channel::unbounded::<String>();
        let messaging = crate::agent_link::AgentMessaging {
            directory: crate::agent_link::AgentDirectory::new(
                runtime_service.clone(),
                temp.path().to_path_buf(),
                move |session_id| {
                    woken_tx
                        .try_send(session_id.to_string())
                        .expect("test keeps the receiver");
                },
            ),
            runtime_service: runtime_service.clone(),
        };

        let mut builder = ToolRegistryBuilder::new();
        register_shared_terminal_commands(
            &mut builder,
            SharedTerminalCommandConfig {
                #[cfg(target_os = "macos")]
                computer: None,
                llm: MockLlm,
                ask_flagship: None,
                ask_balanced: None,
                ask_fast: None,
                session_id: "main".to_string(),
                may_dir: temp.path().to_path_buf(),
                sandbox_dir: session_dir.clone(),
                cwd: PathBuf::from("."),
                db: test_db(temp.path()).await,
                ask_user: None,
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                tunnel_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
                issue_store,
                task_events: aither_agent::TaskEvents::new(),
                workspace_config: crate::workspace::WorkspaceConfig::default(),
                agent_messaging: Some(messaging),
                languages: None,
                acp: None,
                git: crate::host_git::HostGit::from_file("/dev/null"),
            },
        )
        .await
        .expect("commands should register");
        let main = builder.build(session_dir.join("outputs"));
        let subagent = main.for_subagent(None, None);

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
    async fn task_is_a_main_agent_command_a_subagent_terminal_lacks() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().join("sessions/session-1");
        let (task_broker, _queue) = crate::agent::task_tool::channel();
        let mut builder = ToolRegistryBuilder::new();

        register_shared_terminal_commands(
            &mut builder,
            SharedTerminalCommandConfig {
                #[cfg(target_os = "macos")]
                computer: None,
                llm: MockLlm,
                ask_flagship: None,
                ask_balanced: None,
                ask_fast: None,
                session_id: "session-1".to_string(),
                may_dir: temp.path().to_path_buf(),
                sandbox_dir: session_dir.clone(),
                cwd: PathBuf::from("."),
                db: test_db(temp.path()).await,
                ask_user: None,
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: Some(task_broker),
                notify_broker: None,
                tunnel_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
                issue_store: test_issue_store(temp.path()).await,
                task_events: aither_agent::TaskEvents::new(),
                workspace_config: crate::workspace::WorkspaceConfig::default(),
                agent_messaging: None,
                languages: None,
                acp: None,
                git: crate::host_git::HostGit::from_file("/dev/null"),
            },
        )
        .await
        .expect("commands should register");
        let main = builder.build(session_dir.join("outputs"));
        let subagent = main.for_subagent(None, None);

        assert!(main.is_tool_configured("task"));
        assert!(!main.is_reserved("task"));
        assert!(subagent.is_reserved("task"));
    }

    #[tokio::test]
    async fn web_commands_are_registered_without_hosted_tools() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().join("sessions/session-1");
        let mut builder = ToolRegistryBuilder::new();

        let descriptions = register_shared_terminal_commands(
            &mut builder,
            SharedTerminalCommandConfig {
                #[cfg(target_os = "macos")]
                computer: None,
                llm: MockLlm,
                ask_flagship: None,
                ask_balanced: None,
                ask_fast: None,
                session_id: "session-1".to_string(),
                may_dir: temp.path().to_path_buf(),
                sandbox_dir: session_dir.clone(),
                cwd: PathBuf::from("."),
                db: test_db(temp.path()).await,
                ask_user: None,
                imagine: None,
                task_runtime: TaskRuntimeKind::Local,
                task_broker: None,
                notify_broker: None,
                tunnel_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
                issue_store: test_issue_store(temp.path()).await,
                task_events: aither_agent::TaskEvents::new(),
                workspace_config: crate::workspace::WorkspaceConfig::default(),
                agent_messaging: None,
                languages: None,
                acp: None,
                git: crate::host_git::HostGit::from_file("/dev/null"),
            },
        )
        .await
        .expect("commands should register")
        .descriptions;

        let registry = builder.build(session_dir.join("outputs"));
        assert!(!registry.is_tool_configured("websearch"));
        assert!(registry.is_tool_configured("webfetch"));
        assert!(!descriptions.iter().any(|(name, _)| name == "websearch"));
        assert!(descriptions.iter().any(|(name, _)| name == "webfetch"));
    }

    /// Registers the shared commands for a session rooted at `session_dir`
    /// on `runtime`, as the user `git` names.
    async fn register_for(
        temp: &std::path::Path,
        session_dir: &std::path::Path,
        runtime: TaskRuntimeKind,
        git: crate::host_git::HostGit,
    ) -> Result<(ToolRegistryBuilder, SharedTerminalCommands), TerminalCommandsError> {
        let mut builder = ToolRegistryBuilder::new();
        let commands = register_shared_terminal_commands(
            &mut builder,
            SharedTerminalCommandConfig {
                #[cfg(target_os = "macos")]
                computer: None,
                llm: MockLlm,
                ask_flagship: None,
                ask_balanced: None,
                ask_fast: None,
                session_id: "session-1".to_string(),
                may_dir: temp.to_path_buf(),
                sandbox_dir: session_dir.to_path_buf(),
                cwd: PathBuf::from("."),
                db: test_db(temp).await,
                ask_user: None,
                imagine: None,
                task_runtime: runtime,
                task_broker: None,
                notify_broker: None,
                tunnel_broker: None,
                browser_broker: None,
                register_skills_broker: None,
                workspace_request_broker: None,
                subagent_runs: aither_agent::specialized::SubagentRuns::default(),
                subagent_tracker: SubagentTracker::default(),
                request_parameters: Parameters::default(),
                issue_store: test_issue_store(temp).await,
                task_events: aither_agent::TaskEvents::new(),
                workspace_config: crate::workspace::WorkspaceConfig::default(),
                agent_messaging: None,
                languages: None,
                acp: None,
                git,
            },
        )
        .await?;
        Ok((builder, commands))
    }

    /// git calls `sign-commit`, the model never does: it runs in the
    /// sandbox but is not in the command table the model reads.
    #[tokio::test]
    async fn the_signer_command_is_registered_but_not_described_to_the_model() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().join("sessions/session-1");
        let user = crate::commit_signing::test_user::TestUser::new(&temp.path().join("user"));

        let (builder, commands) = register_for(
            temp.path(),
            &session_dir,
            TaskRuntimeKind::Local,
            user.host(),
        )
        .await
        .expect("commands should register");

        let registry = builder.build(session_dir.join("outputs"));
        assert!(registry.is_tool_configured(crate::commit_signing::SIGN_COMMAND));
        assert!(
            !commands
                .descriptions
                .iter()
                .any(|(name, _)| name == crate::commit_signing::SIGN_COMMAND)
        );
    }

    /// A container's `HOME` is its mount of the session directory, so it
    /// reads the same `.gitconfig`, which names the signer by the path the
    /// container sees.
    #[tokio::test]
    async fn a_container_session_names_the_signer_by_its_container_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().join("sessions/session-1");
        let user = crate::commit_signing::test_user::TestUser::new(&temp.path().join("user"));

        let (builder, _) = register_for(
            temp.path(),
            &session_dir,
            TaskRuntimeKind::Container,
            user.host(),
        )
        .await
        .expect("commands should register");

        assert!(
            builder
                .build(session_dir.join("outputs"))
                .is_tool_configured(crate::commit_signing::SIGN_COMMAND)
        );
        let program = std::process::Command::new("git")
            .args(["config", "--file"])
            .arg(session_dir.join(".gitconfig"))
            .args(["--get", "gpg.ssh.program"])
            .output()
            .expect("git runs");
        assert_eq!(
            String::from_utf8_lossy(&program.stdout).trim(),
            "/workspace/.config/may/ssh-sign"
        );
        // The script itself is on the host side of the mount.
        assert!(session_dir.join(".config/may/ssh-sign").is_file());
    }

    /// Without the sandbox's git configuration its commits would go out
    /// unsigned, so registration fails and the agent is not built.
    #[tokio::test]
    async fn a_sandbox_git_configuration_that_cannot_be_written_fails_registration() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().join("sessions/session-1");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        // `.config` is a file, so the signer has no directory to live in.
        std::fs::write(session_dir.join(".config"), "").expect("blocker");
        let user = crate::commit_signing::test_user::TestUser::new(&temp.path().join("user"));

        let Err(error) = register_for(
            temp.path(),
            &session_dir,
            TaskRuntimeKind::Local,
            user.host(),
        )
        .await
        else {
            panic!("registration succeeded");
        };

        assert!(
            matches!(error, TerminalCommandsError::Signing(_)),
            "{error}"
        );
    }

    #[test]
    fn a_catalog_entry_is_the_whole_first_sentence_on_one_line() {
        assert_eq!(
            short_description(
                "`computer <action> ...` — the session's virtual desktop.\n\nActions follow."
            ),
            "`computer <action> ...` — the session's virtual desktop"
        );
        assert_eq!(
            short_description("Track work as issues: create, view,\nlist and close. More."),
            "Track work as issues: create, view, list and close"
        );
        assert_eq!(
            short_description("Runs on aither 0.16.0 builds"),
            "Runs on aither 0.16.0 builds"
        );
    }
}
