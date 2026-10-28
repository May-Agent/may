//! Stable-per-session runtime system prefix assembled by `MayAgent`.
//!
//! Everything in this module feeds into the KV-cache-stable portion of
//! the agent's prompt — workspace descriptions, environment hints,
//! browser status, terminal runtime rules, etc. [`RuntimeSystemContext`]
//! stores each slot as pre-rendered raw text and reconciles the slots
//! into an [`aither_agent::Context`] via `insert_system_text`, so the
//! final prompt is plain prose rather than serde-derived XML.
//!
//! Each slot is keyed by a short `snake_case` tag (see the [`tag`]
//! submodule) so swapping one slot does not invalidate the whole
//! cacheable prefix: e.g. a runtime workspace-grant approval changes
//! only the [`tag::WORKSPACE_WORKDIRS`] block while the rest of the
//! workspace prefix stays cached.

use aither_agent::SubagentDefinition;
use aither_core::llm::model::{Ability, Parameters};
use aither_core::llm::tool::ToolDefinition;
use askama::Template;

use super::may::{AgentEnvironment, MayAgentConfig, MayAgentError};
use crate::network_policy::{CompiledNetworkPolicy, NetworkPolicyMode};
use crate::session::{SessionPaths, SessionPlanState};
use crate::skill::SkillManager;
use crate::subagent::SubagentManager;
use crate::workspace::{Workspace as MayWorkspace, WorkspaceDir as MayWorkspaceDir};

const MEMORY_CONTEXT_HINT: &str = include_str!("../texts/memory_context_hint.txt");
const MACOS_HOST_AUTOMATION_HINT: &str = include_str!("../texts/macos_host_hint.txt");
const NATIVE_VOICE_AVAILABLE_HINT: &str = "Native voice input is available in this session. You may treat spoken input as first-class user messages.";
const PLAN_MODE_DRAFT_HINT: &str = include_str!("../texts/plan_mode_draft.txt");
const PLAN_MODE_APPROVED_HINT: &str = include_str!("../texts/plan_mode_approved.txt");

/// Tag name constants for the stable system blocks managed by
/// [`RuntimeSystemContext`]. Each tag is inserted with raw text content,
/// with no XML wrapping, so these strings end up as the `IndexMap` keys
/// in `aither_agent::Context::system_blocks` and as stable KV-cache
/// prefix segments.
pub mod tag {
    pub const MEMORY: &str = "memory";
    pub const TERMINAL_RUNTIME_CONTEXT: &str = "terminal_runtime_context";
    pub const WORKSPACE: &str = "workspace";
    pub const WORKSPACE_WORKDIRS: &str = "workspace_workdirs";
    pub const CONTAINER_ENVIRONMENT_CONTEXT: &str = "container_environment_context";
    pub const BROWSER: &str = "browser";
    pub const PROFILE_PERSONA: &str = "profile_persona";
    pub const PROFILE_MEMORIES: &str = "profile_memories";
    pub const PLAN_MODE: &str = "plan_mode";
    pub const NETWORK_RESTRICTIONS: &str = "network_restrictions";
    pub const LOCATION_CONTEXT: &str = "location_context";
    pub const NATIVE_VOICE_INPUT: &str = "native_voice_input";
    pub const PLATFORM: &str = "platform";
}

/// Stable per-session system prefix managed by [`MayAgent`].
///
/// Each slot holds a pre-rendered string that is reconciled into the
/// underlying [`aither_agent::Context`] as a raw-text persistent system
/// block (see `insert_system_text`). Keeping the slots as pre-rendered
/// strings means reconcile is a cheap set/unset operation with no
/// additional templating cost and no XML wrapping.
#[derive(Clone)]
pub struct RuntimeSystemContext {
    memory_db: crate::db::ChatDb,
    last_memory_content: String,
    terminal_runtime: Option<String>,
    workspace: Option<String>,
    workspace_workdirs: Option<String>,
    container_environment: Option<String>,
    browser: Option<String>,
    profile_persona: Option<String>,
    profile_memories: Option<String>,
    plan_mode: Option<String>,
    network_restrictions: Option<String>,
    location: Option<String>,
    native_voice_input: Option<String>,
    platform: Option<String>,
}

impl RuntimeSystemContext {
    pub(crate) const fn new(memory_db: crate::db::ChatDb, memory_content: String) -> Self {
        Self {
            memory_db,
            last_memory_content: memory_content,
            terminal_runtime: None,
            workspace: None,
            workspace_workdirs: None,
            container_environment: None,
            browser: None,
            profile_persona: None,
            profile_memories: None,
            plan_mode: None,
            network_restrictions: None,
            location: None,
            native_voice_input: None,
            platform: None,
        }
    }

