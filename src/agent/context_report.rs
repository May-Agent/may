//! Estimated context-window usage for the `/context` command.
//!
//! The report mirrors Claude Code's `/context` panel: a `used/window`
//! headline plus a per-category estimate of what the next request would
//! carry. Everything is measured from the live [`aither_agent::Context`]
//! — persistent system blocks, the command/skill/subagent index sections
//! inside `base_system`, staged handoff and the recent conversation — plus the `terminal` tool definition providers receive
//! alongside the messages. Numbers are estimates at ~4 bytes per token,
//! the same heuristic the agent uses for its own context budgeting.

use aither_agent::{ContextPolicy, ContextWindowMetrics, ContextWindowPhase};
use aither_sandbox::TerminalArgs;

use super::runtime_context::tag;

/// Tag aither-agent uses for the stable base-system block.
const BASE_SYSTEM_TAG: &str = "base_system";

/// A context-usage category, matching the rows Claude Code shows for
/// `/context` with `Commands` added for May's terminal command index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextCategory {
    /// Persistent system blocks (persona, runtime, workspace, the tool
    /// architecture portion of `base_system`, …).
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
    /// The `<skills>` index.
    Skills,
    /// Recent conversation and the handoff document.
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
#[derive(Debug, Clone)]
pub struct ContextCategoryRow {
    /// Which category this row describes.
    pub kind: ContextCategory,
    /// Estimated token usage.
    pub tokens: usize,
    /// Item-count detail, when the category is a collection of things.
    pub detail: Option<String>,
}

/// Estimated composition of the context window the next request would use.
#[derive(Debug, Clone)]
pub struct ContextReport {
    /// Model identifier shown in the header.
    pub model_id: String,
    /// Provider identifier shown in the header.
    pub provider_id: String,
    /// Usable window the agent budgets against: the model's context minus
    /// what the response and the tool surface need.
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
    /// Usage fraction of `window_tokens` at which auto-compaction starts.
    pub autocompact_trigger: f32,
}

/// Result of splitting the `base_system` block into its marked sections.
struct BaseSystemCarve {
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
    /// [`ContextWindowMetrics`] measured on the same state.
    pub(crate) fn assemble(
        model_id: &str,
        provider_id: &str,
        mcp_command_names: &[String],
        context: &aither_agent::Context,
        metrics: &ContextWindowMetrics,
        phase: ContextWindowPhase,
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

        for (block_tag, content) in context.system_blocks() {
            match block_tag.as_str() {
                BASE_SYSTEM_TAG => {
                    let carved = carve_base_system(content, mcp_command_names);
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
                tag::MEMORY | tag::PROFILE_MEMORIES => memory_chars += content.len(),
                _ => system_prompt_chars += content.len(),
            }
        }

        let messages = context.recent();
        let message_chars = messages
            .iter()
            .map(|message| message.content().len())
            .sum::<usize>()
            + context
                .handoff()
                .map_or(0, |handoff| handoff.render().len());

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
        let window = metrics.context_window as usize;
        // May keeps aither's default context policy: a compaction is forked
        // at `compact_at`, so everything above it is the autocompact buffer
        // from the user's perspective. `trigger` is a fraction in [0, 1];
        // flooring the product is the intended bucket.
        let trigger = ContextPolicy::default().compact_at;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let autocompact_buffer =
            window.saturating_sub((f64::from(trigger) * window as f64) as usize);

        Self {
            model_id: model_id.to_string(),
            provider_id: provider_id.to_string(),
            window_tokens: window,
            used_tokens,
            phase,
            categories,
            free_tokens: window.saturating_sub(used_tokens + autocompact_buffer),
            autocompact_buffer_tokens: autocompact_buffer,
            autocompact_trigger: trigger,
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

/// Split the rendered `base_system` block into architecture text and its
/// marked index sections, attributing `<commands>` lines to built-in
/// commands vs MCP tools by name.
fn carve_base_system(content: &str, mcp_command_names: &[String]) -> BaseSystemCarve {
    let mut carved = BaseSystemCarve {
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
            if mcp_command_names
                .iter()
                .any(|name| name == command_line_name(item))
            {
                carved.mcp_chars += item.len();
                carved.mcp_count += 1;
            } else {
                carved.command_count += 1;
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
    use aither_core::llm::Message;

    use super::*;

    fn base_system_block() -> &'static str {
        "<tool_architecture>\n<native_tools>\n- terminal: Execute terminal commands.\n- terminal_kill: Terminate a background task.\n</native_tools>\n</tool_architecture>\n\n\
         <commands>\n\n- ask: Ask a fast model.\n- github_pr: Open a PR.\n- jira_view: View an issue.\n\n</commands>\n\n\
         <skills>\n\n- pdf: read `skills/pdf/SKILL.md` when the request matches this skill. Handle PDFs.\n\n</skills>\n\n\
         <subagents>\nInvoke with `subagent`.\n\n- researcher: Research things.\n\n</subagents>"
    }

    fn test_context() -> aither_agent::Context {
        let mut context = aither_agent::Context::default();
        context.insert_system_text(BASE_SYSTEM_TAG, base_system_block());
        context.insert_system_text(tag::MEMORY, "remember this");
        context.insert_system_text(tag::WORKSPACE, "sandbox=/tmp cwd=/tmp");
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
            &["github_pr".to_string(), "jira_view".to_string()],
            &test_context(),
            &test_metrics(),
            ContextWindowPhase::Stable,
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
        );
        let summed: usize = report.categories.iter().map(|row| row.tokens).sum();
        assert_eq!(summed, report.used_tokens);
        assert_eq!(
            report.used_tokens + report.free_tokens + report.autocompact_buffer_tokens,
            report.window_tokens
        );
        // Default context policy: compaction is forked at 60% of the window.
        assert_eq!(
            report.autocompact_buffer_tokens,
            report.window_tokens * 2 / 5
        );
    }
}
