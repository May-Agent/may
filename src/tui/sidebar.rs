//! State of the right sidebar: the session's task tree, the issues its
//! tasks link, and what runs in the background.
//!
//! The task tree comes from the session's `tasks.md`, served by the daemon
//! at `GET /api/sessions/{id}/tasks`. Only checklist items are kept —
//! headings, notes and other prose in the document are the model's working
//! notes and never reach the sidebar. A task links an issue by mentioning
//! it as `#<number>`; linked issues are looked up in the session's issue
//! store for their title and state.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// Terminal width from which the sidebar is shown.
pub const SIDEBAR_MIN_TERMINAL_WIDTH: u16 = 120;

/// One checklist item of `tasks.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskNode {
    /// Item text, whitespace-normalized.
    pub text: String,
    /// Whether the item is checked.
    pub done: bool,
    /// Number of checklist items it is nested under.
    pub depth: usize,
    /// Issue numbers the item mentions as `#<number>`, in order.
    pub issues: Vec<i64>,
}

/// The checklist items of a `tasks.md`, in document order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskTree {
    /// Items with their nesting depth.
    pub nodes: Vec<TaskNode>,
}

impl TaskTree {
    /// Parse the task tree out of a `tasks.md` document. Nested lists keep
    /// their depth counted in checklist ancestors (a plain bullet between
    /// two task levels does not add one); everything that is not a task
    /// item is dropped.
    #[must_use]
    pub fn parse(markdown: &str) -> Self {
        struct Pending {
            text: String,
            task: bool,
            done: bool,
            parent: Option<usize>,
        }
        let mut items: Vec<Pending> = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        for event in Parser::new_ext(markdown, Options::ENABLE_TASKLISTS) {
            match event {
                Event::Start(Tag::Item) => {
                    items.push(Pending {
                        text: String::new(),
                        task: false,
                        done: false,
                        parent: open.last().copied(),
                    });
                    open.push(items.len() - 1);
                }
                Event::End(TagEnd::Item) => {
                    open.pop();
                }
                Event::TaskListMarker(checked) => {
                    if let Some(&index) = open.last() {
                        items[index].task = true;
                        items[index].done = checked;
                    }
                }
                Event::Text(text) | Event::Code(text) => {
                    if let Some(&index) = open.last() {
                        items[index].text.push_str(&text);
                    }
                }
                Event::SoftBreak | Event::HardBreak => {
                    if let Some(&index) = open.last() {
                        items[index].text.push(' ');
                    }
                }
                _ => {}
            }
        }
        let depth_of = |mut parent: Option<usize>| {
            let mut depth = 0;
            while let Some(index) = parent {
                if items[index].task {
                    depth += 1;
                }
                parent = items[index].parent;
            }
            depth
        };
        let nodes = items
            .iter()
            .filter(|item| item.task)
            .map(|item| {
                let text = item.text.split_whitespace().collect::<Vec<_>>().join(" ");
                TaskNode {
                    issues: issue_refs(&text),
                    text,
                    done: item.done,
                    depth: depth_of(item.parent),
                }
            })
            .collect();
        Self { nodes }
    }

    /// `(done, total)` item counts.
    #[must_use]
    pub fn progress(&self) -> (usize, usize) {
        (
            self.nodes.iter().filter(|node| node.done).count(),
            self.nodes.len(),
        )
    }

    /// Every linked issue number, first mention first, without repeats.
    #[must_use]
    pub fn linked_issues(&self) -> Vec<i64> {
        let mut out: Vec<i64> = Vec::new();
        for number in self.nodes.iter().flat_map(|node| node.issues.iter()) {
            if !out.contains(number) {
                out.push(*number);
            }
        }
        out
    }
}

/// Issue numbers mentioned as `#<digits>` in `text`. The `#` must start a
/// word (so `C#1` or `foo#2` are not references) and the digits must end
/// one.
#[must_use]
pub fn issue_refs(text: &str) -> Vec<i64> {
    let mut out = Vec::new();
    let mut previous: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch == '#' && previous.is_none_or(|p| !p.is_alphanumeric() && p != '#') {
            let mut number: i64 = 0;
            let mut digits = 0usize;
            while let Some(&(_, digit)) = chars.peek() {
                let Some(value) = digit.to_digit(10) else {
                    break;
                };
                number = number.saturating_mul(10).saturating_add(i64::from(value));
                digits += 1;
                chars.next();
            }
            let ends_word = chars
                .peek()
                .is_none_or(|&(_, next)| !next.is_alphanumeric() && next != '_');
            if digits > 0 && ends_word && number > 0 && !out.contains(&number) {
                out.push(number);
            }
            previous = Some('0');
            continue;
        }
        previous = Some(ch);
    }
    out
}

/// A linked issue as the sidebar shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// The session's task tree; `None` until loaded or when there is no
    /// `tasks.md`.
    pub tasks: Option<TaskTree>,
    /// Linked issues with their lookups, in first-mention order.
    pub issues: Vec<IssueBrief>,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            enabled: true,
            session_id: None,
            title: None,
            tasks: None,
            issues: Vec::new(),
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
        self.tasks = content.map(TaskTree::parse);
        let linked = self
            .tasks
            .as_ref()
            .map(TaskTree::linked_issues)
            .unwrap_or_default();
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

    const TASKS: &str = "# Plan\n\nSome notes the model keeps for itself.\n\n## Phase 1\n\n- [x] Read the code (#12)\n- [ ] Fix rendering\n  - [x] images #13\n  - plain bullet\n    - [ ] nested under a bullet\n- [ ] Ship it, see #12 and C#4\n\n```\n- [ ] not a task, inside a fence\n```\n\n## Notes\n\nFree text #99 never shows.\n";

    #[test]
    fn parses_only_the_task_tree() {
        let tree = TaskTree::parse(TASKS);
        let summary: Vec<(&str, bool, usize)> = tree
            .nodes
            .iter()
            .map(|node| (node.text.as_str(), node.done, node.depth))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("Read the code (#12)", true, 0),
                ("Fix rendering", false, 0),
                ("images #13", true, 1),
                ("nested under a bullet", false, 1),
                ("Ship it, see #12 and C#4", false, 0),
            ]
        );
        assert_eq!(tree.progress(), (2, 5));
        // Notes (#99) are not tasks; C#4 is not a reference.
        assert_eq!(tree.linked_issues(), vec![12, 13]);
    }

    #[test]
    fn issue_refs_need_word_boundaries() {
        assert_eq!(
            issue_refs("#1 fixes #22, (#3) a#4 #5x #6_ ##7"),
            vec![1, 22, 3]
        );
        assert_eq!(issue_refs("见 #8。"), vec![8]);
    }

    #[test]
    fn set_tasks_keeps_known_issue_lookups() {
        let mut state = SidebarState::default();
        assert_eq!(
            state.set_tasks("s", Some("- [ ] a #1\n- [ ] b #2\n")),
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
            state.set_tasks("s", Some("- [ ] b #2\n- [ ] a #1\n- [ ] c #3\n")),
            vec![2, 3]
        );
        assert_eq!(state.issues[1].title.as_deref(), Some("First"));
        // Another session starts clean.
        assert_eq!(state.set_tasks("t", Some("- [ ] a #1\n")), vec![1]);
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
