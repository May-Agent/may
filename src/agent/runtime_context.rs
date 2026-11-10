//! The session system prompt May assembles on aither's layout.
//!
//! Instruction blocks come first, one per concern, each rendered from its own
//! typed template under `templates/prompt/` and wrapped in a tag named after
//! its slot. External content (memory, the global `~/.may/AGENTS.md`, a
//! repository's `AGENTS.md`, the approved workspace directories) comes last, in aither's provenance-labelled
//! blocks whose boundary their content cannot close.
//!
//! [`RuntimeSystemContext`] holds every block May writes and reconciles them
//! into an [`aither_agent::Context`] each turn. Memory and `AGENTS.md` change
//! while the session runs, so they are aither live content
//! ([`aither_agent::Context::update_live`]): a change is announced as a diff
//! while the cached prefix stays warm, and reassembly folds the clean version
//! in.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use aither_agent::context::LiveContent;
use aither_agent::{AgentOptions, ExternalContent, InstructionSlot};
use aither_core::llm::model::{Ability, Parameters};
use askama::Template;
use async_fs;

use super::may::{AgentEnvironment, MayAgentError};
use crate::agent::agents_md::{self, AgentsIndex, HandoffAgentDoc, RulesDocument};
use crate::config::{SshServerConfig, TimeZoneName};
use crate::network_policy::{CompiledNetworkPolicy, NetworkPolicyMode};
use crate::session::{SessionPaths, SessionPlanState};
use crate::skill::SkillManager;
use crate::terminal_commands::CoreCommandUsage;
use crate::workspace::{Workspace as MayWorkspace, WorkspaceDir as MayWorkspaceDir};

/// The slots of the blocks May writes, and the order of the whole prompt.
pub mod tag {
    use aither_agent::{ExternalSlot, InstructionSlot, SystemLayout};

    pub const IDENTITY: InstructionSlot = InstructionSlot::new("identity");
    pub const PROFILE_PERSONA: InstructionSlot = InstructionSlot::new("profile_persona");
    pub const ENVIRONMENT: InstructionSlot = InstructionSlot::new("environment");
    pub const SESSION: InstructionSlot = InstructionSlot::new("session");
    pub const REPO_CLONES: InstructionSlot = InstructionSlot::new("repo_clones");
    pub const TERMINAL: InstructionSlot = InstructionSlot::new("terminal");
    pub const EFFICIENCY: InstructionSlot = InstructionSlot::new("efficiency");
    pub const CAPABILITIES: InstructionSlot = InstructionSlot::new("capabilities");
    pub const BROWSER: InstructionSlot = InstructionSlot::new("browser");
    pub const WORK: InstructionSlot = InstructionSlot::new("work");
    pub const PROJECT_RULES: InstructionSlot = InstructionSlot::new("project_rules");
    pub const PLAN_MODE: InstructionSlot = InstructionSlot::new("plan_mode");
    /// aither's built-in permissions slot, holding May's text.
    pub const PERMISSIONS: InstructionSlot = InstructionSlot::PERMISSIONS;
    pub const HARNESS: InstructionSlot = InstructionSlot::new("harness");
    pub const EVIDENCE: InstructionSlot = InstructionSlot::new("evidence");
    /// aither's built-in knowledge-and-time slot, holding May's text.
    pub const TIME: InstructionSlot = InstructionSlot::KNOWLEDGE_AND_TIME;

    pub const MEMORY: ExternalSlot = ExternalSlot::new("memory");
    pub const PROFILE_MEMORIES: ExternalSlot = ExternalSlot::new("profile_memories");
    pub const GLOBAL_AGENTS_MD: ExternalSlot = ExternalSlot::new("global_agents_md");
    pub const AGENTS_MD: ExternalSlot = ExternalSlot::new("agents_md");
    pub const HANDOFF_AGENTS_MD: ExternalSlot = ExternalSlot::new("handoff_agents_md");
    pub const WORKSPACE_WORKDIRS: ExternalSlot = ExternalSlot::new("workspace_workdirs");

    /// The instruction slots May writes, in prompt order. aither's
    /// `prepared_tools`, `commands` and `languages` blocks sit between them
    /// in [`layout`].
    pub const MAY_INSTRUCTIONS: [InstructionSlot; 16] = [
        IDENTITY,
        HARNESS,
        EVIDENCE,
        PROFILE_PERSONA,
        ENVIRONMENT,
        TERMINAL,
        EFFICIENCY,
        CAPABILITIES,
        BROWSER,
        WORK,
        PERMISSIONS,
        TIME,
        SESSION,
        REPO_CLONES,
        PLAN_MODE,
        PROJECT_RULES,
    ];

