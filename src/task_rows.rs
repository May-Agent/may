//! The rows of a session's `tasks.md` as clients see them.
//!
//! The format and its parser belong to [`crate::tasks_md::TasksFile`]; this
//! module only reshapes its rows into one serializable model that the daemon
//! sends (`todo_update` events, `GET /api/sessions/{id}/tasks`) and the TUI
//! reads.

use aither_agent::WorkRef;
use aither_agent::working_document::WorkKind;

use crate::tasks_md::{Section, TasksFile};

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
    TasksFile::parse(content)
        .rows()
        .iter()
        .map(|row| TaskRow {
            section: match row.section() {
                Section::Roots => TaskSection::Roots,
                Section::Doing => TaskSection::Doing,
                Section::Queued => TaskSection::Queued,
                Section::Notes | Section::Decisions => {
                    unreachable!("tasks.md has rows only in row sections")
                }
            },
            text: row.text().to_string(),
            blocking: row.blocking_mark().map(ToString::to_string),
            references: row.references().iter().map(TaskReference::from).collect(),
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