    pub(crate) fn with_terminal_runtime(mut self, terminal_runtime: String) -> Self {
        self.terminal_runtime = Some(terminal_runtime);
        self
    }

    pub(crate) fn with_workspace(mut self, workspace: String) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub(crate) fn with_workspace_workdirs(mut self, workdirs: String) -> Self {
        self.workspace_workdirs = Some(workdirs);
        self
    }

    pub(crate) fn with_container_environment(mut self, container_environment: String) -> Self {
        self.container_environment = Some(container_environment);
        self
    }

    pub(crate) fn with_browser(mut self, browser: String) -> Self {
        self.browser = Some(browser);
        self
    }

    pub(crate) fn with_profile_persona(mut self, profile_persona: String) -> Self {
        self.profile_persona = Some(profile_persona);
        self
    }

    pub(crate) fn with_profile_memories(mut self, profile_memories: String) -> Self {
        self.profile_memories = Some(profile_memories);
        self
    }

    pub(crate) fn with_plan_mode(mut self, plan_mode: String) -> Self {
        self.plan_mode = Some(plan_mode);
        self
    }

    pub(crate) fn with_network_restrictions(mut self, network_restrictions: String) -> Self {
        self.network_restrictions = Some(network_restrictions);
        self
    }

    pub(crate) fn with_location(mut self, location: String) -> Self {
        self.location = Some(location);
        self
    }

    pub(crate) fn with_native_voice_input(mut self, native_voice_input: String) -> Self {
        self.native_voice_input = Some(native_voice_input);
        self
    }

    pub(crate) fn with_platform(mut self, platform: String) -> Self {
        self.platform = Some(platform);
        self
    }

    pub(crate) fn reconcile(&self, context: &mut aither_agent::Context) {
        reconcile_memory_block(
            context,
            build_memory_context_content(&self.last_memory_content),
        );
        reconcile_text_block(
            context,
            tag::TERMINAL_RUNTIME_CONTEXT,
            self.terminal_runtime.clone(),
        );
        reconcile_text_block(context, tag::WORKSPACE, self.workspace.clone());
        reconcile_text_block(
            context,
            tag::WORKSPACE_WORKDIRS,
            self.workspace_workdirs.clone(),
        );
        reconcile_text_block(
            context,
            tag::CONTAINER_ENVIRONMENT_CONTEXT,
            self.container_environment.clone(),
        );
        reconcile_text_block(context, tag::BROWSER, self.browser.clone());
        reconcile_text_block(context, tag::PROFILE_PERSONA, self.profile_persona.clone());
        reconcile_text_block(
            context,
            tag::PROFILE_MEMORIES,
            self.profile_memories.clone(),
        );
        reconcile_text_block(context, tag::PLAN_MODE, self.plan_mode.clone());
        reconcile_text_block(
            context,
            tag::NETWORK_RESTRICTIONS,
            self.network_restrictions.clone(),
        );
        reconcile_text_block(context, tag::LOCATION_CONTEXT, self.location.clone());
        reconcile_text_block(
            context,
            tag::NATIVE_VOICE_INPUT,
            self.native_voice_input.clone(),
        );
        reconcile_text_block(context, tag::PLATFORM, self.platform.clone());
    }

    pub(crate) async fn sync_memory_content(&mut self) {
        match self.memory_db.get_global_memory().await {
            Ok(Some(content)) => self.last_memory_content = content,
            Ok(None) => self.last_memory_content = String::new(),
            Err(error) => {
                tracing::error!(error = %error, "failed to read global memory from DB");
            }
        }
    }
}

fn reconcile_text_block(
    context: &mut aither_agent::Context,
    tag: &'static str,
    value: Option<String>,
) {
    match value {
        Some(text) => context.insert_system_text(tag, text),
        None => context.remove_system_named(tag),
    }
}

/// Reminder surfaced to the model when MEMORY.md changed mid-session.
#[derive(serde::Serialize)]
struct MemoryUpdate {
    #[serde(rename = "$text")]
    content: String,
}

/// Reconciles the MEMORY block without invalidating the KV-cache prefix.
///
/// Mid-session memory changes are staged into the context (applied on the
/// next reassembly, when the cache is invalidated anyway) and surfaced to
/// the model as an appended diff alert instead of a block rewrite.
fn reconcile_memory_block(context: &mut aither_agent::Context, desired: String) {
    if !context.has_system_block(tag::MEMORY) {
        context.insert_system_text(tag::MEMORY, desired);
        return;
    }
    if context.staged_system_update(tag::MEMORY) == Some(desired.as_str()) {
        return;
    }
    let current = context
        .system_blocks()
        .get(tag::MEMORY)
        .cloned()
        .unwrap_or_default();
    if context.stage_system_text(tag::MEMORY, desired.clone()) {
        let diff = similar::TextDiff::from_lines(&current, &desired)
            .unified_diff()
            .context_radius(2)
            .to_string();
        // Reminders are cleared on every request's populate pass, so a
        // reminder inserted here would be dropped before ever reaching the
        // model. A durable alert survives until reassembly folds the staged
        // memory block in.
        context.push_alert(&MemoryUpdate {
            content: format!(
                "MEMORY.md changed since this conversation started. The memory block above is the previous version; apply this diff mentally (the context will fold it in later):\n{diff}"
            ),
        });
    }
}