    /// The system prompt's order, from most shared to least shared, so
    /// sessions share the longest cached prefix. Text every session of a
    /// build has comes first: identity, harness and evidence. Text that
    /// depends on the installation and its configuration follows: persona,
    /// environment, terminal, prepared tools, efficiency, capabilities and
    /// the command table, browser, work, permissions, time and languages.
    /// Text specific to the session comes last: the session section, repo
    /// clones, plan mode and project rules, then the external content.
    #[must_use]
    pub fn layout() -> SystemLayout {
        SystemLayout::new(
            [
                IDENTITY,
                HARNESS,
                EVIDENCE,
                PROFILE_PERSONA,
                ENVIRONMENT,
                TERMINAL,
                InstructionSlot::PREPARED_TOOLS,
                EFFICIENCY,
                CAPABILITIES,
                InstructionSlot::COMMANDS,
                BROWSER,
                WORK,
                PERMISSIONS,
                TIME,
                InstructionSlot::LANGUAGES,
                SESSION,
                REPO_CLONES,
                PLAN_MODE,
                PROJECT_RULES,
            ],
            [
                MEMORY,
                PROFILE_MEMORIES,
                GLOBAL_AGENTS_MD,
                AGENTS_MD,
                HANDOFF_AGENTS_MD,
                WORKSPACE_WORKDIRS,
            ],
        )
    }
}

/// Where memory lives as the shell sees it, the source label of its block.
const MEMORY_SOURCE: &str = "~/.may/memories/MEMORY.md";
/// The source label of the global rules block.
const GLOBAL_AGENTS_SOURCE: &str = "~/.may/AGENTS.md";
const PROFILE_MEMORIES_SOURCE: &str = "profile memories extracted from past sessions";
const WORKDIRS_SOURCE: &str = "approved workspace directories";
const HANDOFF_AGENTS_SOURCE: &str = "AGENTS.md files linked from the compaction handoff";

/// Every block May writes into a session's system prompt, and the sources it
/// re-reads each turn.
#[derive(Clone)]
pub struct RuntimeSystemContext {
    /// Instruction blocks fixed when the agent is built.
    instructions: BTreeMap<InstructionSlot, String>,
    profile_memories: Option<ExternalContent>,
    workdirs: Option<ExternalContent>,
    memory_path: PathBuf,
    /// Snapshot of every file under the memories directory; `None` until the
    /// first scan establishes a silent baseline.
    memories_snapshot: Option<HashMap<String, String>>,
    last_memory_content: String,
    /// The global rules file, re-read every turn.
    global_agents_path: Option<PathBuf>,
    agents_workdir: Option<PathBuf>,
    agents_root_source: String,
    agents: AgentsMdBlocks,
    handoff_agents: Vec<HandoffAgentDoc>,
    last_handoff: Option<String>,
}

impl RuntimeSystemContext {
    pub(crate) fn new(memory_path: PathBuf, memory_content: String) -> Self {
        Self {
            instructions: BTreeMap::new(),
            profile_memories: None,
            workdirs: None,
            memory_path,
            memories_snapshot: None,
            last_memory_content: memory_content,
            global_agents_path: None,
            agents_workdir: None,
            agents_root_source: agents_md_source(None),
            agents: AgentsMdBlocks::default(),
            handoff_agents: Vec::new(),
            last_handoff: None,
        }
    }

    /// Sets the instruction block in `slot`, one of [`tag::MAY_INSTRUCTIONS`]
    /// other than `project_rules`.
    ///
    /// # Panics
    /// Panics on a slot May does not write this way: a mistake in the caller.
    pub(crate) fn with_instruction(mut self, slot: InstructionSlot, text: String) -> Self {
        assert!(
            tag::MAY_INSTRUCTIONS.contains(&slot) && slot != tag::PROJECT_RULES,
            "`{}` is not an instruction block May renders at build",
            slot.name()
        );
        self.instructions.insert(slot, text);
        self
    }

    pub(crate) fn with_profile_memories(mut self, memories: Option<ExternalContent>) -> Self {
        self.profile_memories = memories;
        self
    }

    pub(crate) fn with_workdirs(mut self, workdirs: Option<ExternalContent>) -> Self {
        self.workdirs = workdirs;
        self
    }

    pub(crate) fn with_agents_md(mut self, agents: AgentsMdBlocks) -> Self {
        self.agents = agents;
        self
    }

    /// Marks the launch directory `AGENTS.md` is discovered from, so
    /// [`Self::sync_agents_md`] can rescan it every turn. `label` is the
    /// launch directory as the model sees it, when it sees the host path.
    pub(crate) fn with_agents_source(mut self, workdir: PathBuf, label: Option<&str>) -> Self {
        self.agents_workdir = Some(workdir);
        self.agents_root_source = agents_md_source(label);
        self
    }

