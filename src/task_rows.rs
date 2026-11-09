//! The session's `tasks.md` working document.
//!
//! `TasksSchema` is the [`aither_agent::DocumentSchema`] the agent parses the
//! document with; `task_rows` reshapes the same document's rows into one
//! serializable model that the daemon sends (`todo_update` events,
//! `GET /api/sessions/{id}/tasks`) and the TUI reads.

use aither_agent::working_document::WorkKind;
use aither_agent::{DocumentRow, DocumentSchema, ParsedDocument, WorkRef};
use askama::Template;

/// A section that holds rows.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskSection {
    /// The root issues the session serves.
    Roots,
    /// Work in progress.
    Doing,
    /// Work waiting to start.
    Queued,
}

impl TaskSection {
    /// Whether rows of this section are open work.
    #[must_use]
    pub const fn is_open_work(self) -> bool {
        matches!(self, Self::Doing | Self::Queued)
    }

    /// The row section a `## ` heading names; every other heading is free
    /// text.
    fn from_heading(heading: &str) -> Option<Self> {
        match heading.trim() {
            "Roots" => Some(Self::Roots),
            "Doing" => Some(Self::Doing),
            "Queued" => Some(Self::Queued),
            _ => None,
        }
    }
}

/// One parsed row of a `tasks.md` document.
struct ParsedRow {
    /// The 1-based line the row starts on.
    line: usize,
    /// The row section it is in.
    section: TaskSection,
    /// Its text without the `- ` marker; continuation lines are joined by
    /// newlines.
    text: String,
    /// The payload of its `BLOCKING <what>` mark.
    blocking: Option<String>,
    /// The issues and pull requests it names, in order.
    references: Vec<WorkRef>,
}

/// Parses `tasks.md` into its rows.
///
/// `## Roots`, `## Doing` and `## Queued` hold `- ` rows; every other `## `
/// section (`## Notes`, `## Decisions`, anything else) is free text. A row's
/// text runs over following lines indented under it. Text inside a fenced
/// code block is never a row.
fn parse_tasks(content: &str) -> Vec<ParsedRow> {
    let mut rows = Vec::new();
    let mut section = None;
    let mut fenced: Option<char> = None;
    for (index, line) in content.lines().enumerate() {
        let trimmed = line.trim_start_matches(['`', '~']);
        if let Some(marker) = fenced {
            if trimmed.is_empty() {
                fenced = None;
            } else {
                let _ = marker;
            }
            continue;
        }
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            fenced = line.trim_start().chars().next();
            continue;
        }
        if let Some(heading) = line.strip_prefix("## ") {
            section = TaskSection::from_heading(heading);
            continue;
        }
        let Some(current) = section else {
            continue;
        };
        if let Some(text) = line.strip_prefix("- ") {
            rows.push(ParsedRow {
                line: index + 1,
                section: current,
                text: text.to_string(),
                blocking: None,
                references: Vec::new(),
            });
        } else if !line.trim().is_empty()
            && line.starts_with(char::is_whitespace)
            && let Some(row) = rows.last_mut()
        {
            row.text.push('\n');
            row.text.push_str(line.trim());
        }
    }
    for row in &mut rows {
        if let Some(index) = row.text.find(" BLOCKING ") {
            row.blocking = Some(row.text[index + " BLOCKING ".len()..].trim().to_string());
        }
        row.references = WorkRef::parse_all(&row.text);
    }
    rows
}

/// May's `tasks.md` format as the agent's working-document schema.
#[derive(Debug)]
pub struct TasksSchema;

impl DocumentSchema for TasksSchema {
    fn parse(&self, content: &str) -> ParsedDocument {
        let mut document = ParsedDocument::new();
        for row in parse_tasks(content) {
            if row.section == TaskSection::Roots {
                for reference in &row.references {
                    document.push_root(reference.clone());
                }
            } else {
                let mut document_row =
                    DocumentRow::new(row.line, row.text.clone()).naming(row.references.clone());
                if row.section.is_open_work() {
                    document_row = document_row.open();
                }
                if let Some(blocking) = &row.blocking {
                    document_row = document_row.blocked_on(blocking.clone());
                }
                document.push_row(document_row);
            }
        }
        document
    }
}

