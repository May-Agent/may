//! State of the right sidebar: the session's open work, the issues its
//! rows name, and what runs in the background.
//!
//! The rows come from the session's `tasks.md`, parsed by aither's
//! `TasksFile` ([`crate::task_rows`]); the daemon serves them at
//! `GET /api/sessions/{id}/tasks`. Notes and decisions never reach the
//! sidebar. A row names an issue of the session's repository as
//! `#<number>`; named issues are looked up in the session's issue store for
//! their title and state.

use std::collections::HashSet;

use crate::task_rows::TaskRow;

/// Every collapsible section.
const ALL_SECTIONS: [SidebarSection; 7] = [
    SidebarSection::Doing,
    SidebarSection::Queued,
    SidebarSection::Issues,
    SidebarSection::PullRequests,
    SidebarSection::Changes,
    SidebarSection::Subagents,
    SidebarSection::Jobs,
];

/// The key a task row's detail state is kept under: its title.
#[must_use]
pub fn task_key(row: &TaskRow) -> String {
    row.title.split_whitespace().collect::<Vec<_>>().join(" ")
}

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

/// A section of the sidebar that collapses under its heading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SidebarSection {
    /// Tasks in progress.
    Doing,
    /// Tasks waiting their turn.
    Queued,
    /// Issues the task rows name.
    Issues,
    /// The session's live pull requests.
    PullRequests,
    /// File changes per workspace root.
    Changes,
    /// Running subagents.
    Subagents,
    /// Background terminal jobs.
    Jobs,
}

/// What a click on a sidebar row acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarHit {
    /// A section heading: collapses or expands the section.
    Heading(SidebarSection),
    /// A `… N more` row: lists every entry of the section, or folds it again.
    More(SidebarSection),
    /// A task row with a detail: shows or hides the detail. Keyed by the
    /// row's first line, which survives a re-read of `tasks.md`.
    Task(String),
    /// A PR row with checks: shows or hides them. Keyed by the PR number.
    Pr(i64),
}

/// What the last frame drew, so input can act on it.
#[derive(Debug, Default)]
pub struct SidebarView {
    /// Rows of the screen the click targets sit on.
    pub hits: Vec<(u16, SidebarHit)>,
    /// How far the body can scroll.
    pub max_scroll: usize,
}

/// Sidebar state kept by the app.
#[derive(Debug)]
pub struct SidebarState {
    /// Screen rect the sidebar occupied last frame (mouse mapping); empty
    /// while it is hidden.
    pub area: ratatui::layout::Rect,
    /// Rows scrolled off the top of the body (everything below the title).
    pub scroll: usize,
    /// Sections folded under their heading.
    pub collapsed: HashSet<SidebarSection>,
    /// Sections listing every entry instead of their first few.
    pub listed_all: HashSet<SidebarSection>,
    /// Task rows showing their detail.
    pub detailed: HashSet<String>,
    /// PR rows (by number) showing their checks.
    pub checked: HashSet<i64>,
    /// Every section open, every list and every detail shown.
    pub expand_all: bool,
    /// What the last frame drew.
    pub view: SidebarView,
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
            area: ratatui::layout::Rect::default(),
            scroll: 0,
            collapsed: HashSet::new(),
            listed_all: HashSet::new(),
            detailed: HashSet::new(),
            checked: HashSet::new(),
            expand_all: false,
            view: SidebarView::default(),
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
        let linked = self.tasks.as_deref().map_or_default(linked_issues);
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

    /// Whether the body overflows, so the wheel and keys scroll it.
    #[must_use]
    pub const fn scrollable(&self) -> bool {
        self.view.max_scroll > 0
    }

    /// Scroll the body up by `rows`.
    pub fn scroll_up(&mut self, rows: usize) {
        self.scroll = self.scroll.min(self.view.max_scroll).saturating_sub(rows);
    }

    /// Scroll the body down by `rows`, stopping at its end.
    pub fn scroll_down(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_add(rows).min(self.view.max_scroll);
    }

    /// Whether `section` is folded under its heading.
    #[must_use]
    pub fn is_collapsed(&self, section: SidebarSection) -> bool {
        self.collapsed.contains(&section)
    }

    /// Whether `section` lists every entry.
    #[must_use]
    pub fn lists_all(&self, section: SidebarSection) -> bool {
        self.expand_all || self.listed_all.contains(&section)
    }

    /// Whether the task row keyed `key` shows its detail.
    #[must_use]
    pub fn shows_detail(&self, key: &str) -> bool {
        self.expand_all || self.detailed.contains(key)
    }

    /// Whether the PR numbered `number` shows its checks.
    #[must_use]
    pub fn shows_checks(&self, number: i64) -> bool {
        self.expand_all || self.checked.contains(&number)
    }

    /// Open every section and show every entry and detail, or fold the
    /// lists and details back.
    pub fn toggle_expand_all(&mut self) {
        self.expand_all = !self.expand_all;
        if self.expand_all {
            self.collapsed.clear();
        }
    }

    /// Act on a click at screen row `row`.
    pub fn click(&mut self, row: u16) {
        let Some(hit) = self
            .view
            .hits
            .iter()
            .find_map(|(y, hit)| (*y == row).then(|| hit.clone()))
        else {
            return;
        };
        match hit {
            SidebarHit::Heading(section) => {
                if !self.collapsed.remove(&section) {
                    self.collapsed.insert(section);
                }
            }
            SidebarHit::More(section) => {
                if self.expand_all {
                    self.expand_all = false;
                    self.listed_all.extend(ALL_SECTIONS);
                    self.listed_all.remove(&section);
                } else if !self.listed_all.remove(&section) {
                    self.listed_all.insert(section);
                }
            }
            SidebarHit::Task(key) => {
                if self.expand_all {
                    self.expand_all = false;
                    self.detailed
                        .extend(self.tasks.iter().flatten().map(task_key));
                    self.detailed.remove(&key);
                } else if !self.detailed.remove(&key) {
                    self.detailed.insert(key);
                }
            }
            SidebarHit::Pr(number) => {
                if self.expand_all {
                    self.expand_all = false;
                    self.checked.extend(self.prs.iter().map(|pr| pr.number));
                    self.checked.remove(&number);
                } else if !self.checked.remove(&number) {
                    self.checked.insert(number);
                }
            }
        }
    }

    /// Forget everything about the previous session.
    pub fn reset(&mut self) {
        self.scroll = 0;
        self.collapsed.clear();
        self.listed_all.clear();
        self.detailed.clear();
        self.checked.clear();
        self.expand_all = false;
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