    /// Marks the global rules file, so [`Self::sync_agents_md`] re-reads it
    /// every turn.
    pub(crate) fn with_global_agents_source(mut self, path: PathBuf) -> Self {
        self.global_agents_path = Some(path);
        self
    }

    /// The source label of the launch directory's `AGENTS.md` block.
    pub(crate) fn agents_root_source(&self) -> &str {
        &self.agents_root_source
    }

    fn memory_block(&self) -> ExternalContent {
        ExternalContent::data(MEMORY_SOURCE, self.last_memory_content.as_str())
    }

    /// Hands every block to the agent builder, so the first request carries
    /// them before any reconcile.
    pub(crate) fn install<B: AgentOptions>(&self, mut builder: B) -> B {
        for (slot, text) in &self.instructions {
            builder = builder.system_text(slot, text.clone());
        }
        if let Some(rules) = &self.agents.rules {
            builder = builder.system_text(&tag::PROJECT_RULES, rules.clone());
        }
        builder = builder.external_content(&tag::MEMORY, self.memory_block());
        for (slot, block) in [
            (&tag::PROFILE_MEMORIES, &self.profile_memories),
            (&tag::GLOBAL_AGENTS_MD, &self.agents.global),
            (&tag::AGENTS_MD, &self.agents.root),
            (&tag::HANDOFF_AGENTS_MD, &self.agents.handoff),
            (&tag::WORKSPACE_WORKDIRS, &self.workdirs),
        ] {
            if let Some(block) = block {
                builder = builder.external_content(slot, block.clone());
            }
        }
        builder
    }

    pub(crate) fn reconcile(&self, context: &mut aither_agent::Context) {
        let instructions = context.instructions_mut();
        for slot in &tag::MAY_INSTRUCTIONS {
            if *slot == tag::PROJECT_RULES {
                continue;
            }
            match self.instructions.get(slot) {
                Some(text) => instructions.insert(slot, text.clone()),
                None => {
                    instructions.remove(slot);
                }
            }
        }
        context.update_live(&MEMORY_CONTENT, |update| {
            update.external(&tag::MEMORY, Some(self.memory_block()));
        });
        context.update_live(&AGENTS_MD_CONTENT, |update| {
            update
                .instruction(&tag::PROJECT_RULES, self.agents.rules.clone())
                .external(&tag::GLOBAL_AGENTS_MD, self.agents.global.clone())
                .external(&tag::AGENTS_MD, self.agents.root.clone())
                .external(&tag::HANDOFF_AGENTS_MD, self.agents.handoff.clone());
        });
        let external = context.external_mut();
        for (slot, block) in [
            (&tag::PROFILE_MEMORIES, &self.profile_memories),
            (&tag::WORKSPACE_WORKDIRS, &self.workdirs),
        ] {
            match block {
                Some(block) => external.insert(slot, block.clone()),
                None => {
                    external.remove(slot);
                }
            }
        }
    }

    /// Refreshes the `AGENTS.md` blocks: re-reads the global rules, rescans
    /// the launch directory and, when the active handoff changed, reloads
    /// the `AGENTS.md` files it links to. The reconcile stages any difference
    /// with a diff alert, so the cached prefix stays warm until reassembly
    /// folds it in.
    pub(crate) async fn sync_agents_md(&mut self, handoff: Option<&str>) {
        let workdir = self.agents_workdir.clone();
        let reload = handoff != self.last_handoff.as_deref();
        let global_fut = async {
            match &self.global_agents_path {
                Some(path) => agents_md::load_global(path).await,
                None => None,
            }
        };
        let index_fut = async {
            match &workdir {
                Some(dir) => agents_md::discover(dir).await,
                None => AgentsIndex::default(),
            }
        };
        let docs_fut = async {
            if !reload {
                return Vec::new();
            }
            match handoff {
                Some(markdown) => agents_md::load_referenced(markdown, workdir.as_deref()).await,
                None => Vec::new(),
            }
        };
        let ((global, index), docs) =
            futures_lite::future::zip(futures_lite::future::zip(global_fut, index_fut), docs_fut)
                .await;
        if reload {
            self.last_handoff = handoff.map(str::to_string);
            self.handoff_agents = docs;
        }
        self.agents = build_agents_md_blocks(
            global.as_ref(),
            &index,
            &self.agents_root_source,
            &self.handoff_agents,
        );
    }