#[derive(Clone, Template)]
#[template(path = "browser_context.txt", escape = "none")]
struct BrowserTemplate {
    state: BrowserContextState,
    cdp_endpoint: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BrowserContextState {
    Enabled,
    Disabled,
}

impl BrowserContextState {
    const fn enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

#[derive(Clone, Template)]
#[template(path = "terminal_runtime_context.txt", escape = "none")]
struct TerminalRuntimeContextTemplate {
    model_id: String,
    home_environment_kind: String,
    operating_system: String,
    shell_dialect: &'static str,
    home_directory: String,
    session_directory: String,
    current_working_directory: String,
    home_layout: Vec<String>,
    session_layout: Vec<String>,
    command_rules: Vec<String>,
    environment_rules: Vec<String>,
    modality_support: Vec<String>,
    attached_remote_servers: Vec<crate::config::SshServerConfig>,
    secret_names: Vec<String>,
    secrets_file: String,
}

#[derive(Clone, Template)]
#[template(path = "workspace_context.txt", escape = "none")]
struct WorkspaceTemplate {
    operating_system: String,
    sandbox: String,
    cwd: String,
}

#[derive(Clone, Template)]
#[template(path = "workspace_workdirs.txt", escape = "none")]
struct WorkspaceWorkdirsTemplate {
    workdirs: Vec<WorkspaceDirEntry>,
}

#[derive(Clone)]
struct WorkspaceDirEntry {
    path: String,
    description: String,
}

impl From<&MayWorkspaceDir> for WorkspaceDirEntry {
    fn from(value: &MayWorkspaceDir) -> Self {
        Self {
            path: value.path.display().to_string(),
            description: value
                .description
                .clone()
                .unwrap_or_else(|| "User-approved directory".to_string()),
        }
    }
}

#[derive(Clone, Template)]
#[template(path = "container_environment_context.txt", escape = "none")]
struct ContainerEnvironmentContextTemplate {
    operating_system: String,
    cwd: String,
}

#[derive(Clone)]
struct BaseSystemCommand {
    name: String,
    description: String,
}

#[derive(Clone)]
struct BaseSystemNativeTool {
    name: String,
    description: String,
}

#[derive(Clone)]
struct BaseSystemSkill {
    dir_name: String,
    name: String,
    description: String,
}

#[derive(Clone)]
struct BaseSystemSubagent {
    id: String,
    description: String,
}

#[derive(Clone, Template)]
#[template(path = "base_system.md", escape = "none")]
struct BaseSystemTemplate {
    native_tools: Vec<BaseSystemNativeTool>,
    has_hosted_web_search: bool,
    has_hosted_web_fetch: bool,
    has_browser_command: bool,
    has_commands: bool,
    commands: Vec<BaseSystemCommand>,
    has_skills: bool,
    skills: Vec<BaseSystemSkill>,
    has_subagents: bool,
    subagents: Vec<BaseSystemSubagent>,
}

pub fn build_browser_context(enabled: bool, cdp_endpoint: Option<&str>) -> String {
    let state = if enabled {
        BrowserContextState::Enabled
    } else {
        BrowserContextState::Disabled
    };
    let template = BrowserTemplate {
        state,
        cdp_endpoint: cdp_endpoint.map(str::to_string),
    };
    template
        .render()
        .unwrap_or_else(|error| panic!("failed to render browser context: {error}"))
}

pub fn build_terminal_runtime_context(
    config: &MayAgentConfig,
    session_paths: &SessionPaths,
    model_info: Option<&aither_models::ModelEntry>,
    attached_remote_servers: Vec<crate::config::SshServerConfig>,
) -> String {
    let mut modality_support = Vec::new();
    if let Some(info) = model_info {
        modality_support.push(resolved_model_capability(info.id()));
        if info.has_ability(Ability::Vision) {
            modality_support.push("Image input is supported natively.".to_string());
        }
        if info.has_ability(Ability::Audio) {
            modality_support.push("Audio input is supported natively.".to_string());
        }
        if info.has_ability(Ability::AudioOutput) {
            modality_support.push("Audio output is supported natively.".to_string());
        }
        if info.has_ability(Ability::Video) {
            modality_support.push("Video input is supported natively.".to_string());
        }
        if info.has_ability(Ability::Pdf) {
            modality_support.push("PDF/document input is supported natively.".to_string());
        }
        if info.has_ability(Ability::ComputerUse) {
            modality_support.push(
                "Computer-use capable runtime interactions are supported by the model.".to_string(),
            );
        }
    }
    if modality_support.is_empty() {
        modality_support.push(
            "No additional multimodal abilities were resolved for this model; rely on persisted files and CLI tools when media cannot be viewed directly.".to_string(),
        );
    }

    let template = TerminalRuntimeContextTemplate {
        model_id: config.model_id.clone(),
        home_environment_kind: home_environment_kind_label(&config.environment).to_string(),
        operating_system: operating_system_hint(&config.environment).to_string(),
        shell_dialect: "POSIX sh",
        home_directory: session_paths.home_root().display().to_string(),
        session_directory: session_paths.root().display().to_string(),
        current_working_directory: session_paths.root().display().to_string(),
        home_layout: vec![
            "/sessions - per-session sandboxes under this home environment".to_string(),
            "/skills - profile/home-scoped skills, exposed read-only inside the session"
                .to_string(),
            "/subagents - profile/home-scoped subagents, exposed read-only inside the session"
                .to_string(),
            "workspace.toml - May-managed workspace grants for this home environment".to_string(),
        ],
        session_layout: vec![
            "/tmp - disposable temporary files".to_string(),
            "/artifacts - durable user-facing outputs; put deliverables here instead of tmp/"
                .to_string(),
            "/files - durable uploaded or recalled files".to_string(),
            "/outputs - durable overflow and full command outputs".to_string(),
            "/data - durable session-owned state".to_string(),
            "/skills - read-only profile skills mounted into this session".to_string(),
            "/subagents - read-only profile subagents mounted into this session".to_string(),
            "tasks.md - canonical task/plan document for long-running work".to_string(),
        ],
        command_rules: vec![
            "The native tools are `terminal`, which executes commands, and `terminal_kill`, `terminal_input`, and `terminal_read`, which manage its background jobs. Higher-level capabilities are terminal CLI commands exposed through the IPC bridge rather than extra native tools; rely on runtime tool descriptions and command `--help` output instead of assuming a static command list.".to_string(),
            "Issue at most one tool call per assistant turn. If you need multiple operations or parallel work, compose them inside one terminal command with shell control flow, pipelines, or background jobs instead of emitting multiple tool calls in the same response.".to_string(),
            "Read a selected skill completely before acting on it. The terminal call that reads the skill must finish before any task command that depends on those instructions.".to_string(),
            "IPC-backed commands are only valid when executed inside this profile's home environment. If you get a system-reminder because you tried to call a non-existent direct tool, immediately retry through `terminal` with the corresponding CLI command.".to_string(),
            "Commands run in the current session home environment by default unless you explicitly target another environment. Treat the home environment as the place where profile-scoped state, IPC commands, and durable session directories live.".to_string(),
            "Every terminal command call must set timeout. timeout=0 means immediate background execution. timeout>0 runs in foreground until timeout and then promotes to a background task if still running.".to_string(),
            "Inline command results are previews, not guaranteed byte-for-byte raw output. May may clean, fold, summarize, or structurally compress large outputs before putting them in context.".to_string(),
            "Treat context as memory and the filesystem as disk. After compaction, restart, or long runs, recover prior tool results from outputs/, data/, files/, artifacts/, or tasks.md instead of assuming the inline preview was complete.".to_string(),
            "For large saved outputs, use normal shell tools such as `head`, `tail`, `sed`, `rg`, `jq`, or the `ask` command against files to recall only the relevant slice.".to_string(),
            "Background tasks persist output to files. Use terminal_read for incremental reads, terminal_input for stdin when the process needs user input, and terminal_kill to stop them.".to_string(),
            "For image-producing commands, set expect=image and optionally resolution=auto|low|medium|high|native. If omitted, May picks a preview resolution from the source media.".to_string(),
            "For browser work, use the terminal `browser` command with this exact lifecycle: `browser open`, `browser read <url> --detail <brief|standard|full> --screenshot <off|viewport|full-page> --format json`, `browser cdp-endpoint` only when CDP is required, and `browser close` when finished so the live browser sidebar closes and resources are released.".to_string(),
            "When you create a user-facing file, write it under artifacts/ so it survives and can be surfaced cleanly to the user.".to_string(),
            "Use tasks.md as the canonical working document for multi-step work; it should stay accurate enough to resume after context loss.".to_string(),
        ],
        environment_rules: environment_runtime_rules(&config.environment),
        modality_support,
        attached_remote_servers,
        secret_names: config
            .secrets
            .names()
            .into_iter()
            .map(str::to_string)
            .collect(),
        secrets_file: crate::secrets::SECRETS_ENV_FILE.to_string(),
    };
    template
        .render()
        .unwrap_or_else(|error| panic!("failed to render terminal runtime context: {error}"))
}

fn resolved_model_capability(model_id: &str) -> String {
    let mut text = String::with_capacity(model_id.len() + 16);
    text.push_str("Resolved model: ");
    text.push_str(model_id);
    text
}

pub fn build_workspace_context(workspace: &MayWorkspace) -> String {
    let template = WorkspaceTemplate {
        operating_system: operating_system_hint(&AgentEnvironment::Local).to_string(),
        sandbox: workspace.sandbox.display().to_string(),
        cwd: workspace.cwd.display().to_string(),
    };
    template
        .render()
        .unwrap_or_else(|error| panic!("failed to render workspace context: {error}"))
}

/// Builds the optional `workspace_workdirs` block from currently approved
/// workspace grants. Split from [`build_workspace_context`] so that runtime
/// workdir approvals can invalidate only this block and leave the stable
/// workspace prefix cached.
pub fn build_workspace_workdirs_context(workspace: &MayWorkspace) -> Option<String> {
    let workdirs: Vec<WorkspaceDirEntry> = workspace
        .config
        .workdirs
        .iter()
        .map(WorkspaceDirEntry::from)
        .collect();
    if workdirs.is_empty() {
        return None;
    }
    let template = WorkspaceWorkdirsTemplate { workdirs };
    Some(
        template
            .render()
            .unwrap_or_else(|error| panic!("failed to render workspace workdirs context: {error}")),
    )
}

pub fn build_container_environment_context(cwd: &std::path::Path) -> String {
    let template = ContainerEnvironmentContextTemplate {
        operating_system: "linux (managed container runtime)".to_string(),
        cwd: cwd.display().to_string(),
    };
    template
        .render()
        .unwrap_or_else(|error| panic!("failed to render container environment context: {error}"))
}

pub async fn build_base_system_prompt(
    shared_terminal_commands: &[(String, String)],
    native_tool_definitions: &[ToolDefinition],
    request_parameters: &Parameters,
    skills_dir: &std::path::Path,
    subagents_dir: &std::path::Path,
) -> Result<String, MayAgentError> {
    let mut commands = shared_terminal_commands
        .iter()
        .map(|(name, description)| BaseSystemCommand {
            name: name.clone(),
            description: description.clone(),
        })
        .collect::<Vec<_>>();
    commands.sort_by(|left, right| left.name.cmp(&right.name));

    let mut native_tools = native_tool_definitions
        .iter()
        .map(|definition| BaseSystemNativeTool {
            name: definition.name().to_string(),
            description: crate::terminal_commands::short_description(definition.description()),
        })
        .collect::<Vec<_>>();
    let has_hosted_web_search = request_parameters.hosted_web_search();
    let has_hosted_web_fetch = request_parameters.hosted_web_fetch();
    if has_hosted_web_search {
        native_tools.push(BaseSystemNativeTool {
            name: "web_search".to_string(),
            description: "Provider-hosted web search".to_string(),
        });
    }
    if has_hosted_web_fetch {
        native_tools.push(BaseSystemNativeTool {
            name: "web_fetch".to_string(),
            description: "Provider-hosted URL fetch".to_string(),
        });
    }
    let has_browser_command = commands.iter().any(|command| command.name == "browser");

    let skills = SkillManager::from_skills_dir(skills_dir.to_path_buf())
        .list_skills()
        .await
        .map_err(|error| {
            MayAgentError::Sandbox(format!("failed to load skill prompt metadata: {error}"))
        })?
        .into_iter()
        .map(|skill| BaseSystemSkill {
            dir_name: skill.dir_name,
            name: skill.name,
            description: skill.description,
        })
        .collect::<Vec<_>>();

    let mut subagents = load_prompt_subagents(subagents_dir).await?;
    subagents.sort_by(|left, right| left.id.cmp(&right.id));

    let template = BaseSystemTemplate {
        native_tools,
        has_hosted_web_search,
        has_hosted_web_fetch,
        has_browser_command,
        has_commands: !commands.is_empty(),
        commands,
        has_skills: !skills.is_empty(),
        skills,
        has_subagents: !subagents.is_empty(),
        subagents,
    };
    template.render().map_err(|error| {
        MayAgentError::Sandbox(format!("failed to render base system prompt: {error}"))
    })
}

async fn load_prompt_subagents(
    subagents_dir: &std::path::Path,
) -> Result<Vec<BaseSystemSubagent>, MayAgentError> {
    let manager = SubagentManager::from_subagents_dir(subagents_dir.to_path_buf());
    let path = manager.subagents_dir();
    if async_fs::metadata(path).await.is_err() {
        return Ok(Vec::new());
    }

    let defs = SubagentDefinition::load_from_dir_async(path)
        .await
        .map_err(|error| {
            MayAgentError::Sandbox(format!("failed to load subagent prompt metadata: {error}"))
        })?;

    Ok(defs
        .into_iter()
        .map(|def| BaseSystemSubagent {
            id: def.id,
            description: def.description,
        })
        .collect())
}

const fn home_environment_kind_label(environment: &AgentEnvironment) -> &'static str {
    match environment {
        AgentEnvironment::Local => "local",
        AgentEnvironment::Container { .. } => "container",
    }
}

fn operating_system_hint(environment: &AgentEnvironment) -> &'static str {
    match environment {
        AgentEnvironment::Local => host_operating_system_label(),
        AgentEnvironment::Container { .. } => "linux (managed container runtime)",
    }
}

