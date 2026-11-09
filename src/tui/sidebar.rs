//! State of the right sidebar: the session's open work, the issues its
//! rows name, and what runs in the background.
//!
//! The rows come from the session's `tasks.md`, parsed by aither's
//! `TasksFile` ([`crate::task_rows`]); the daemon serves them at
//! `GET /api/sessions/{id}/tasks`. Notes and decisions never reach the
//! sidebar. A row names an issue of the session's repository as
//! `#<number>`; named issues are looked up in the session's issue store for
//! their title and state.

use crate::task_rows::TaskRow;

/// Terminal width from which the sidebar is shown.
pub const SIDEBAR_MIN_TERMINAL_WIDTH: u16 = 120;

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

/// Sidebar state kept by the app.
#[derive(Debug)]
pub struct SidebarState {
    /// User toggle (`Ctrl+B`); the sidebar also needs a wide terminal.
    pub enabled: bool,
    /// Session the task tree belongs to.
    pub session_id: Option<String>,
    /// The session's stored title. Titles are generated in the daemon, so
    /// the TUI's in-memory session never learns them by itself.
    pub title: Option<String>,
    /// The rows of the session's `tasks.md`; `None` until loaded or when
    /// there is no `tasks.md`.
    pub tasks: Option<Vec<TaskRow>>,
    /// Linked issues with their lookups, in first-mention order.
    pub issues: Vec<IssueBrief>,
    /// The session's PRs from the daemon's `/prs` endpoint, newest first.
    pub prs: Vec<crate::client::WirePr>,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            enabled: true,
            session_id: None,
            title: None,
            tasks: None,
            issues: Vec::new(),
            prs: Vec::new(),
        }
    }
}

impl SidebarState {
    /// Whether the sidebar shows in a terminal `width` columns wide.
    #[must_use]
    pub const fn visible_at(&self, width: u16) -> bool {
        self.enabled && width >= SIDEBAR_MIN_TERMINAL_WIDTH
    }

    /// Sidebar width for a terminal `width` columns wide.
    #[must_use]
    pub fn width_for(width: u16) -> u16 {
        (width / 4).clamp(32, 44)
    }

    /// Install a freshly loaded `tasks.md` of `session_id`. Returns the
    /// linked issue numbers whose lookup is missing.
    pub fn set_tasks(&mut self, session_id: &str, content: Option<&str>) -> Vec<i64> {
        if self.session_id.as_deref() != Some(session_id) {
            self.session_id = Some(session_id.to_string());
            self.issues.clear();
        }
        self.tasks = content.map(crate::task_rows::task_rows);
        let linked = self.tasks.as_deref().map(linked_issues).unwrap_or_default();
        // Keep known lookups in mention order; unknown ones show their
        // number until their lookup arrives. An unresolved number (its
        // issue may not have existed yet) is looked up again.
        let mut issues = Vec::with_capacity(linked.len());
        let mut missing = Vec::new();
        for number in linked {
            if let Some(known) = self.issues.iter().find(|issue| issue.number == number) {
                if known.title.is_none() {
                    missing.push(number);
                }
                issues.push(known.clone());
            } else {
                issues.push(IssueBrief {
                    number,
                    title: None,
                    closed: false,
                });
                missing.push(number);
            }
        }
        self.issues = issues;
        missing
    }

    /// Store the session's PR list from the daemon.
    pub fn set_prs(&mut self, session_id: &str, prs: Vec<crate::client::WirePr>) {
        if self.session_id.as_deref() == Some(session_id) {
            self.prs = prs;
        }
    }

    /// Install issue lookups for `session_id`.
    pub fn set_issues(&mut self, session_id: &str, found: Vec<IssueBrief>) {
        if self.session_id.as_deref() != Some(session_id) {
            return;
        }
        for brief in found {
            if let Some(slot) = self
                .issues
                .iter_mut()
                .find(|issue| issue.number == brief.number)
            {
                *slot = brief;
            }
        }
    }

    /// Forget everything about the previous session.
    pub fn reset(&mut self) {
        self.session_id = None;
        self.title = None;
        self.tasks = None;
        self.issues.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASKS: &str = "# Plan\n\n## Roots\n- #11\n\n## Doing\n- Read the code (#12)\n  more on #13\n- Other repo o/r#5 and PR #6\n\n## Queued\n- Ship it, see #12\n\n## Notes\n- Free text #99 never shows.\n";

    #[test]
    fn only_local_issues_of_row_sections_are_linked() {
        let rows = crate::task_rows::task_rows(TASKS);
        assert_eq!(linked_issues(&rows), vec![11, 12, 13]);
    }

    #[test]
    fn set_tasks_keeps_known_issue_lookups() {
        let mut state = SidebarState::default();
        assert_eq!(
            state.set_tasks("s", Some("## Doing\n- a #1\n- b #2\n")),
            vec![1, 2]
        );
        state.set_issues(
            "s",
            vec![IssueBrief {
                number: 1,
                title: Some("First".to_string()),
                closed: true,
            }],
        );
        assert_eq!(
            state.set_tasks("s", Some("## Doing\n- b #2\n- a #1\n- c #3\n")),
            vec![2, 3]
        );
        assert_eq!(state.issues[1].title.as_deref(), Some("First"));
        // Another session starts clean.
        assert_eq!(state.set_tasks("t", Some("## Doing\n- a #1\n")), vec![1]);
    }

    #[test]
    fn a_session_issues_event_renames_and_closes_a_shown_issue() {
        let mut state = SidebarState::default();
        state.set_tasks("s", Some("## Doing\n- a #1\n- b #2\n"));
        state.set_issues(
            "s",
            vec![
                IssueBrief {
                    number: 1,
                    title: Some("Old".to_string()),
                    closed: false,
                },
                IssueBrief {
                    number: 2,
                    title: Some("Other".to_string()),
                    closed: false,
                },
            ],
        );

        // The frame the daemon sends after #1 is renamed and closed.
        let frame: crate::client::SessionIssuesFrame = serde_json::from_str(
            r#"{"sessionId":"s","issues":[{"number":1,"title":"New","closed":true}]}"#,
        )
        .expect("frame parses");
        state.set_issues(&frame.session_id, frame.issues);

        assert_eq!(state.issues[0].title.as_deref(), Some("New"));
        assert!(state.issues[0].closed);
        assert_eq!(state.issues[1].title.as_deref(), Some("Other"));
        assert!(!state.issues[1].closed);
    }

    #[test]
    fn visibility_needs_width_and_toggle() {
        let mut state = SidebarState::default();
        assert!(state.visible_at(120));
        assert!(!state.visible_at(119));
        state.enabled = false;
        assert!(!state.visible_at(200));
    }
}