    /// Re-read the memory index and watch the whole memories directory,
    /// alerting on changes to files this session has actually seen.
    pub(crate) async fn sync_memory_content(&mut self, context: &mut aither_agent::Context) {
        match async_fs::read_to_string(&self.memory_path).await {
            Ok(index_content) => self.last_memory_content = index_content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.last_memory_content = String::new();
            }
            Err(error) => {
                tracing::error!(error = %error, "failed to read global memory file");
            }
        }

        let Some(memories_dir) = self.memory_path.parent() else {
            return;
        };
        let mut next = HashMap::new();
        scan_memories_dir(memories_dir, memories_dir, &mut next);
        let Some(snapshot) = self.memories_snapshot.replace(next.clone()) else {
            // First scan is a silent baseline: nothing counts as changed.
            return;
        };

        // A file counts as read only if a memory path reached the
        // conversation — changes to never-surfaced files stay silent.
        let mut seen = HashSet::new();
        for message in context.conversation_messages() {
            collect_memory_paths(message.content(), &mut seen);
        }

        let mut changes = Vec::new();
        for (rel, content) in &next {
            if rel == "MEMORY.md" || !seen.contains(rel) {
                continue;
            }
            if snapshot.get(rel) != Some(content) {
                changes.push(format!("{rel} (modified)"));
            }
        }
        for rel in snapshot.keys() {
            if rel != "MEMORY.md" && !next.contains_key(rel) && seen.contains(rel) {
                changes.push(format!("{rel} (deleted)"));
            }
        }
        if !changes.is_empty() {
            changes.sort();
            context.push_alert(&MemoriesChanges { files: changes });
        }
    }
}

fn scan_memories_dir(root: &Path, dir: &Path, out: &mut HashMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            scan_memories_dir(root, &path, out);
        } else if let Ok(content) = std::fs::read_to_string(&path) {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(rel, content);
        }
    }
}

/// Collect `memories/<relative>` path mentions out of a text.
fn collect_memory_paths(text: &str, out: &mut HashSet<String>) {
    let mut rest = text;
    while let Some(index) = rest.find("memories/") {
        let tail = &rest[index + "memories/".len()..];
        let end = tail
            .find(|c: char| !(c.is_alphanumeric() || matches!(c, '/' | '.' | '-' | '_')))
            .unwrap_or(tail.len());
        let rel = tail[..end].trim_end_matches(['.', '/']).to_string();
        if !rel.is_empty() && rel != "." {
            out.insert(rel);
        }
        rest = &tail[end..];
    }
}

/// Durable alert listing which files under `~/.may/memories/` changed.
#[derive(Template)]
#[template(path = "alert_memories_changes.txt", escape = "none")]
struct MemoriesChanges {
    files: Vec<String>,
}

/// Durable alert telling the agent the user fell asleep; `greet` is set in
/// the session that ran `/sleep`.
#[derive(Template)]
#[template(path = "alert_sleep.txt", escape = "none")]
pub struct SleepAlert {
    greet: bool,
}

/// Durable alert telling the agent the user woke and how.
#[derive(Template)]
#[template(path = "alert_wake.txt", escape = "none")]
pub struct WakeAlert {
    cause: crate::sleep::WakeCause,
}

impl RuntimeSystemContext {
    /// Records what the user's sleep state changed to as a durable alert.
    pub(crate) fn push_sleep_notice(
        context: &mut aither_agent::Context,
        notice: crate::sleep::SleepNotice,
    ) {
        match notice {
            crate::sleep::SleepNotice::Asleep { greet } => {
                context.push_alert(&SleepAlert { greet });
            }
            crate::sleep::SleepNotice::Woke(cause) => {
                context.push_alert(&WakeAlert { cause });
            }
        }
    }
}

/// The memory index, announced by name when it changes mid-session.
const MEMORY_CONTENT: LiveContent = LiveContent::new("MEMORY.md", "memory_diff");

/// The `AGENTS.md` blocks (the rules on how to treat them, the global rules,
/// the launch directory's own file and the files the handoff linked),
/// announced together when any of them changes mid-session.
const AGENTS_MD_CONTENT: LiveContent = LiveContent::new("The AGENTS.md files", "agents_md_diff");

/// The blocks the global rules and a launch directory's `AGENTS.md` files
/// produce.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentsMdBlocks {
    /// How to treat the files: the nested list, a cut file, handoff links.
    rules: Option<String>,
    /// The global rules, `~/.may/AGENTS.md`.
    global: Option<ExternalContent>,
    /// The launch directory's own `AGENTS.md`.
    root: Option<ExternalContent>,
    /// The `AGENTS.md` files the compaction handoff linked.
    handoff: Option<ExternalContent>,
}