fn host_operating_system_label() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    }
}

fn environment_runtime_rules(environment: &AgentEnvironment) -> Vec<String> {
    match environment {
        AgentEnvironment::Local => vec![
            "This home environment runs on the user's local machine. Default to the session sandbox and approved workspace roots for writes.".to_string(),
            "Outside approved writable paths, treat the host as read-mostly and request approval before side effects.".to_string(),
            "Host automation must respect the local operating system and user presence.".to_string(),
        ],
        AgentEnvironment::Container { .. } => vec![
            "This home environment runs inside a managed container. Prefer direct terminal usage and assume the filesystem is writable unless a command says otherwise.".to_string(),
            "Container state outside the durable session directories may be ephemeral, so persist anything important into session files.".to_string(),
            "The container is isolated from the user's host machine, but you should still avoid needless destructive commands inside it.".to_string(),
        ],
    }
}

pub fn build_location_context(location: &crate::location::LocationInfo) -> String {
    format!(
        "Location hint (inferred):\nCity: {}\nRegion: {}\nCountry: {}\nTimezone: {}",
        location.city, location.region, location.country, location.timezone,
    )
}

pub fn build_memory_context_content(memory: &str) -> String {
    let mut out = String::with_capacity(MEMORY_CONTEXT_HINT.len() + memory.len() + 2);
    out.push_str(MEMORY_CONTEXT_HINT.trim());
    out.push_str("\n\n");
    out.push_str(memory);
    out
}