/// Holds the run while `tasks.md` falls behind, and with [`TasksHoldText`]
/// while it has actionable rows.
#[derive(Debug)]
pub struct HoldOpenTasks;

/// The reminder [`HoldOpenTasks`] holds the run with.
#[derive(Template)]
#[template(path = "tasks_hold.txt", escape = "none")]
struct TasksHoldText;

impl aither_agent::StopHook for HoldOpenTasks {
    fn on_turn_end(
        &self,
        ctx: &aither_agent::TurnBoundaryContext<'_>,
    ) -> impl Future<Output = aither_agent::StopDecision> + Send {
        let decision = match ctx.document {
            Some(review) if !review.findings().is_empty() => {
                aither_agent::StopDecision::Hold { reminder: None }
            }
            Some(review) if review.actionable_rows().next().is_some() => {
                aither_agent::StopDecision::Hold {
                    reminder: Some(
                        TasksHoldText
                            .render()
                            .expect("the tasks hold template renders"),
                    ),
                }
            }
            _ => aither_agent::StopDecision::End,
        };
        std::future::ready(decision)
    }
}

/// An issue or pull request a row names.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskReference {
    /// `owner/repo` when the row writes one.
    pub repository: Option<String>,
    /// The issue or pull request number.
    pub number: u64,
    /// Whether it is a pull request rather than an issue.
    pub pull_request: bool,
}

impl TaskReference {
    /// Whether it is an issue of the session's own repository.
    #[must_use]
    pub const fn is_local_issue(&self) -> bool {
        self.repository.is_none() && !self.pull_request
    }
}

impl From<&WorkRef> for TaskReference {
    fn from(reference: &WorkRef) -> Self {
        Self {
            repository: reference.repository().map(ToString::to_string),
            number: reference.number(),
            pull_request: reference.kind() == WorkKind::PullRequest,
        }
    }
}

/// One row of `Roots`, `Doing` or `Queued`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskRow {
    /// The section it is in.
    pub section: TaskSection,
    /// Its text without the `- ` marker; continuation lines are joined by
    /// newlines.
    pub text: String,
    /// The payload of its `BLOCKING <what>` mark.
    pub blocking: Option<String>,
    /// The issues and pull requests it names, in order.
    pub references: Vec<TaskReference>,
}

/// The rows of a `tasks.md` document, in document order.
#[must_use]
pub fn task_rows(content: &str) -> Vec<TaskRow> {
    parse_tasks(content)
        .into_iter()
        .map(|row| TaskRow {
            section: row.section,
            text: row.text,
            blocking: row.blocking,
            references: row.references.iter().map(TaskReference::from).collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCUMENT: &str = "# Session\n\n## Roots\n- May-Agent/may#746\n\n## Doing\n- Fix the parser (#12)\n  second line\n- Ask about scope BLOCKING q42\n\n## Queued\n- Ship PR #13\n\n## Notes\n- not a row #99\n";

    #[test]
    fn rows_keep_section_text_blocking_and_references() {
        let rows = task_rows(DOCUMENT);
        let summary: Vec<_> = rows
            .iter()
            .map(|row| (row.section, row.text.as_str(), row.blocking.as_deref()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (TaskSection::Roots, "May-Agent/may#746", None),
                (
                    TaskSection::Doing,
                    "Fix the parser (#12)\nsecond line",
                    None
                ),
                (
                    TaskSection::Doing,
                    "Ask about scope BLOCKING q42",
                    Some("q42")
                ),
                (TaskSection::Queued, "Ship PR #13", None),
            ]
        );
        assert_eq!(
            rows[0].references[0].repository.as_deref(),
            Some("May-Agent/may")
        );
        assert!(rows[1].references[0].is_local_issue());
        assert!(rows[3].references[0].pull_request);
    }

    #[test]
    fn a_document_without_rows_has_none() {
        assert_eq!(task_rows("just prose\n"), Vec::<TaskRow>::new());
        assert_eq!(task_rows("## Doing\n"), Vec::<TaskRow>::new());
    }
}