/// The source label of the launch directory's `AGENTS.md` block.
fn agents_md_source(launch_dir: Option<&str>) -> String {
    launch_dir.map_or_else(
        || "AGENTS.md of the launch directory".to_string(),
        |dir| Path::new(dir).join("AGENTS.md").display().to_string(),
    )
}

#[derive(Template)]
#[template(path = "prompt/project_rules.md", escape = "none")]
struct ProjectRulesTemplate<'a> {
    global_truncated: bool,
    truncated_root: Option<&'a str>,
    nested: &'a [String],
    extra_nested: usize,
}

#[derive(Template)]
#[template(path = "prompt/handoff_agents.md", escape = "none")]
struct HandoffAgentsTemplate<'a> {
    documents: &'a [HandoffAgentDoc],
}

/// Builds the `AGENTS.md` blocks from the global rules, the launch-directory
/// scan and the documents the active compaction handoff linked;
/// `root_source` labels the launch directory's own file.
pub fn build_agents_md_blocks(
    global: Option<&RulesDocument>,
    index: &AgentsIndex,
    root_source: &str,
    handoff_documents: &[HandoffAgentDoc],
) -> AgentsMdBlocks {
    if global.is_none()
        && index.root_document.is_none()
        && index.nested.is_empty()
        && handoff_documents.is_empty()
    {
        return AgentsMdBlocks::default();
    }
    let rules = ProjectRulesTemplate {
        global_truncated: global.is_some_and(|document| document.truncated),
        truncated_root: (index.root_document.is_some() && index.root_truncated)
            .then_some(root_source),
        nested: &index.nested,
        extra_nested: index.extra_nested,
    }
    .render()
    .unwrap_or_else(|error| panic!("failed to render project rules: {error}"));
    let handoff = (!handoff_documents.is_empty()).then(|| {
        let content = HandoffAgentsTemplate {
            documents: handoff_documents,
        }
        .render()
        .unwrap_or_else(|error| panic!("failed to render handoff AGENTS.md: {error}"));
        ExternalContent::repository_instructions(HANDOFF_AGENTS_SOURCE, content)
    });
    AgentsMdBlocks {
        rules: Some(rules),
        global: global.map(|document| {
            ExternalContent::repository_instructions(GLOBAL_AGENTS_SOURCE, &document.content)
        }),
        root: index
            .root_document
            .as_ref()
            .map(|document| ExternalContent::repository_instructions(root_source, document)),
        handoff,
    }
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

#[derive(Template)]
#[template(path = "prompt/browser.md", escape = "none")]
struct BrowserTemplate {
    state: BrowserContextState,
    cdp_endpoint: Option<String>,
}

pub fn build_browser_context(enabled: bool, cdp_endpoint: Option<&str>) -> String {
    let state = if enabled {
        BrowserContextState::Enabled
    } else {
        BrowserContextState::Disabled
    };
    BrowserTemplate {
        state,
        cdp_endpoint: cdp_endpoint.map(str::to_string),
    }
    .render()
    .unwrap_or_else(|error| panic!("failed to render browser context: {error}"))
}

#[derive(Template)]
#[template(path = "prompt/identity.md", escape = "none")]
struct IdentityTemplate;

/// The identity section: who the agent is, the operating mode, and the rule
/// that questions go through `ask_user`.
pub fn build_identity() -> String {
    IdentityTemplate
        .render()
        .unwrap_or_else(|error| panic!("failed to render identity: {error}"))
}

#[derive(Template)]
#[template(path = "prompt/profile_persona.md", escape = "none")]
struct ProfilePersonaTemplate<'a> {
    persona: &'a str,
}

/// The profile's own persona, which the user wrote.
pub fn build_profile_persona(persona: &str) -> String {
    ProfilePersonaTemplate { persona }
        .render()
        .unwrap_or_else(|error| panic!("failed to render profile persona: {error}"))
}

/// The home environment as the environment and permissions sections state it.
enum HomeEnvironment {
    /// The user's own machine, under the sandbox.
    Local {
        os: &'static str,
        /// The user's home directory, which `~` does not point to.
        host_home: Option<String>,
    },
    /// A managed container.
    Container,
}

impl HomeEnvironment {
    fn of(environment: &AgentEnvironment) -> Self {
        match environment {
            AgentEnvironment::Local => Self::Local {
                os: host_operating_system_label(),
                host_home: Some(
                    crate::config::MayConfig::home_dir()
                        .expect(
                            "the home directory resolved when the session directory was derived",
                        )
                        .display()
                        .to_string(),
                ),
            },
            AgentEnvironment::Container { .. } => Self::Container,
        }
    }

    const fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }
}

