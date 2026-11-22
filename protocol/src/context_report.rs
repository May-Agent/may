//! Estimated context-window usage for the `/context` command.
//!
//! The report mirrors Claude Code's `/context` panel: a `used/window`
//! headline plus a per-category estimate of what the next request would
//! carry. Everything is measured from the live [`aither_agent::Context`]
//! — instruction and external system blocks, the skill/subagent index
//! sections inside `capabilities`, the command table, staged handoff,
//! reminders and the recent
//! conversation — plus the `terminal` tool definition providers receive
//! alongside the messages. Numbers are estimates at ~4 bytes per token,
//! the same heuristic the agent uses for its own context budgeting.

use aither_agent::{ContextPolicy, ContextWindowMetrics, ContextWindowPhase};
use aither_core::llm::tool::ToolDefinition;

use crate::context_tag::tag;

/// A context-usage category, matching the rows Claude Code shows for
/// `/context` with `Commands` added for May's terminal command index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextCategory {
    /// Persistent system blocks (identity, environment, workspace, the
    /// prose of `capabilities`, …).
    SystemPrompt,
    /// The native `terminal` tool definition sent with every request.
    SystemTools,
    /// Built-in IPC commands listed in `<commands>`.
    Commands,
    /// MCP-provided commands listed in `<commands>`.
    McpTools,
    /// The `<subagents>` index of available custom agents.
    CustomAgents,
    /// Memory blocks (`memory` + `profile_memories`).
    Memory,
    /// The `<skills>` index plus activated skill instructions.
    Skills,
    /// Recent conversation, handoff document and non-skill reminders.
    Messages,
}

impl ContextCategory {
    /// Row label shown in the report.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SystemPrompt => "System prompt",
            Self::SystemTools => "System tools",
            Self::Commands => "Commands",
            Self::McpTools => "MCP tools",
            Self::CustomAgents => "Custom agents",
            Self::Memory => "Memory",
            Self::Skills => "Skills",
            Self::Messages => "Messages",
        }
    }
}

/// One usage row: estimated tokens for a category plus an optional item
/// count detail (e.g. `12 tools`, `84 messages`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextCategoryRow {
    /// Which category this row describes.
    pub kind: ContextCategory,
    /// Estimated token usage.
    pub tokens: usize,
    /// Item-count detail, when the category is a collection of things.
    pub detail: Option<String>,
}

/// Estimated composition of the context window the next request would use.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextReport {
    /// Model identifier shown in the header.
    pub model_id: String,
    /// Provider identifier shown in the header.
    pub provider_id: String,
    /// Usable window the agent budgets against.
    pub window_tokens: usize,
    /// Estimated tokens the next request would send, messages plus the
    /// `terminal` tool definition.
    pub used_tokens: usize,
    /// Lifecycle phase the agent reports for the current window.
    pub phase: ContextWindowPhase,
    /// Per-category estimates in display order.
    pub categories: Vec<ContextCategoryRow>,
    /// Estimated tokens below the auto-compaction trigger still unused.
    pub free_tokens: usize,
    /// Window share kept in reserve above the auto-compaction trigger.
    pub autocompact_buffer_tokens: usize,
    /// Tokens at which a background compaction starts.
    pub compact_start_tokens: usize,
    /// Tokens at which the agent pauses to finish the compaction:
    /// `min(window - handoff_reserve, compact_ceiling)`.
    pub compact_tokens: usize,
}

/// Result of splitting the `capabilities` block, or the command table, into
/// its marked sections.
struct CapabilitiesCarve {
    /// Everything outside the `<commands>`/`<skills>`/`<subagents>` spans.
    architecture_chars: usize,
    /// `<commands>` span bytes attributed to built-in commands.
    command_chars: usize,
    /// `- name: …` item lines in `<commands>` that are not MCP tools.
    command_count: usize,
    /// `<commands>` span bytes attributed to MCP tools.
    mcp_chars: usize,
    /// `- name: …` item lines in `<commands>` that are MCP tools.
    mcp_count: usize,
    /// `<skills>` span bytes.
    skill_chars: usize,
    /// Item count in `<skills>`.
    skill_count: usize,
    /// `<subagents>` span bytes.
    subagent_chars: usize,
    /// Item count in `<subagents>`.
    subagent_count: usize,
}

/// The tools an agent was built with.
#[derive(Debug)]
pub struct ToolInventory<'a> {
    /// The commands registered by name; every other row of the command table
    /// is an MCP connection's tool.
    pub named_commands: &'a [String],
    /// The native tool definitions the agent sends with every request.
    pub native_tools: &'a [ToolDefinition],
}

