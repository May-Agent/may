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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use aither_core::llm::model::{Ability, Parameters};
use askama::Template;
use async_fs;

use super::may::{AgentEnvironment, MayAgentConfig, MayAgentError};
use crate::agent::agents_md::{self, AgentsIndex, HandoffAgentDoc};
use crate::network_policy::{CompiledNetworkPolicy, NetworkPolicyMode};
use crate::session::{SessionPaths, SessionPlanState};
use crate::skill::SkillManager;
use crate::workspace::{Workspace as MayWorkspace, WorkspaceDir as MayWorkspaceDir};

const MEMORY_CONTEXT_HINT: &str = include_str!("../texts/memory_context_hint.txt");
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
    pub const REPO_CLONES: &str = "repo_clones";
    pub const AGENTS_MD: &str = "agents_md";
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
    memory_path: PathBuf,
    /// Snapshot of every file under the memories directory; `None` until the
    /// first scan establishes a silent baseline.
    memories_snapshot: Option<HashMap<String, String>>,
    last_memory_content: String,
    terminal_runtime: Option<String>,
    workspace_workdirs: Option<String>,
    repo_clones: Option<String>,
    agents_workdir: Option<PathBuf>,
    agents_workdir_label: Option<String>,
    agents_md: Option<String>,
    handoff_agents: Vec<HandoffAgentDoc>,
    last_handoff: Option<String>,
    browser: Option<String>,
    profile_persona: Option<String>,
    profile_memories: Option<String>,
    plan_mode: Option<String>,
    network_restrictions: Option<String>,
    location: Option<String>,
    native_voice_input: Option<String>,
}

impl RuntimeSystemContext {
    pub(crate) const fn new(memory_path: PathBuf, memory_content: String) -> Self {
        Self {
            memory_path,
            memories_snapshot: None,
            last_memory_content: memory_content,
            terminal_runtime: None,
            workspace_workdirs: None,
            repo_clones: None,
            agents_workdir: None,
            agents_workdir_label: None,
            agents_md: None,
            handoff_agents: Vec::new(),
            last_handoff: None,
            browser: None,
            profile_persona: None,
            profile_memories: None,
            plan_mode: None,
            network_restrictions: None,
            location: None,
            native_voice_input: None,
        }
    }

    pub(crate) fn with_terminal_runtime(mut self, terminal_runtime: String) -> Self {
        self.terminal_runtime = Some(terminal_runtime);
        self
    }

    pub(crate) fn with_workspace_workdirs(mut self, workdirs: String) -> Self {
        self.workspace_workdirs = Some(workdirs);
        self
    }

    pub(crate) fn with_repo_clones(mut self, repo_clones: Option<String>) -> Self {
        self.repo_clones = repo_clones;
        self
    }

    pub(crate) fn with_agents_md(mut self, agents_md: Option<String>) -> Self {
        self.agents_md = agents_md;
        self
    }

    /// Marks the launch directory `agents_md` content is discovered from,
    /// so [`Self::sync_agents_md`] can rescan it every turn.
    pub(crate) fn with_agents_source(
        mut self,
        workdir: PathBuf,
        workdir_label: Option<String>,
    ) -> Self {
        self.agents_workdir = Some(workdir);
        self.agents_workdir_label = workdir_label;
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
        // The workspace and container-environment blocks were folded into the
        // terminal-runtime block; a context restored from an older snapshot may
        // still carry them, so drop them unconditionally.
        reconcile_text_block(context, tag::WORKSPACE, None);
        reconcile_text_block(
            context,
            tag::WORKSPACE_WORKDIRS,
            self.workspace_workdirs.clone(),
        );
        reconcile_text_block(context, tag::REPO_CLONES, self.repo_clones.clone());
        reconcile_agents_md_block(context, self.agents_md.clone());
        reconcile_text_block(context, tag::CONTAINER_ENVIRONMENT_CONTEXT, None);
        // The platform hint is gone; a context restored from an older
        // snapshot may still carry it, so drop it unconditionally.
        reconcile_text_block(context, tag::PLATFORM, None);
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
    }