/// The remote hosts attached to the session; never empty, so the
/// multi-environment text renders only while one is connected.
struct RemoteHosts(Vec<SshServerConfig>);

impl RemoteHosts {
    fn connected(hosts: Vec<SshServerConfig>) -> Option<Self> {
        (!hosts.is_empty()).then_some(Self(hosts))
    }

    fn hosts(&self) -> &[SshServerConfig] {
        &self.0
    }
}

#[derive(Template)]
#[template(path = "prompt/environment.md", escape = "none")]
struct EnvironmentTemplate<'a> {
    model_id: &'a str,
    home: HomeEnvironment,
    modalities: Vec<&'static str>,
    remote: Option<RemoteHosts>,
}

/// The inputs a model takes natively, as the environment section names them.
fn native_inputs(model_info: Option<&aither_models::ModelEntry>) -> Vec<&'static str> {
    let Some(info) = model_info else {
        return Vec::new();
    };
    [
        (Ability::Vision, "images"),
        (Ability::Audio, "audio"),
        (Ability::Video, "video"),
        (Ability::Pdf, "PDFs"),
    ]
    .into_iter()
    .filter(|(ability, _)| info.has_ability(*ability))
    .map(|(_, name)| name)
    .collect()
}

/// The environment section: where the agent runs, the layout of the session
/// directory and the remote hosts while any is connected. It refers to the
/// session directory by role; [`build_session`] holds the path.
pub fn build_environment(
    model_id: &str,
    environment: &AgentEnvironment,
    model_info: Option<&aither_models::ModelEntry>,
    remote_hosts: Vec<SshServerConfig>,
) -> String {
    EnvironmentTemplate {
        model_id,
        home: HomeEnvironment::of(environment),
        modalities: native_inputs(model_info),
        remote: RemoteHosts::connected(remote_hosts),
    }
    .render()
    .unwrap_or_else(|error| panic!("failed to render environment: {error}"))
}

#[derive(Template)]
#[template(path = "prompt/session.md", escape = "none")]
struct SessionTemplate<'a> {
    session_dir: String,
    launch_dir: Option<String>,
    issue_repository: Option<&'a str>,
}

/// The session section, where the values only this session has are stated:
/// its directory, the launch directory and the issue repository.
pub fn build_session(
    environment: &AgentEnvironment,
    session_paths: &SessionPaths,
    launch_dir: Option<String>,
    issue_backend: &crate::issue::IssueBackendInfo,
) -> String {
    SessionTemplate {
        session_dir: sandbox_visible_path(environment, session_paths, session_paths.root()),
        launch_dir,
        issue_repository: issue_backend.repository.as_deref(),
    }
    .render()
    .unwrap_or_else(|error| panic!("failed to render session: {error}"))
}

#[derive(Template)]
#[template(path = "prompt/terminal.md", escape = "none")]
struct TerminalTemplate {
    /// Whether scripts run in the managed bash, which May's command policy
    /// confines to `rg`, `fd` and line-range `sed`.
    managed_bash: bool,
}

/// The terminal section: how to run commands.
pub fn build_terminal(environment: &AgentEnvironment) -> String {
    TerminalTemplate {
        managed_bash: crate::command_policy::runs_managed_bash(matches!(
            environment,
            AgentEnvironment::Container { .. }
        )),
    }
    .render()
    .unwrap_or_else(|error| panic!("failed to render terminal: {error}"))
}

#[derive(Template)]
#[template(path = "prompt/efficiency.md", escape = "none")]
struct EfficiencyTemplate {
    has_search_command: bool,
}

/// The efficiency section: concurrent calls, the cheapest search, `explore`.
pub fn build_efficiency(commands: &[(String, String)]) -> String {
    EfficiencyTemplate {
        has_search_command: commands.iter().any(|(name, _)| name == "search"),
    }
    .render()
    .unwrap_or_else(|error| panic!("failed to render efficiency: {error}"))
}

struct CatalogSkill {
    dir_name: String,
    name: String,
    description: String,
}

struct CatalogSubagent {
    id: String,
    description: String,
}

#[derive(Template)]
#[template(path = "prompt/capabilities.md", escape = "none")]
struct CapabilitiesTemplate {
    has_hosted_web_search: bool,
    has_hosted_web_fetch: bool,
    has_browser_command: bool,
    skills: Vec<CatalogSkill>,
    subagents: Vec<CatalogSubagent>,
    core_commands: Vec<CoreCommandUsage>,
}

