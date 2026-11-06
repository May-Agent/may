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

use aither_agent::{
    ContextPolicy, ContextWindowMetrics, ContextWindowPhase, Handoff, InstructionSlot,
};
use aither_sandbox::TerminalArgs;

use super::runtime_context::tag;

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
    /// Tokens at which the agent pauses to finish the compaction.
    pub compact_pause_tokens: usize,
    /// Tokens at which the conversation is auto-compacted:
    /// `min(window * compact_at, compact_ceiling)`.
    pub compact_tokens: usize,
}

/// Result of splitting the `capabilities` block, or the command table, into
/// its marked sections.
struct CapabilitiesCarve {
    /// Everything outside the `<native_tools>`/`<commands>`/`<skills>`/
    /// `<subagents>` spans.
    architecture_chars: usize,
    /// `<native_tools>` span bytes.
    native_tool_chars: usize,
    /// Item count in `<native_tools>`.
    native_tool_count: usize,
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

impl ContextReport {
    /// Assemble a report from the agent's live context and a
    /// [`ContextWindowMetrics`] measured on the same state. `named_commands`
    /// are the commands registered by name; every other row of the command
    /// table is an MCP connection's tool.
    pub(crate) fn assemble(
        model_id: &str,
        provider_id: &str,
        named_commands: &[String],
        context: &aither_agent::Context,
        metrics: &ContextWindowMetrics,
        phase: ContextWindowPhase,
        policy: &ContextPolicy,
    ) -> Self {
        let mut system_prompt_chars = 0usize;
        let mut memory_chars = 0usize;
        let mut native_tool_chars = 0usize;
        let mut native_tool_count = 0usize;
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
                slot if *slot == tag::CAPABILITIES || *slot == InstructionSlot::COMMANDS => {
                    let carved = carve_capabilities(content, named_commands);
                    system_prompt_chars += carved.architecture_chars;
                    native_tool_chars += carved.native_tool_chars;
                    native_tool_count += carved.native_tool_count;
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
            + context.handoff().map_or(0, |handoff| match handoff {
                Handoff::Text(text) => text.len(),
                Handoff::Document(document) => document.render_markdown().len(),
            });

        let categories = [
            (ContextCategory::SystemPrompt, system_prompt_chars, None),
            (
                ContextCategory::SystemTools,
                terminal_tool_definition_chars() + native_tool_chars,
                Some(format!("{native_tool_count} tools")),
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
        let compact_tokens = policy.compact_point(metrics.context_window);
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
            compact_pause_tokens: policy.pause_point(metrics.context_window),
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
    let end = content.find(&close).map(|index| index + close.len())?;
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
        native_tool_chars: 0,
        native_tool_count: 0,
        command_chars: 0,
        command_count: 0,
        mcp_chars: 0,
        mcp_count: 0,
        skill_chars: 0,
        skill_count: 0,
        subagent_chars: 0,
        subagent_count: 0,
    };

    if let Some((start, end)) = section_span(content, "native_tools") {
        let section = &content[start..end];
        carved.native_tool_chars = section.len();
        carved.native_tool_count = section_items(section).count();
        carved.architecture_chars -= section.len();
    }
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

/// Estimated wire size of the native `terminal` tool definition — the only
/// tool providers see, since every other capability is an IPC command.
fn terminal_tool_definition_chars() -> usize {
    let schema = schemars::schema_for!(TerminalArgs).to_value();
    let description = schema
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    // Providers receive a {name, description, parameters} object; the
    // description rides both at top level and inside the schema.
    serde_json::json!({
        "name": "terminal",
        "description": description,
        "parameters": schema,
    })
    .to_string()
    .len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aither_core::llm::Message;

    fn base_system_block() -> &'static str {
        "<tool_architecture>\n<native_tools>\n- terminal: Execute terminal commands.\n- terminal_kill: Terminate a background task.\n</native_tools>\n</tool_architecture>\n\n\
         <skills>\n\n- pdf: read `skills/pdf/SKILL.md` when the request matches this skill. Handle PDFs.\n\n</skills>\n\n\
         <subagents>\nInvoke with `subagent`.\n\n- researcher: Research things.\n\n</subagents>"
    }

    /// The command table: `ask` is registered by name, the rest are MCP
    /// tools.
    const COMMAND_TABLE: &str = "<commands>\n- ask: Ask a fast model\n- github_pr: Open a PR\n- jira_view: View an issue\n</commands>";

    fn test_context() -> aither_agent::Context {
        let mut context = aither_agent::Context::with_layout(tag::layout());
        let instructions = context.instructions_mut();
        instructions.insert(&tag::CAPABILITIES, base_system_block());
        instructions.insert(&InstructionSlot::COMMANDS, COMMAND_TABLE);
        instructions.insert(&tag::ENVIRONMENT, "sandbox=/tmp cwd=/tmp");
        context.external_mut().insert(
            &tag::MEMORY,
            aither_agent::ExternalContent::data("MEMORY.md", "remember this"),
        );
        context.push(Message::user("hello"));
        context.push(Message::assistant("hi there"));
        context
    }

    fn test_metrics() -> ContextWindowMetrics {
        ContextWindowMetrics {
            usage_fraction: 0.0,
            context_window: 200_000,
            estimated_tokens: 0,
            chars_per_token: 4.0,
            system_block_count: 0,
            recent_message_count: 0,
            recent_system_message_count: 0,
            has_handoff: false,
            has_staged_system_updates: false,
            prompt_cache_epoch: 0,
        }
    }

    fn category_tokens(report: &ContextReport, kind: ContextCategory) -> usize {
        report
            .categories
            .iter()
            .find(|row| row.kind == kind)
            .map_or(0, |row| row.tokens)
    }

    #[test]
    fn report_partitions_base_system_and_counts_items() {
        let report = ContextReport::assemble(
            "gpt-5",
            "openai",
            &["ask".to_string()],
            &test_context(),
            &test_metrics(),
            ContextWindowPhase::Stable,
            &ContextPolicy::default(),
        );

        let commands = report
            .categories
            .iter()
            .find(|row| row.kind == ContextCategory::Commands)
            .expect("commands row");
        assert_eq!(commands.detail.as_deref(), Some("1 commands"));

        let mcp = report
            .categories
            .iter()
            .find(|row| row.kind == ContextCategory::McpTools)
            .expect("mcp row");
        assert_eq!(mcp.detail.as_deref(), Some("2 tools"));
        assert!(mcp.tokens > 0);

        let skills = report
            .categories
            .iter()
            .find(|row| row.kind == ContextCategory::Skills)
            .expect("skills row");
        assert_eq!(skills.detail.as_deref(), Some("1 skills"));

        let agents = report
            .categories
            .iter()
            .find(|row| row.kind == ContextCategory::CustomAgents)
            .expect("agents row");
        assert_eq!(agents.detail.as_deref(), Some("1 agents"));

        // The `<native_tools>` section lands on System tools alongside the
        // wire `terminal` definition estimate.
        let native_span = &base_system_block()[base_system_block().find("<native_tools>").unwrap()
            ..base_system_block().find("</native_tools>").unwrap() + "</native_tools>".len()];
        let system_tools = report
            .categories
            .iter()
            .find(|row| row.kind == ContextCategory::SystemTools)
            .expect("system tools row");
        assert_eq!(system_tools.detail.as_deref(), Some("2 tools"));
        assert_eq!(
            system_tools.tokens,
            (terminal_tool_definition_chars() + native_span.len()) / 4
        );

        let skills_span = &base_system_block()[base_system_block().find("<skills>").unwrap()
            ..base_system_block().find("</skills>").unwrap() + "</skills>".len()];
        assert_eq!(
            category_tokens(&report, ContextCategory::Skills),
            skills_span.len() / 4
        );
        assert_eq!(
            category_tokens(&report, ContextCategory::Messages),
            ("hello".len() + "hi there".len()) / 4
        );
        assert_eq!(
            category_tokens(&report, ContextCategory::Memory),
            "remember this".len() / 4
        );
    }

    #[test]
    fn used_tokens_cover_every_category_and_leave_free_space() {
        let report = ContextReport::assemble(
            "gpt-5",
            "openai",
            &[],
            &test_context(),
            &test_metrics(),
            ContextWindowPhase::Stable,
            &ContextPolicy::default(),
        );
        let summed: usize = report.categories.iter().map(|row| row.tokens).sum();
        assert_eq!(summed, report.used_tokens);
        assert_eq!(
            report.used_tokens + report.free_tokens + report.autocompact_buffer_tokens,
            report.window_tokens
        );
        // Default policy on a 200k window: 0.8 of it, below the 400k ceiling.
        assert_eq!(report.compact_tokens, 160_000);
        assert_eq!(report.autocompact_buffer_tokens, 40_000);
        assert_eq!(report.compact_start_tokens, 110_000);
        assert_eq!(report.compact_pause_tokens, 155_000);
    }

    #[test]
    fn a_million_token_window_compacts_at_the_ceiling() {
        let mut metrics = test_metrics();
        metrics.context_window = 1_000_000;
        let report = ContextReport::assemble(
            "gpt-5",
            "openai",
            &[],
            &test_context(),
            &metrics,
            ContextWindowPhase::Stable,
            &ContextPolicy::default(),
        );
        assert_eq!(report.compact_tokens, 400_000);
        assert_eq!(report.compact_start_tokens, 350_000);
        assert_eq!(report.compact_pause_tokens, 395_000);
        assert_eq!(report.autocompact_buffer_tokens, 600_000);
    }

    #[test]
    fn the_report_follows_the_configured_policy() {
        let policy = crate::config::ContextConfig {
            compact_ceiling: 100_000,
            start_distance: 20_000,
            safe_buffer: 1_000,
            ..crate::config::ContextConfig::default()
        }
        .policy();
        let report = ContextReport::assemble(
            "gpt-5",
            "openai",
            &[],
            &test_context(),
            &test_metrics(),
            ContextWindowPhase::Compacting,
            &policy,
        );
        assert_eq!(report.compact_tokens, 100_000);
        assert_eq!(report.compact_start_tokens, 80_000);
        assert_eq!(report.compact_pause_tokens, 99_000);
        assert_eq!(report.phase, ContextWindowPhase::Compacting);
    }
}