impl ContextReport {
    /// Assemble a report from the agent's live context and a
    /// [`ContextWindowMetrics`] measured on the same state.
    ///
    /// # Panics
    /// Panics when the metrics contradict the context they were measured on.
    pub fn assemble(
        model_id: &str,
        provider_id: &str,
        tools: &ToolInventory<'_>,
        context: &aither_agent::Context,
        metrics: &ContextWindowMetrics,
        phase: ContextWindowPhase,
        policy: &ContextPolicy,
    ) -> Self {
        let ToolInventory {
            named_commands,
            native_tools,
        } = tools;
        let mut system_prompt_chars = 0usize;
        let mut memory_chars = 0usize;
        let mut command_chars = 0usize;
        let mut mcp_chars = 0usize;
        let mut command_count = 0usize;
        let mut mcp_count = 0usize;
        let mut skill_chars = 0usize;
        let mut skill_count = 0usize;
        let mut subagent_chars = 0usize;
        let mut subagent_count = 0usize;

        for (slot, block) in context.external().iter() {
            if *slot == tag::MEMORY || *slot == tag::PROFILE_MEMORIES {
                memory_chars += block.content().len();
            } else {
                system_prompt_chars += block.content().len();
            }
        }
        for (slot, content) in context.instructions().iter() {
            match slot {
                slot if *slot == tag::CAPABILITIES || *slot == tag::COMMANDS => {
                    let carved = carve_capabilities(content, named_commands);
                    system_prompt_chars += carved.architecture_chars;
                    command_chars += carved.command_chars;
                    mcp_chars += carved.mcp_chars;
                    command_count += carved.command_count;
                    mcp_count += carved.mcp_count;
                    skill_chars += carved.skill_chars;
                    skill_count += carved.skill_count;
                    subagent_chars += carved.subagent_chars;
                    subagent_count += carved.subagent_count;
                }
                _ => system_prompt_chars += content.len(),
            }
        }

        let messages = context.recent();
        let message_chars = messages
            .iter()
            .map(|message| message.content().len())
            .sum::<usize>()
            + context.handoff().map_or(0, |handoff| handoff.text().len());

        let categories = [
            (ContextCategory::SystemPrompt, system_prompt_chars, None),
            (
                ContextCategory::SystemTools,
                native_tools.iter().map(tool_definition_chars).sum(),
                Some(format!("{} tools", native_tools.len())),
            ),
            (
                ContextCategory::Commands,
                command_chars,
                Some(format!("{command_count} commands")),
            ),
            (
                ContextCategory::McpTools,
                mcp_chars,
                Some(format!("{mcp_count} tools")),
            ),
            (
                ContextCategory::CustomAgents,
                subagent_chars,
                Some(format!("{subagent_count} agents")),
            ),
            (ContextCategory::Memory, memory_chars, None),
            (
                ContextCategory::Skills,
                skill_chars,
                Some(format!("{skill_count} skills")),
            ),
            (
                ContextCategory::Messages,
                message_chars,
                Some(format!("{} messages", messages.len())),
            ),
        ]
        .into_iter()
        .map(|(kind, chars, detail)| ContextCategoryRow {
            kind,
            tokens: chars / 4,
            detail,
        })
        .collect::<Vec<_>>();

        let used_tokens = categories.iter().map(|row| row.tokens).sum();
        let window = usize::try_from(metrics.context_window).expect("a u32 window fits usize");
        // Everything above the auto-compact point is the autocompact buffer
        // from the user's perspective.
        let compact_tokens = policy.pause_point(metrics.context_window);
        let autocompact_buffer = window.saturating_sub(compact_tokens);

        Self {
            model_id: model_id.to_string(),
            provider_id: provider_id.to_string(),
            window_tokens: window,
            used_tokens,
            phase,
            categories,
            free_tokens: window.saturating_sub(used_tokens + autocompact_buffer),
            autocompact_buffer_tokens: autocompact_buffer,
            compact_start_tokens: policy.start_point(metrics.context_window),
            compact_tokens,
        }
    }
}

/// Byte bounds of a `<name>…</name>` span inside the rendered base-system
/// block, tags included.
fn section_span(content: &str, section: &str) -> Option<(usize, usize)> {
    let open = format!("<{section}>");
    let close = format!("</{section}>");
    let start = content.find(&open)?;
    let end = content[start..]
        .find(&close)
        .map(|index| start + index + close.len())?;
    Some((start, end))
}

/// Item lines (`- name: description`) inside a section span.
fn section_items(section: &str) -> impl Iterator<Item = &str> {
    section.lines().filter_map(|line| line.strip_prefix("- "))
}

/// Command name from a `- name: description` index line.
fn command_line_name(item: &str) -> &str {
    item.split_once(": ").map_or(item, |(name, _)| name)
}

/// Split a rendered block into its prose and its marked index sections,
/// attributing `<commands>` lines to commands registered by name vs MCP
/// tools.
fn carve_capabilities(content: &str, named_commands: &[String]) -> CapabilitiesCarve {
    let mut carved = CapabilitiesCarve {
        architecture_chars: content.len(),
        command_chars: 0,
        command_count: 0,
        mcp_chars: 0,
        mcp_count: 0,
        skill_chars: 0,
        skill_count: 0,
        subagent_chars: 0,
        subagent_count: 0,
    };

    if let Some((start, end)) = section_span(content, "commands") {
        let section = &content[start..end];
        // Section wrapper (tags, blank lines) counts toward the built-in
        // index; only MCP item lines move to the MCP bucket.
        carved.command_chars = section.len();
        for item in section_items(section) {
            if named_commands
                .iter()
                .any(|name| name == command_line_name(item))
            {
                carved.command_count += 1;
            } else {
                carved.mcp_chars += item.len();
                carved.mcp_count += 1;
            }
        }
        carved.command_chars -= carved.mcp_chars;
        carved.architecture_chars -= section.len();
    }
    if let Some((start, end)) = section_span(content, "skills") {
        let section = &content[start..end];
        carved.skill_chars = section.len();
        carved.skill_count = section_items(section).count();
        carved.architecture_chars -= section.len();
    }
    if let Some((start, end)) = section_span(content, "subagents") {
        let section = &content[start..end];
        carved.subagent_chars = section.len();
        carved.subagent_count = section_items(section).count();
        carved.architecture_chars -= section.len();
    }

    carved
}

/// Estimated wire size of one native tool definition: the
/// `{name, description, parameters}` object providers receive.
fn tool_definition_chars(definition: &ToolDefinition) -> usize {
    serde_json::json!({
        "name": definition.name(),
        "description": definition.description(),
        "parameters": definition.arguments_openai_schema(),
    })
    .to_string()
    .len()
}
