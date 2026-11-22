//! The issues a session's task rows name, as the wire carries them.
//!
//! A row of `tasks.md` names an issue of the session's repository as
//! `#<number>`; named issues are looked up in the session's issue store for
//! their title and state. The daemon resolves and serves the briefs; the
//! TUI's sidebar shows them.

use crate::task_rows::TaskRow;

/// Every issue of the session's repository the rows name, first mention
/// first, without repeats.
#[must_use]
pub fn linked_issues(rows: &[TaskRow]) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for reference in rows.iter().flat_map(|row| &row.references) {
        if reference.is_local_issue()
            && let Ok(number) = i64::try_from(reference.number)
            && !out.contains(&number)
        {
            out.push(number);
        }
    }
    out
}

/// A linked issue as the sidebar shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct IssueBrief {
    /// Issue number.
    pub number: i64,
    /// Title, when the lookup succeeded.
    pub title: Option<String>,
    /// Whether the issue is closed.
    pub closed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_rows::task_rows;

    #[test]
    fn only_local_issues_of_row_sections_are_linked() {
        const TASKS: &str = "# Plan\n\n## Roots\n- #11\n\n## Doing\n- Read the code (#12)\n  more on #13\n- Other repo o/r#5 and PR #6\n\n## Queued\n- Ship it, see #12\n\n## Notes\n- Free text #99 never shows.\n";
        let rows = task_rows(TASKS);
        assert_eq!(linked_issues(&rows), vec![11, 12, 13]);
    }
}