/// The capabilities section: how commands run, the skills and the
/// subagents. aither lists the commands themselves in its live command
/// table; `commands` are the ones registered by name, which decide what the
/// section says about them.
///
/// # Errors
/// Fails when the skills directory cannot be read.
pub async fn build_capabilities(
    commands: &[(String, String)],
    core_commands: &[CoreCommandUsage],
    request_parameters: &Parameters,
    skills_dir: &Path,
) -> Result<String, MayAgentError> {
    let has_browser_command = commands.iter().any(|(name, _)| name == "browser");

    let skills = SkillManager::from_skills_dir(skills_dir.to_path_buf())
        .list_skills()
        .await
        .map_err(|error| {
            MayAgentError::Sandbox(format!("failed to load skill prompt metadata: {error}"))
        })?
        .into_iter()
        .map(|skill| CatalogSkill {
            dir_name: skill.dir_name,
            name: skill.name,
            description: skill.description,
        })
        .collect::<Vec<_>>();

    let mut subagents: Vec<CatalogSubagent> = crate::bundle::Bundle::subagent_definitions()
        .into_iter()
        .map(|definition| CatalogSubagent {
            id: definition.name,
            description: definition.description,
        })
        .collect();
    subagents.push(CatalogSubagent {
        id: "fork".to_string(),
        description: "Continues your own context on your model; use it for work that depends on what you have read and decided".to_string(),
    });
    subagents.sort_by(|left, right| left.id.cmp(&right.id));

    CapabilitiesTemplate {
        has_hosted_web_search: request_parameters.hosted_web_search(),
        has_hosted_web_fetch: request_parameters.hosted_web_fetch(),
        has_browser_command,
        skills,
        subagents,
        core_commands: core_commands.to_vec(),
    }
    .render()
    .map_err(|error| MayAgentError::Sandbox(format!("failed to render capabilities: {error}")))
}

#[derive(Template)]
#[template(path = "prompt/work.md", escape = "none")]
struct WorkTemplate<'a> {
    issue_backend_name: &'a str,
}

/// The work section: issues, PRs, `tasks.md`, artifacts and memory. The
/// issue repository is the session's, so the session section names it.
pub fn build_work(issue_backend: &crate::issue::IssueBackendInfo) -> String {
    WorkTemplate {
        issue_backend_name: issue_backend.name,
    }
    .render()
    .unwrap_or_else(|error| panic!("failed to render work: {error}"))
}

/// A network policy as the permissions section states it.
enum NetworkRestriction {
    Offline,
    Only(Vec<String>),
    Except(Vec<String>),
}

impl NetworkRestriction {
    fn of(policy: &CompiledNetworkPolicy) -> Option<Self> {
        let hosts = || policy.hosts().to_vec();
        match policy.mode() {
            NetworkPolicyMode::Disabled => None,
            NetworkPolicyMode::Restricted => Some(Self::Offline),
            NetworkPolicyMode::AllowList => Some(Self::Only(hosts())),
            NetworkPolicyMode::DenyList => Some(Self::Except(hosts())),
        }
    }
}

#[derive(Template)]
#[template(path = "prompt/permissions.md", escape = "none")]
struct PermissionsTemplate<'a> {
    home: HomeEnvironment,
    secret_names: Vec<&'a str>,
    network: Option<NetworkRestriction>,
}

/// The permissions and safety section: what the agent may write, escalation,
/// untrusted text and stored secrets.
pub fn build_permissions(
    environment: &AgentEnvironment,
    secret_names: Vec<&str>,
    network_policy: Option<&CompiledNetworkPolicy>,
) -> String {
    PermissionsTemplate {
        home: HomeEnvironment::of(environment),
        secret_names,
        network: network_policy.and_then(NetworkRestriction::of),
    }
    .render()
    .unwrap_or_else(|error| panic!("failed to render permissions: {error}"))
}

#[derive(Template)]
#[template(path = "prompt/harness.md", escape = "none")]
struct HarnessTemplate;

/// The harness section: each mechanism and how to work with it.
pub fn build_harness() -> String {
    HarnessTemplate
        .render()
        .unwrap_or_else(|error| panic!("failed to render harness: {error}"))
}

#[derive(Template)]
#[template(path = "prompt/evidence.md", escape = "none")]
struct EvidenceTemplate;

/// The evidence section: alerts as facts, causes with their output.
pub fn build_evidence() -> String {
    EvidenceTemplate
        .render()
        .unwrap_or_else(|error| panic!("failed to render evidence: {error}"))
}

#[derive(Template)]
#[template(path = "prompt/time.md", escape = "none")]
struct TimeTemplate<'a> {
    has_hosted_web_search: bool,
    timezone: &'a TimeZoneName,
    location: Option<&'a crate::location::LocationInfo>,
}