    /// Refreshes the `agents_md` slot: rescans the launch directory and,
    /// when the active handoff changed, reloads the `AGENTS.md` files it
    /// links to. The reconcile stages any difference with a diff alert,
    /// so the cached prefix stays warm until reassembly folds it in.
    pub(crate) async fn sync_agents_md(&mut self, handoff: Option<&str>) {
        let workdir = self.agents_workdir.clone();
        let reload = handoff != self.last_handoff.as_deref();
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
        let (index, docs) = futures_lite::future::zip(index_fut, docs_fut).await;
        if reload {
            self.last_handoff = handoff.map(str::to_string);
            self.handoff_agents = docs;
        }
        self.agents_md = build_agents_md_context(
            &index,
            self.agents_workdir_label.clone(),
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
#[derive(Template)]
#[template(path = "alert_memory_update.txt", escape = "none")]
struct MemoryUpdate {
    diff: String,
}

/// Durable alert listing which files under `~/.may/memories/` changed.
#[derive(Template)]
#[template(path = "alert_memories_changes.txt", escape = "none")]
struct MemoriesChanges {
    files: Vec<String>,
}

/// Reminder surfaced when the launch directory's `AGENTS.md` index
/// changed mid-session.
#[derive(Template)]
#[template(path = "alert_agents_md_update.txt", escape = "none")]
struct AgentsMdUpdate {
    diff: String,
}

/// Reconciles the `agents_md` block like the memory block: a change is
/// staged rather than rewritten, and surfaced as a durable diff alert,
/// so the cached prefix stays warm until reassembly folds the update in.
fn reconcile_agents_md_block(context: &mut aither_agent::Context, desired: Option<String>) {
    let Some(desired) = desired else {
        context.remove_system_named(tag::AGENTS_MD);
        return;
    };
    if !context.has_system_block(tag::AGENTS_MD) {
        context.insert_system_text(tag::AGENTS_MD, desired);
        return;
    }
    if context.staged_system_update(tag::AGENTS_MD) == Some(desired.as_str()) {
        return;
    }
    let current = context
        .system_blocks()
        .get(tag::AGENTS_MD)
        .cloned()
        .unwrap_or_default();
    if current == desired {
        return;
    }
    if context.stage_system_text(tag::AGENTS_MD, desired.clone()) {
        let diff = similar::TextDiff::from_lines(&current, &desired)
            .unified_diff()
            .context_radius(2)
            .to_string();
        context.push_alert(&AgentsMdUpdate { diff });
    }
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
        context.push_alert(&MemoryUpdate { diff });
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
    working_directory: String,
    home_note: &'static str,
    host_launch_directory: Option<String>,
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
#[template(path = "workspace_workdirs.txt", escape = "none")]
struct WorkspaceWorkdirsTemplate {
    workdirs: Vec<WorkspaceDirEntry>,
}

#[derive(Clone)]
struct WorkspaceDirEntry {
    path: String,
    description: String,
}

/// One session repository clone shown in runtime context.
#[derive(Clone)]
pub struct RepoCloneEntry {
    /// Clone path inside the session sandbox.
    pub clone_path: PathBuf,
    /// Checkout or remote the clone came from.
    pub source: String,
}

#[derive(Clone, Template)]
#[template(path = "repo_clones.txt", escape = "none")]
struct RepoClonesTemplate {
    clones: Vec<RepoCloneTemplateEntry>,
}

#[derive(Clone)]
struct RepoCloneTemplateEntry {
    path: String,
    source: String,
}

#[derive(Clone, Template)]
#[template(path = "agents_md.txt", escape = "none")]
struct AgentsMdTemplate {
    workdir: Option<String>,
    root_document: Option<String>,
    root_truncated: bool,
    nested: Vec<String>,
    extra_nested: usize,
    handoff_documents: Vec<HandoffAgentDoc>,
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

#[derive(Clone)]
struct BaseSystemCommand {
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
    has_hosted_web_search: bool,
    has_hosted_web_fetch: bool,
    has_browser_command: bool,
    has_commands: bool,
    commands: Vec<BaseSystemCommand>,
    has_skills: bool,
    skills: Vec<BaseSystemSkill>,
    has_subagents: bool,
    subagents: Vec<BaseSystemSubagent>,
    issue_backend_name: &'static str,
    issue_repository: Option<String>,
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
    host_launch_directory: Option<String>,
) -> String {
    let mut modality_support = Vec::new();
    if let Some(info) = model_info {
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
        working_directory: working_directory(&config.environment, session_paths),
        home_note: home_note(&config.environment),
        host_launch_directory,
        home_layout: home_layout(&config.environment),
        session_layout: vec![
            "/tmp - disposable temporary files".to_string(),
            "/artifacts - durable user-facing outputs; put deliverables here".to_string(),
            "/files - durable uploaded or recalled files".to_string(),
            "/outputs - durable overflow and full command outputs".to_string(),
            "/data - durable session-owned state".to_string(),
            "/skills, /subagents - read-only profile mounts".to_string(),
            "tasks.md - canonical task/plan document for long-running work; keep its checklist in sync with progress. An item waiting on an open ask_user question or on anything else the run cannot resolve is marked `BLOCKING <request_id or reason>` — it waits for what it names instead of holding the turn open; a mark naming a request that is no longer open does not hold".to_string(),
        ],
        command_rules: vec![
            "Issue at most one tool call per assistant turn; compose multiple operations inside one terminal command with shell control flow, pipelines, or background jobs.".to_string(),
            "Commands run in the current session home environment unless you explicitly target another. IPC-backed commands only work inside this profile's home environment; if a system-reminder reports a call to a non-existent direct tool, retry it through `terminal` as the CLI command.".to_string(),
            "Read a selected skill completely before acting on it; the terminal call that reads the skill must finish before any task command that depends on it.".to_string(),
            "Inline command results are previews May may clean, fold, or compress; the filesystem is durable. After compaction, restart, or long runs, recover prior results from outputs/, data/, files/, artifacts/, or tasks.md - read the slice you need with `head`, `tail`, `rg`, `jq`, or `ask`.".to_string(),
            "Surface a user-facing file by writing it under artifacts/ and emitting an artifact tag on its own line: `<artifact url=\"path/inside/artifacts\" />`; a Slidev deck adds the `slidev` attribute.".to_string(),
            "Ids surfaced by tool results and reminders — sessions, tasks, questions, runs, agents, requests — are internal handles: pass them to commands and reference them in tasks.md, but never quote them to the user.".to_string(),
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

/// Builds the optional `workspace_workdirs` block from currently approved
/// workspace grants. Kept as its own block so that runtime workdir approvals
/// can invalidate only this block and leave the stable prefix cached.
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

/// Builds the optional `repo_clones` block listing this session's private
/// repository clones, kept as its own block so clone creation invalidates
/// only it and not the stable prefix.
pub fn build_repo_clones_context(clones: &[RepoCloneEntry]) -> Option<String> {
    if clones.is_empty() {
        return None;
    }
    let template = RepoClonesTemplate {
        clones: clones
            .iter()
            .map(|c| RepoCloneTemplateEntry {
                path: c.clone_path.display().to_string(),
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

/// Builds the `agents_md` block from the launch-directory scan: the
/// loaded root `AGENTS.md`, the relative paths of nested `AGENTS.md`
/// files the agent must read before working in their subtree, and the
/// `AGENTS.md` documents the active compaction handoff referenced.
/// `workdir` is the launch directory as the model sees it — present only
/// when the home environment exposes the host path (local sessions).
/// `None` when there is nothing to say.
pub fn build_agents_md_context(
    index: &AgentsIndex,
    workdir: Option<String>,
    handoff_documents: &[HandoffAgentDoc],
) -> Option<String> {
    if index.root_document.is_none() && index.nested.is_empty() && handoff_documents.is_empty() {
        return None;
    }
    let template = AgentsMdTemplate {
        workdir,
        root_document: index.root_document.clone(),
        root_truncated: index.root_truncated,
        nested: index.nested.clone(),
        extra_nested: index.extra_nested,
        handoff_documents: handoff_documents.to_vec(),
    };
    Some(
        template
            .render()
            .unwrap_or_else(|error| panic!("failed to render agents_md context: {error}")),
    )
}

pub async fn build_base_system_prompt(
    shared_terminal_commands: &[(String, String)],
    request_parameters: &Parameters,
    skills_dir: &std::path::Path,
    issue_backend: &crate::issue::IssueBackendInfo,
) -> Result<String, MayAgentError> {
    let mut commands = shared_terminal_commands
        .iter()
        .map(|(name, description)| BaseSystemCommand {
            name: name.clone(),
            description: description.clone(),
        })
        .collect::<Vec<_>>();
    commands.sort_by(|left, right| left.name.cmp(&right.name));

    let has_hosted_web_search = request_parameters.hosted_web_search();
    let has_hosted_web_fetch = request_parameters.hosted_web_fetch();
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

    let mut subagents: Vec<BaseSystemSubagent> = aither_agent::builtin_subagents()
        .into_iter()
        .map(|def| BaseSystemSubagent {
            id: def.id,
            description: def.description,
        })
        .collect();
    subagents.push(BaseSystemSubagent {
        id: "fork".to_string(),
        description: "Continues your own context on your model; use it for work that depends on what you have read and decided".to_string(),
    });
    subagents.sort_by(|left, right| left.id.cmp(&right.id));

    let template = BaseSystemTemplate {
        has_hosted_web_search,
        has_hosted_web_fetch,
        has_browser_command,
        has_commands: !commands.is_empty(),
        commands,
        has_skills: !skills.is_empty(),
        skills,
        has_subagents: !subagents.is_empty(),
        subagents,
        issue_backend_name: issue_backend.name,
        issue_repository: issue_backend.repository.clone(),
    };
    template.render().map_err(|error| {
        MayAgentError::Sandbox(format!("failed to render base system prompt: {error}"))
    })
}

const fn home_environment_kind_label(environment: &AgentEnvironment) -> &'static str {
    match environment {
        AgentEnvironment::Local => "local",
        AgentEnvironment::Container { .. } => "container",
    }
}

/// The working directory the terminal starts in, as the shell sees it.
fn working_directory(environment: &AgentEnvironment, session_paths: &SessionPaths) -> String {
    match environment {
        AgentEnvironment::Local => session_paths.root().display().to_string(),
        AgentEnvironment::Container { .. } => {
            crate::container::lifecycle::CONTAINER_WORKSPACE.to_string()
        }
    }
}

/// What `~`/`$HOME` expands to inside the shell.
const fn home_note(environment: &AgentEnvironment) -> &'static str {
    match environment {
        AgentEnvironment::Local => {
            "`~` and `$HOME` expand to the current working directory in every terminal mode, unsafe escalations included; use absolute paths for host locations."
        }
        AgentEnvironment::Container { .. } => {
            "`~` and `$HOME` expand to the current working directory; storage outside /workspace is ephemeral to the container."
        }
    }
}

/// Layout of the profile home environment, empty where it has no meaningful
/// anchor inside the session (containers mount its pieces under /root/.may).
fn home_layout(environment: &AgentEnvironment) -> Vec<String> {
    match environment {
        AgentEnvironment::Local => vec![
            "/sessions - per-session sandboxes under this home environment".to_string(),
            "/skills, /subagents - profile-scoped, mounted read-only inside the session"
                .to_string(),
            "workspace.toml - May-managed workspace grants for this home environment".to_string(),
        ],
        AgentEnvironment::Container { .. } => Vec::new(),
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
            "This home environment is the user's real machine: prefer inspection over mutation outside the sandbox and approved workdirs, and request approval before destructive or irreversible commands.".to_string(),
        ],
        AgentEnvironment::Container { .. } => vec![
            "This home environment is a managed container isolated from the user's host machine; the filesystem is writable unless a command says otherwise, but avoid needless destructive commands.".to_string(),
            "Container-global state may be ephemeral; persist anything important into durable session directories (data/, outputs/, files/, artifacts/). Host-specific and desktop automation tools do not exist inside the container unless verified.".to_string(),
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

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use aither_core::llm::model::Parameters;

    use super::{
        RuntimeSystemContext, build_agents_md_context, build_base_system_prompt,
        build_memory_context_content, tag,
    };

    fn test_memory_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().expect("temp dir should be created");
        let path = dir.path().join("MEMORY.md");
        (dir, path)
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
        let (_dir, memory_path) = test_memory_file();
        let runtime_context = RuntimeSystemContext::new(memory_path, "remember this".to_string());
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
        let (_dir, memory_path) = test_memory_file();
        std::fs::write(&memory_path, "before").expect("write initial memory");

        let mut runtime_context =
            RuntimeSystemContext::new(memory_path.clone(), "before".to_string());
        let mut context = aither_agent::Context::default();
        runtime_context.reconcile(&mut context);

        std::fs::write(&memory_path, "after").expect("update memory");
        tokio_test::block_on(runtime_context.sync_memory_content(&mut context));
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
        assert_eq!(
            alerts[0],
            format!(
                "MEMORY.md changed since this conversation started. The memory block above is the previous version; apply this diff mentally (the context will fold it in later):\n<memory_diff>\n{}</memory_diff>",
                similar::TextDiff::from_lines(
                    build_memory_context_content("before"),
                    build_memory_context_content("after"),
                )
                .unified_diff()
                .context_radius(2)
            )
        );

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
        let (_dir, memory_path) = test_memory_file();
        let runtime_context = RuntimeSystemContext::new(memory_path, "remember this".to_string())
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
        let (_dir, memory_path) = test_memory_file();
        let mut runtime_context = RuntimeSystemContext::new(memory_path, String::new())
            .with_terminal_runtime("Model: demo-model".to_string());
        let mut context = aither_agent::Context::default();
        context.insert_system_text(tag::WORKSPACE, "stale workspace");
        context.insert_system_text(tag::CONTAINER_ENVIRONMENT_CONTEXT, "stale container");
        runtime_context.reconcile(&mut context);
        let without_workdirs_fingerprint = context.stable_prefix_fingerprint();

        runtime_context = runtime_context
            .with_workspace_workdirs("Approved workspace directories:\n- /foo".to_string());
        runtime_context.reconcile(&mut context);

        assert!(!context.has_system_block(tag::WORKSPACE));
        assert!(!context.has_system_block(tag::CONTAINER_ENVIRONMENT_CONTEXT));
        assert!(context.has_system_block(tag::WORKSPACE_WORKDIRS));
        assert_ne!(
            without_workdirs_fingerprint,
            context.stable_prefix_fingerprint(),
            "adding workdirs block must change the fingerprint",
        );
    }

    #[test]
    fn agents_md_block_is_reconciled_and_pruned() {
        let (_dir, memory_path) = test_memory_file();
        let mut runtime_context = RuntimeSystemContext::new(memory_path, String::new());
        let mut context = aither_agent::Context::default();
        runtime_context.reconcile(&mut context);
        assert!(!context.has_system_block(tag::AGENTS_MD));

        runtime_context = runtime_context.with_agents_md(Some("project rules".to_string()));
        runtime_context.reconcile(&mut context);
        assert_eq!(
            context.system_blocks().get(tag::AGENTS_MD),
            Some(&"project rules".to_string())
        );

        runtime_context = runtime_context.with_agents_md(None);
        runtime_context.reconcile(&mut context);
        assert!(!context.has_system_block(tag::AGENTS_MD));
    }

    #[test]
    fn agents_md_context_renders_document_and_nested_list() {
        let index = crate::agent::agents_md::AgentsIndex {
            root_document: Some("do the thing".to_string()),
            root_truncated: false,
            nested: vec!["crates/foo/AGENTS.md".to_string()],
            extra_nested: 2,
        };
        let block = build_agents_md_context(&index, Some("/launch".to_string()), &[])
            .expect("block renders");
        assert!(block.contains("/launch"), "{block}");
        assert!(block.contains("do the thing"), "{block}");
        assert!(block.contains("crates/foo/AGENTS.md"), "{block}");
        assert!(block.contains("2 more"), "{block}");

        let empty = crate::agent::agents_md::AgentsIndex::default();
        assert!(build_agents_md_context(&empty, None, &[]).is_none());
    }

    fn render_base_system_prompt(
        commands: &[(String, String)],
        request_parameters: &Parameters,
    ) -> String {
        render_base_system_prompt_with_backend(
            commands,
            request_parameters,
            &crate::issue::IssueBackendInfo {
                name: "may",
                repository: None,
            },
        )
    }

    fn render_base_system_prompt_with_backend(
        commands: &[(String, String)],
        request_parameters: &Parameters,
        issue_backend: &crate::issue::IssueBackendInfo,
    ) -> String {
        let dir = tempdir().expect("temp dir should be created");
        tokio_test::block_on(build_base_system_prompt(
            commands,
            request_parameters,
            dir.path(),
            issue_backend,
        ))
        .expect("base system prompt should render")
    }

    #[test]
    fn base_system_prompt_names_the_issue_backend() {
        let prompt = render_base_system_prompt(&[], &Parameters::default());
        assert!(prompt.contains("`issue` command (may backend)"), "{prompt}");

        let prompt = render_base_system_prompt_with_backend(
            &[],
            &Parameters::default(),
            &crate::issue::IssueBackendInfo {
                name: "github",
                repository: Some("May-Agent/may".to_string()),
            },
        );
        assert!(
            prompt.contains("`issue` command (github backend: May-Agent/may)"),
            "{prompt}"
        );
    }

    #[test]
    fn base_system_prompt_has_no_native_tool_listing() {
        // Native tool usage lives in the injected tool schemas; the system
        // prompt only names CLI commands, skills and subagents.
        let prompt = render_base_system_prompt(&[], &Parameters::default());
        assert!(!prompt.contains("<native_tools>"), "{prompt}");
    }

    #[test]
    fn base_system_prompt_advertises_hosted_web_search_and_fetch() {
        let commands = vec![("browser".to_string(), "Drive the browser".to_string())];
        let mut parameters = Parameters::default();
        parameters.native_tools.claude.web_search = true;
        parameters.native_tools.claude.web_fetch = true;

        let prompt = render_base_system_prompt(&commands, &parameters);

        assert!(prompt.contains("Use `web_search` to look up current information"));
        assert!(prompt.contains("Use `web_fetch` to read a known URL"));
        assert!(prompt.contains(
            "The `browser` command is for interacting with pages or reaching what search and fetch cannot"
        ));
    }

    #[test]
    fn base_system_prompt_keeps_webfetch_when_hosted_fetch_off() {
        let commands = vec![
            ("browser".to_string(), "Drive the browser".to_string()),
            ("webfetch".to_string(), "Fetch a URL".to_string()),
        ];

        let prompt = render_base_system_prompt(&commands, &Parameters::default());

        assert!(!prompt.contains("web_search"), "{prompt}");
        assert!(!prompt.contains("look up current information"));
        let commands_section = prompt
            // The tool architecture mentions `<commands>` inline, so match the
            // tag on its own line.
            .split("\n<commands>\n")
            .nth(1)
            .and_then(|rest| rest.split("</commands>").next())
            .expect("commands section should render");
        assert!(
            !commands_section.contains("websearch"),
            "{commands_section}"
        );
        assert!(
            commands_section.contains("- webfetch: Fetch a URL"),
            "{commands_section}"
        );
    }
}
