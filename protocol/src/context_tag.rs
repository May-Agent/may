//! The prompt-section tags a context layout orders, shared by the report.

/// The system-prompt slots May's prompt sections render into.
pub mod tag {
    use aither_agent::{ExternalSlot, InstructionSlot, SystemLayout};

    /// Who the agent is (`templates/prompt/identity.md`).
    pub const IDENTITY: InstructionSlot = InstructionSlot::new("identity");
    /// The profile's persona text.
    pub const PROFILE_PERSONA: InstructionSlot = InstructionSlot::new("profile_persona");
    /// The model, machine and environment the session runs on.
    pub const ENVIRONMENT: InstructionSlot = InstructionSlot::new("environment");
    /// The session's directories, task and bindings.
    pub const SESSION: InstructionSlot = InstructionSlot::new("session");
    /// The session's private repository clones.
    pub const REPO_CLONES: InstructionSlot = InstructionSlot::new("repo_clones");
    /// How the `terminal` tool runs commands.
    pub const TERMINAL: InstructionSlot = InstructionSlot::new("terminal");
    /// How to work efficiently: concurrency and search.
    pub const EFFICIENCY: InstructionSlot = InstructionSlot::new("efficiency");
    /// The commands, skills and subagents the session can call.
    pub const CAPABILITIES: InstructionSlot = InstructionSlot::new("capabilities");
    /// Whether browser automation is available and how it is driven.
    pub const BROWSER: InstructionSlot = InstructionSlot::new("browser");
    /// How work is recorded: issues, tasks and PRs.
    pub const WORK: InstructionSlot = InstructionSlot::new("work");
    /// The AGENTS.md rules text rendered inline.
    pub const PROJECT_RULES: InstructionSlot = InstructionSlot::new("project_rules");
    /// The plan-mode phase and its constraints.
    pub const PLAN_MODE: InstructionSlot = InstructionSlot::new("plan_mode");
    /// What the sandbox allows and how `unsafe` escalation works.
    pub const PERMISSIONS: InstructionSlot = InstructionSlot::new("permissions");
    /// The mechanisms the harness runs around the agent.
    pub const HARNESS: InstructionSlot = InstructionSlot::new("harness");
    /// How alerts and evidence are treated and reported.
    pub const EVIDENCE: InstructionSlot = InstructionSlot::new("evidence");
    /// The knowledge cutoff and timezone handling.
    pub const TIME: InstructionSlot = InstructionSlot::new("knowledge_and_time");
    /// The prepared tools of the home environment.
    pub const PREPARED_TOOLS: InstructionSlot = InstructionSlot::new("prepared_tools");
    /// The table of the commands a script can call.
    pub const COMMANDS: InstructionSlot = InstructionSlot::new("commands");
    /// The languages the session talks and works in.
    pub const LANGUAGES: InstructionSlot = InstructionSlot::new("languages");

    /// The session's extracted memories.
    pub const MEMORY: ExternalSlot = ExternalSlot::new("memory");
    /// The profile's accumulated memories.
    pub const PROFILE_MEMORIES: ExternalSlot = ExternalSlot::new("profile_memories");
    /// The user's global AGENTS.md.
    pub const GLOBAL_AGENTS_MD: ExternalSlot = ExternalSlot::new("global_agents_md");
    /// The launch directory's AGENTS.md.
    pub const AGENTS_MD: ExternalSlot = ExternalSlot::new("agents_md");
    /// The AGENTS.md carried by a handoff.
    pub const HANDOFF_AGENTS_MD: ExternalSlot = ExternalSlot::new("handoff_agents_md");
    /// The user-approved workspace directories.
    pub const WORKSPACE_WORKDIRS: ExternalSlot = ExternalSlot::new("workspace_workdirs");

    /// The instruction slots May writes, in prompt order.
    pub const MAY_INSTRUCTIONS: [InstructionSlot; 19] = [
        IDENTITY,
        HARNESS,
        EVIDENCE,
        PROFILE_PERSONA,
        ENVIRONMENT,
        TERMINAL,
        PREPARED_TOOLS,
        EFFICIENCY,
        CAPABILITIES,
        COMMANDS,
        BROWSER,
        WORK,
        PERMISSIONS,
        TIME,
        LANGUAGES,
        SESSION,
        REPO_CLONES,
        PLAN_MODE,
        PROJECT_RULES,
    ];

    /// The system prompt's order, from most shared to least shared.
    ///
    /// Sessions share the longest cached prefix. Text every session of a
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
                PREPARED_TOOLS,
                EFFICIENCY,
                CAPABILITIES,
                COMMANDS,
                BROWSER,
                WORK,
                PERMISSIONS,
                TIME,
                LANGUAGES,
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