/// The time section: the knowledge cutoff, the user's timezone and, for a
/// location-aware profile, where the user probably is.
pub fn build_time(
    request_parameters: &Parameters,
    timezone: &TimeZoneName,
    location: Option<&crate::location::LocationInfo>,
) -> String {
    TimeTemplate {
        has_hosted_web_search: request_parameters.hosted_web_search(),
        timezone,
        location,
    }
    .render()
    .unwrap_or_else(|error| panic!("failed to render time: {error}"))
}

/// A planning phase that adds a section.
enum PlanPhase {
    Draft,
    Approved,
}

#[derive(Template)]
#[template(path = "prompt/plan_mode.md", escape = "none")]
struct PlanModeTemplate {
    phase: PlanPhase,
}

pub fn build_plan_mode_context(plan_state: SessionPlanState) -> Option<String> {
    let phase = match plan_state {
        SessionPlanState::Disabled => return None,
        SessionPlanState::Draft => PlanPhase::Draft,
        SessionPlanState::Approved => PlanPhase::Approved,
    };
    Some(
        PlanModeTemplate { phase }
            .render()
            .unwrap_or_else(|error| panic!("failed to render plan mode: {error}")),
    )
}

#[derive(Template)]
#[template(path = "prompt/workdirs.md", escape = "none")]
struct WorkspaceWorkdirsTemplate {
    workdirs: Vec<WorkspaceDirEntry>,
}

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

/// The approved workspace directories as an external block; `None` while
/// none is approved.
pub fn build_workspace_workdirs_context(workspace: &MayWorkspace) -> Option<ExternalContent> {
    let workdirs: Vec<WorkspaceDirEntry> = workspace
        .config
        .workdirs
        .iter()
        .map(WorkspaceDirEntry::from)
        .collect();
    if workdirs.is_empty() {
        return None;
    }
    let content = WorkspaceWorkdirsTemplate { workdirs }
        .render()
        .unwrap_or_else(|error| panic!("failed to render workspace workdirs: {error}"));
    Some(ExternalContent::data(WORKDIRS_SOURCE, content))
}

/// The profile's extracted memories as an external block; `None` when there
/// are none.
pub fn build_profile_memories(
    memories: &crate::memory::ProfileMemories,
) -> Option<ExternalContent> {
    (!memories.is_empty())
        .then(|| ExternalContent::data(PROFILE_MEMORIES_SOURCE, memories.to_context_string()))
}

/// One session repository clone shown in runtime context.
#[derive(Clone)]
pub struct RepoCloneEntry {
    /// Clone path inside the session sandbox.
    pub clone_path: PathBuf,
    /// Checkout or remote the clone came from.
    pub source: String,
}

#[derive(Template)]
#[template(path = "prompt/repo_clones.md", escape = "none")]
struct RepoClonesTemplate {
    clones: Vec<RepoCloneTemplateEntry>,
}

struct RepoCloneTemplateEntry {
    path: String,
    source: String,
}

/// Builds the optional `repo_clones` block listing this session's private
/// repository clones.
///
/// # Panics
/// Panics when a clone lies outside the session directory: a container
/// session cannot see it, so the caller broke the clone-placement invariant.
pub fn build_repo_clones_context(
    clones: &[RepoCloneEntry],
    environment: &AgentEnvironment,
    session_paths: &SessionPaths,
) -> Option<String> {
    if clones.is_empty() {
        return None;
    }
    let template = RepoClonesTemplate {
        clones: clones
            .iter()
            .map(|c| RepoCloneTemplateEntry {
                path: sandbox_visible_path(environment, session_paths, &c.clone_path),
                source: c.source.clone(),
            })
            .collect(),
    };
    Some(
        template
            .render()
            .unwrap_or_else(|error| panic!("failed to render repo clones context: {error}")),
    )
}

/// A path under the session directory as the shell sees it: the host path
/// natively, or the same location under the container workspace mount.
///
/// # Panics
/// Panics in a container session when `host_path` is outside the session
/// directory, which the container does not mount.
fn sandbox_visible_path(
    environment: &AgentEnvironment,
    session_paths: &SessionPaths,
    host_path: &Path,
) -> String {
    match environment {
        AgentEnvironment::Local => host_path.display().to_string(),
        AgentEnvironment::Container { .. } => {
            let relative = host_path
                .strip_prefix(session_paths.root())
                .unwrap_or_else(|_| {
                    panic!(
                        "{} is outside the session directory {} that the container mounts",
                        host_path.display(),
                        session_paths.root().display()
                    )
                });
            Path::new(crate::container::lifecycle::CONTAINER_WORKSPACE)
                .join(relative)
                .display()
                .to_string()
        }
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

#[cfg(test)]
#[path = "runtime_context_tests.rs"]
mod tests;