pub fn build_native_voice_context(native_voice_input: bool) -> Option<String> {
    native_voice_input.then(|| NATIVE_VOICE_AVAILABLE_HINT.to_string())
}

pub fn build_plan_mode_context(plan_state: SessionPlanState) -> Option<String> {
    match plan_state {
        SessionPlanState::Disabled => None,
        SessionPlanState::Draft => Some(PLAN_MODE_DRAFT_HINT.to_string()),
        SessionPlanState::Approved => Some(PLAN_MODE_APPROVED_HINT.to_string()),
    }
}

pub fn build_network_restrictions_context(
    policy: Option<&CompiledNetworkPolicy>,
) -> Option<String> {
    let policy = policy?;
    if matches!(policy.mode(), NetworkPolicyMode::Disabled) {
        return None;
    }
    let mut out = format!(
        "Network restrictions active for this session.\nMode: {}\n",
        policy.mode().as_str(),
    );
    if !policy.hosts().is_empty() {
        out.push_str("Hosts:\n");
        for host in policy.hosts() {
            out.push_str("- ");
            out.push_str(host);
            out.push('\n');
        }
    }
    Some(out)
}

pub fn build_macos_host_automation_context() -> Option<String> {
    if cfg!(target_os = "macos") && !is_running_in_container() {
        return Some(MACOS_HOST_AUTOMATION_HINT.to_string());
    }
    None
}

