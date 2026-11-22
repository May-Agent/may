#[cfg(test)]
mod tests {
    use may_protocol::context_report::*;
    use super::tag;
    use aither_core::llm::Message;

    fn base_system_block() -> &'static str {
        "<tool-architecture>\nNative tools run commands.\n</tool-architecture>\n\n\
         <skills>\n\n- pdf: read `skills/pdf/SKILL.md` when the request matches this skill. Handle PDFs.\n\n</skills>\n\n\
         <subagents>\nInvoke with `subagent`.\n\n- researcher: Research things.\n\n</subagents>"
    }

    fn inventory<'a>(
        named_commands: &'a [String],
        native_tools: &'a [ToolDefinition],
    ) -> ToolInventory<'a> {
        ToolInventory {
            named_commands,
            native_tools,
        }
    }

    fn native_tools() -> Vec<ToolDefinition> {
        ["terminal", "terminal_read"]
            .into_iter()
            .map(|name| {
                ToolDefinition::from_parts(
                    name.into(),
                    "runs a command".into(),
                    serde_json::json!({"type": "object"}),
                )
                .expect("an object schema is valid")
            })
            .collect()
    }

    /// The command table: `ask` is registered by name, the rest are MCP
    /// tools.
    const COMMAND_TABLE: &str = "<commands>\n- ask: Ask a fast model\n- github_pr: Open a PR\n- jira_view: View an issue\n</commands>";

    fn test_context() -> aither_agent::Context {
        let mut context = aither_agent::Context::with_layout(tag::layout());
        let instructions = context.instructions_mut();
        instructions.insert(&tag::CAPABILITIES, base_system_block());
        instructions.insert(&tag::COMMANDS, COMMAND_TABLE);
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
            &inventory(&["ask".to_string()], &native_tools()),
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

        // System tools counts and sizes the definitions the agent sends.
        let system_tools = report
            .categories
            .iter()
            .find(|row| row.kind == ContextCategory::SystemTools)
            .expect("system tools row");
        assert_eq!(system_tools.detail.as_deref(), Some("2 tools"));
        assert_eq!(
            system_tools.tokens,
            native_tools()
                .iter()
                .map(tool_definition_chars)
                .sum::<usize>()
                / 4
        );
        assert!(system_tools.tokens > 0);

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
            &inventory(&[], &native_tools()),
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
        // Default policy on a 200k window: the window less the 20k handoff
        // reserve, below the 400k ceiling.
        assert_eq!(report.compact_tokens, 180_000);
        assert_eq!(report.autocompact_buffer_tokens, 20_000);
        assert_eq!(report.compact_start_tokens, 130_000);
    }

    #[test]
    fn a_million_token_window_compacts_at_the_ceiling() {
        let mut metrics = test_metrics();
        metrics.context_window = 1_000_000;
        let report = ContextReport::assemble(
            "gpt-5",
            "openai",
            &inventory(&[], &native_tools()),
            &test_context(),
            &metrics,
            ContextWindowPhase::Stable,
            &ContextPolicy::default(),
        );
        assert_eq!(report.compact_tokens, 400_000);
        assert_eq!(report.compact_start_tokens, 350_000);
        assert_eq!(report.autocompact_buffer_tokens, 600_000);
    }

    #[test]
    fn the_report_follows_the_configured_policy() {
        let policy = may_config::ContextConfig {
            compact_ceiling: 100_000,
            start_distance: 20_000,
            ..may_config::ContextConfig::default()
        }
        .policy();
        let report = ContextReport::assemble(
            "gpt-5",
            "openai",
            &inventory(&[], &native_tools()),
            &test_context(),
            &test_metrics(),
            ContextWindowPhase::Compacting,
            &policy,
        );
        assert_eq!(report.compact_tokens, 100_000);
        assert_eq!(report.compact_start_tokens, 80_000);
        assert_eq!(report.phase, ContextWindowPhase::Compacting);
    }

    #[test]
    fn a_closing_tag_before_the_opening_one_is_no_section() {
        assert_eq!(section_span("</skills> 你好 <skills>", "skills"), None);
        assert_eq!(
            section_span("你好<skills>あ</skills>한", "skills"),
            Some((6, 6 + "<skills>あ</skills>".len()))
        );
    }
}
