//! Tool argument projection for streaming UI summaries.
//!
//! The agent streams tool-call arguments as JSON, often arriving as
//! incomplete fragments (`{"description":"Get curr`). The frontend wants
//! to surface a stable summary label as soon as any field is readable,
//! without re-implementing partial-JSON parsing in TypeScript.
//!
//! This module repairs the partial JSON via `partial-json-fixer`, parses
//! it with `serde_json`, and projects only the string fields each tool
//! variant cares about. Each projection is `Option<String>` — `None` when
//! the field hasn't streamed in yet, `Some(value)` once readable.

use partial_json_fixer::fix_json;
use serde_json::Value;

use crate::command_reading::CommandReading;
use crate::daemon::types::ToolVariant;

/// Per-variant subset of tool arguments shown in the UI summary chip.
///
/// Fields are populated based on `ToolVariant`:
/// - `terminal/terminal_input/terminal_read/terminal_kill` → `description`, `script`
/// - `web_search` → `query`
/// - `web_fetch` → `url`
/// - browser → `action`, `target`
/// - task / ask / session / notify / generic → `description`, `prompt` where applicable
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// How May reads a `terminal` call's script. Present once the call's
    /// arguments are complete, so a half-streamed script is never read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reading: Option<CommandReading>,
}

impl ToolSummary {
    /// Returns `true` iff at least one field is populated.
    #[must_use]
    pub const fn has_any(&self) -> bool {
        self.reading.is_some()
            || self.description.is_some()
            || self.script.is_some()
            || self.query.is_some()
            || self.url.is_some()
            || self.action.is_some()
            || self.target.is_some()
            || self.prompt.is_some()
    }

    /// Returns `true` iff every field is `None`. Used by serde to skip
    /// emitting the `summary` object on the wire when there's nothing yet.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !self.has_any()
    }
}

fn pick(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Project a `ToolSummary` from cumulative arguments JSON for the given
/// variant. Empty / unparseable input returns an all-`None` summary.
#[must_use]
pub fn project_tool_summary(arguments_raw: &str, variant: ToolVariant) -> ToolSummary {
    let trimmed = arguments_raw.trim();
    if trimmed.is_empty() {
        return ToolSummary::default();
    }
    // `fix_json` repairs the fragment textually (one pass, no intermediate
    // tree); the single serde parse then performs JSON unescaping. This
    // replaces the previous repair-parse → re-serialize → re-parse round
    // trip, which did three full passes per streamed delta.
    let (value, complete): (Value, bool) = match serde_json::from_str(trimmed) {
        Ok(v) => (v, true),
        Err(_) => match serde_json::from_str(&fix_json(trimmed)) {
            Ok(v) => (v, false),
            Err(_) => return ToolSummary::default(),
        },
    };
    if !value.is_object() {
        return ToolSummary::default();
    }

    match variant {
        ToolVariant::Terminal => {
            let script = pick(&value, "script");
            let reading = script
                .as_deref()
                .filter(|_| complete)
                .map(CommandReading::of);
            ToolSummary {
                description: pick(&value, "description"),
                script,
                reading,
                ..ToolSummary::default()
            }
        }
        ToolVariant::TerminalInput | ToolVariant::TerminalRead | ToolVariant::TerminalKill => {
            ToolSummary {
                description: pick(&value, "description"),
                script: pick(&value, "script"),
                ..ToolSummary::default()
            }
        }
        ToolVariant::WebSearch => ToolSummary {
            query: pick(&value, "query"),
            ..ToolSummary::default()
        },
        ToolVariant::WebFetch => ToolSummary {
            url: pick(&value, "url"),
            ..ToolSummary::default()
        },
        ToolVariant::Browser => ToolSummary {
            action: pick(&value, "action"),
            target: pick(&value, "target").or_else(|| pick(&value, "url")),
            ..ToolSummary::default()
        },
        ToolVariant::Task => ToolSummary {
            description: pick(&value, "description"),
            prompt: pick(&value, "prompt"),
            ..ToolSummary::default()
        },
        ToolVariant::Ask | ToolVariant::Session | ToolVariant::Notify | ToolVariant::Generic => {
            ToolSummary {
                description: pick(&value, "description"),
                prompt: pick(&value, "prompt"),
                ..ToolSummary::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Summary projection MUST surface `description` first for terminal
    /// variants, since the user sees the description in the summary chip
    /// long before the script bytes finish streaming. Anyone tempted to
    /// reorder these fields should fail this test.
    #[test]
    fn terminal_summary_extracts_description_and_script_progressively() {
        let frames = [
            "{\"",
            "{\"description",
            "{\"description\":\"",
            "{\"description\":\"Get",
            "{\"description\":\"Get current local time",
            "{\"description\":\"Get current local time\",\"script",
            "{\"description\":\"Get current local time\",\"script\":\"date",
            "{\"description\":\"Get current local time\",\"script\":\"date +%s\"}",
        ];
        let mut prior_desc: Option<String> = None;
        for frame in frames {
            let s = project_tool_summary(frame, ToolVariant::Terminal);
            // Description must monotonically grow once it appears — never
            // disappear, never get replaced by the empty default.
            if let Some(d) = &s.description {
                if let Some(prev) = &prior_desc {
                    assert!(
                        d.starts_with(prev),
                        "description regressed: {prev:?} -> {d:?}"
                    );
                }
                prior_desc = Some(d.clone());
            }
        }
        // Final frame: both fields populated.
        let final_summary = project_tool_summary(frames.last().unwrap(), ToolVariant::Terminal);
        assert_eq!(
            final_summary.description.as_deref(),
            Some("Get current local time")
        );
        assert_eq!(final_summary.script.as_deref(), Some("date +%s"));
    }

    #[test]
    fn a_terminal_call_is_read_only_once_its_arguments_are_complete() {
        let streaming = "{\"description\":\"x\",\"script\":\"cat a.rs";
        assert!(
            project_tool_summary(streaming, ToolVariant::Terminal)
                .reading
                .is_none()
        );
        let complete = "{\"description\":\"x\",\"script\":\"cat a.rs\"}";
        let summary = project_tool_summary(complete, ToolVariant::Terminal);
        let reading = summary.reading.expect("complete arguments are read");
        assert_eq!(reading.steps[0].line, "Read a.rs");
        let other = project_tool_summary(complete, ToolVariant::TerminalInput);
        assert!(other.reading.is_none());
    }

    #[test]
    fn empty_input_returns_default_summary() {
        let s = project_tool_summary("", ToolVariant::Terminal);
        assert!(!s.has_any());
        let s2 = project_tool_summary("   ", ToolVariant::Terminal);
        assert!(!s2.has_any());
    }

    #[test]
    fn web_search_extracts_query_only() {
        let s = project_tool_summary(
            "{\"query\":\"may agent\",\"description\":\"ignored here\"}",
            ToolVariant::WebSearch,
        );
        assert_eq!(s.query.as_deref(), Some("may agent"));
        assert!(s.description.is_none());
    }

    #[test]
    fn browser_falls_back_to_url_when_target_missing() {
        let s = project_tool_summary(
            "{\"action\":\"navigate\",\"url\":\"https://example.com\"}",
            ToolVariant::Browser,
        );
        assert_eq!(s.action.as_deref(), Some("navigate"));
        assert_eq!(s.target.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn malformed_input_yields_empty_summary_without_panic() {
        let s = project_tool_summary("not json at all !!!", ToolVariant::Terminal);
        assert!(!s.has_any());
    }
}