fn is_running_in_container() -> bool {
    detect_container_signal().is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerSignalKind {
    EnvVar,
    DockerEnvFile,
    ContainerEnvFile,
    OverlayRootFs,
}

fn detect_container_signal() -> Option<ContainerSignalKind> {
    if std::env::var_os("container").is_some() {
        return Some(ContainerSignalKind::EnvVar);
    }

    if std::path::Path::new("/.dockerenv").exists() {
        return Some(ContainerSignalKind::DockerEnvFile);
    }

    if std::path::Path::new("/run/.containerenv").exists() {
        return Some(ContainerSignalKind::ContainerEnvFile);
    }

    proc_root_fs_type()
        .filter(|fs_type| matches!(fs_type.as_str(), "overlay" | "aufs"))
        .map(|_| ContainerSignalKind::OverlayRootFs)
}

fn proc_root_fs_type() -> Option<String> {
    let mountinfo = std::fs::read_to_string("/proc/1/mountinfo").ok()?;
    for line in mountinfo.lines() {
        let (left, right) = line.split_once(" - ")?;
        let mut left_fields = left.split_whitespace();
        let mount_point = left_fields.nth(4)?;
        if mount_point != "/" {
            continue;
        }
        let fs_type = right.split_whitespace().next()?;
        return Some(fs_type.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use aither_core::llm::model::Parameters;
    use aither_core::llm::tool::ToolDefinition;

    use super::{
        RuntimeSystemContext, build_base_system_prompt, build_memory_context_content, tag,
    };

    fn native_tool_definitions() -> Vec<ToolDefinition> {
        [
            (
                "terminal",
                "Execute terminal commands in a sandboxed environment.",
            ),
            (
                "terminal_kill",
                "Terminate a background terminal task by its task id.",
            ),
            (
                "terminal_input",
                "Send input to a background terminal task that is waiting on stdin.",
            ),
            (
                "terminal_read",
                "Read incremental output from a background terminal task.",
            ),
        ]
        .into_iter()
        .map(|(name, description)| {
            ToolDefinition::from_parts(
                name.into(),
                description.into(),
                serde_json::json!({"type": "object", "properties": {}}),
            )
            .expect("tool definition schema should be valid")
        })
        .collect()
    }

    fn test_db() -> crate::db::ChatDb {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("test.db");
        // Leak the tempdir so the DB file persists for the test duration
        let dir = Box::leak(Box::new(dir));
        let _ = dir;
        tokio_test::block_on(crate::db::ChatDb::open(&db_path)).expect("test db should open")
    }

    /// Durable system alerts pushed with `Context::push_alert`.
    ///
    /// Alerts enter the conversation stream as `system` messages rather than
    /// as ephemeral reminders, which are dropped by every request's reminder
    /// repopulation pass.
    fn system_alerts(context: &aither_agent::Context) -> Vec<String> {
        context
            .conversation_messages()
            .into_iter()
            .filter(|message| message.role() == aither_core::llm::Role::System)
            .map(|message| message.content().to_string())
            .collect()
    }

    #[test]
    fn runtime_system_context_reinstates_memory_and_prunes_absent_blocks() {
        let db = test_db();
        let runtime_context = RuntimeSystemContext::new(db, "remember this".to_string());
        let mut context = aither_agent::Context::default();
        context.insert_system_text(tag::PROFILE_PERSONA, "stale persona");
        context.insert_system_text(tag::WORKSPACE, "stale workspace");

        runtime_context.reconcile(&mut context);

        assert!(context.has_system_block(tag::MEMORY));
        let memory = context
            .system_blocks()
            .get(tag::MEMORY)
            .expect("memory block should be present");
        assert_eq!(memory, &build_memory_context_content("remember this"));
        assert!(!context.has_system_block(tag::PROFILE_PERSONA));
        assert!(!context.has_system_block(tag::WORKSPACE));
    }

    #[test]
    fn runtime_system_context_stages_memory_updates_with_diff_alert() {
        let db = test_db();
        tokio_test::block_on(db.set_global_memory("before")).expect("write initial memory");

        let mut runtime_context = RuntimeSystemContext::new(db.clone(), "before".to_string());
        let mut context = aither_agent::Context::default();
        runtime_context.reconcile(&mut context);

        tokio_test::block_on(db.set_global_memory("after")).expect("update memory");
        tokio_test::block_on(runtime_context.sync_memory_content());
        runtime_context.reconcile(&mut context);

        // Cache-friendly: the live block still holds the old content; the
        // change is staged and surfaced as an appended diff alert.
        let memory = context
            .system_blocks()
            .get(tag::MEMORY)
            .expect("memory block should be present");
        assert_eq!(memory, &build_memory_context_content("before"));
        assert_eq!(
            context.staged_system_update(tag::MEMORY),
            Some(build_memory_context_content("after").as_str())
        );
        let alerts = system_alerts(&context);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].contains("MEMORY.md changed"));
        assert!(alerts[0].contains("+after"));

        // Repeated reconciles with the same staged content stay quiet.
        runtime_context.reconcile(&mut context);
        assert_eq!(system_alerts(&context).len(), 1);

        // Reassembly folds the staged update in and clears the alert.
        let report = context.reassemble();
        assert_eq!(report.staged_updates_applied, 1);
        let memory = context
            .system_blocks()
            .get(tag::MEMORY)
            .expect("memory block should be present");
        assert_eq!(memory, &build_memory_context_content("after"));
        assert_eq!(system_alerts(&context), [] as [std::string::String; 0]);
    }

    #[test]
    fn terminal_runtime_context_is_reconciled_as_system_block() {
        let db = test_db();
        let runtime_context = RuntimeSystemContext::new(db, "remember this".to_string())
            .with_terminal_runtime("Model: demo-model\nHome environment kind: local".to_string());
        let mut context = aither_agent::Context::default();
        runtime_context.reconcile(&mut context);

        let terminal_runtime = context
            .system_blocks()
            .get(tag::TERMINAL_RUNTIME_CONTEXT)
            .expect("terminal runtime block should be present");
        assert!(
            terminal_runtime.contains("demo-model"),
            "unexpected terminal runtime block: {terminal_runtime}"
        );
        assert!(
            !terminal_runtime.contains('<'),
            "raw-text block must not contain XML tags: {terminal_runtime}"
        );
    }

    #[test]
    fn workspace_and_workdirs_are_reconciled_independently() {
        let db = test_db();
        let mut runtime_context = RuntimeSystemContext::new(db, String::new())
            .with_workspace("Workspace: local".to_string());
        let mut context = aither_agent::Context::default();
        runtime_context.reconcile(&mut context);
        let without_workdirs_fingerprint = context.stable_prefix_fingerprint();

        runtime_context = runtime_context
            .with_workspace_workdirs("Approved workspace directories:\n- /foo".to_string());
        runtime_context.reconcile(&mut context);

        assert!(context.has_system_block(tag::WORKSPACE));
        assert!(context.has_system_block(tag::WORKSPACE_WORKDIRS));
        assert_ne!(
            without_workdirs_fingerprint,
            context.stable_prefix_fingerprint(),
            "adding workdirs block must change the fingerprint",
        );
    }

    fn render_base_system_prompt(
        commands: &[(String, String)],
        request_parameters: &Parameters,
    ) -> String {
        let dir = tempdir().expect("temp dir should be created");
        tokio_test::block_on(build_base_system_prompt(
            commands,
            &native_tool_definitions(),
            request_parameters,
            dir.path(),
            dir.path(),
        ))
        .expect("base system prompt should render")
    }

    #[test]
    fn base_system_prompt_lists_native_tools() {
        let prompt = render_base_system_prompt(&[], &Parameters::default());

        let native_section = prompt
            .split("<native_tools>")
            .nth(1)
            .and_then(|rest| rest.split("</native_tools>").next())
            .expect("native tools section should render");
        for name in [
            "terminal",
            "terminal_kill",
            "terminal_input",
            "terminal_read",
        ] {
            assert!(
                native_section.contains(&format!("- {name}:")),
                "native tool '{name}' missing: {native_section}"
            );
        }
    }

    #[test]
    fn base_system_prompt_advertises_hosted_web_search_and_fetch() {
        let commands = vec![("browser".to_string(), "Drive the browser".to_string())];
        let mut parameters = Parameters::default();
        parameters.native_tools.claude.web_search = true;
        parameters.native_tools.claude.web_fetch = true;

        let prompt = render_base_system_prompt(&commands, &parameters);

        let native_section = prompt
            .split("<native_tools>")
            .nth(1)
            .and_then(|rest| rest.split("</native_tools>").next())
            .expect("native tools section should render");
        assert!(native_section.contains("- web_search:"), "{native_section}");
        assert!(native_section.contains("- web_fetch:"), "{native_section}");
        assert!(prompt.contains("Use `web_search` to look up current information"));
        assert!(prompt.contains("Use `web_fetch` to read a known URL"));
        assert!(prompt.contains(
            "The `browser` command is for interacting with pages or reaching what search and fetch cannot"
        ));
    }

    #[test]
    fn base_system_prompt_keeps_web_commands_when_hosted_tools_off() {
        let commands = vec![
            ("browser".to_string(), "Drive the browser".to_string()),
            ("websearch".to_string(), "Search the web".to_string()),
            ("webfetch".to_string(), "Fetch a URL".to_string()),
        ];

        let prompt = render_base_system_prompt(&commands, &Parameters::default());

        let native_section = prompt
            .split("<native_tools>")
            .nth(1)
            .and_then(|rest| rest.split("</native_tools>").next())
            .expect("native tools section should render");
        assert!(!native_section.contains("web_search"), "{native_section}");
        assert!(!native_section.contains("web_fetch"), "{native_section}");
        assert!(!prompt.contains("look up current information"));
        let commands_section = prompt
            // The tool architecture mentions `<commands>` inline, so match the
            // tag on its own line.
            .split("\n<commands>\n")
            .nth(1)
            .and_then(|rest| rest.split("</commands>").next())
            .expect("commands section should render");
        assert!(
            commands_section.contains("- websearch: Search the web"),
            "{commands_section}"
        );
        assert!(
            commands_section.contains("- webfetch: Fetch a URL"),
            "{commands_section}"
        );
    }
}
