//! Markdown checklist parser for `tasks.md`.
//!
//! Extracts structured checklist items from CommonMark task lists using
//! `pulldown-cmark`. This correctly handles nesting, task markers inside
//! code spans vs real items, and edge cases that a regex approach would
//! miss (for example, `- [ ]` lines inside fenced code blocks must not
//! be treated as checklist items).

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// A parsed checklist document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoChecklist {
    /// Ordered checklist items discovered in the document.
    pub items: Vec<TodoChecklistItem>,
}

/// A single checklist item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoChecklistItem {
    /// The text of the checklist item with surrounding whitespace trimmed.
    pub text: String,
    /// Whether the item is marked as complete (`- [x]`).
    pub checked: bool,
}

impl TodoChecklist {
    /// Parse a CommonMark string into a structured checklist.
    ///
    /// Nested task-list items are flattened into document order: a
    /// top-level checklist item containing a nested sub-checklist
    /// produces one entry per task marker, with the outer item listed
    /// before its children. Only list items that begin with a
    /// task-list marker (`[ ]` / `[x]`) are collected; regular bullets
    /// and prose are ignored.
    #[must_use]
    pub fn parse(content: &str) -> Self {
        let parser = Parser::new_ext(content, Options::ENABLE_TASKLISTS);
        // `pending` holds every open list item in document order, with
        // a `is_task` flag that flips to true only when a task-list
        // marker is observed inside it. Non-task list items are
        // dropped at the end.
        let mut pending: Vec<PendingItem> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();

        for event in parser {
            match event {
                Event::Start(Tag::Item) => {
                    stack.push(pending.len());
                    pending.push(PendingItem::default());
                }
                Event::End(TagEnd::Item) => {
                    stack.pop();
                }
                Event::TaskListMarker(checked) => {
                    if let Some(&index) = stack.last() {
                        pending[index].is_task = true;
                        pending[index].checked = checked;
                    }
                }
                Event::Text(text) | Event::Code(text) => {
                    if let Some(&index) = stack.last() {
                        pending[index].text.push_str(text.as_ref());
                    }
                }
                Event::SoftBreak | Event::HardBreak => {
                    if let Some(&index) = stack.last() {
                        pending[index].text.push(' ');
                    }
                }
                _ => {}
            }
        }

        let items = pending
            .into_iter()
            .filter(|entry| entry.is_task)
            .map(|entry| TodoChecklistItem {
                text: entry.text.trim().to_string(),
                checked: entry.checked,
            })
            .collect();

        Self { items }
    }

    /// Total number of items.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.items.len()
    }

    /// Number of completed (checked) items.
    #[must_use]
    pub fn completed(&self) -> usize {
        self.items.iter().filter(|item| item.checked).count()
    }

    /// Returns `true` if the checklist is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[derive(Default)]
struct PendingItem {
    is_task: bool,
    checked: bool,
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_checklist() {
        let content = "\
- [x] Initialize project
- [ ] Build core module
- [ ] Add tests";

        let checklist = TodoChecklist::parse(content);
        assert_eq!(checklist.total(), 3);
        assert_eq!(checklist.completed(), 1);
        assert_eq!(checklist.items[0].text, "Initialize project");
        assert!(checklist.items[0].checked);
        assert_eq!(checklist.items[1].text, "Build core module");
        assert!(!checklist.items[1].checked);
    }

    #[test]
    fn parse_asterisk_prefix() {
        let content = "\
* [x] Done task
* [ ] Pending task";

        let checklist = TodoChecklist::parse(content);
        assert_eq!(checklist.total(), 2);
        assert!(checklist.items[0].checked);
        assert!(!checklist.items[1].checked);
    }

    #[test]
    fn parse_uppercase_x() {
        let content = "- [X] Completed with uppercase X";
        let checklist = TodoChecklist::parse(content);
        assert_eq!(checklist.total(), 1);
        assert!(checklist.items[0].checked);
    }

    #[test]
    fn parse_with_headings_and_prose() {
        let content = "\
# Project Plan

Some intro text.

## Phase 1
- [x] Step one
- [ ] Step two

## Phase 2
- [ ] Step three";

        let checklist = TodoChecklist::parse(content);
        assert_eq!(checklist.total(), 3);
        assert_eq!(checklist.completed(), 1);
        assert_eq!(checklist.items[0].text, "Step one");
        assert_eq!(checklist.items[2].text, "Step three");
    }

    #[test]
    fn parse_indented_items() {
        let content = "\
- [x] Top level
  - [ ] Indented child
    - [ ] Deeply indented";

        let checklist = TodoChecklist::parse(content);
        assert_eq!(checklist.total(), 3);
        assert!(checklist.items[0].checked);
        assert_eq!(checklist.items[1].text, "Indented child");
        assert_eq!(checklist.items[2].text, "Deeply indented");
    }

    #[test]
    fn parse_empty_content() {
        let checklist = TodoChecklist::parse("");
        assert!(checklist.is_empty());
        assert_eq!(checklist.total(), 0);
        assert_eq!(checklist.completed(), 0);
    }

    #[test]
    fn parse_no_checklist_lines() {
        let content = "\
# Just a heading
Some text here.
- Regular bullet (no checkbox)
* Another bullet";

        let checklist = TodoChecklist::parse(content);
        assert!(checklist.is_empty());
    }

    #[test]
    fn parse_ignores_code_block_checklist_markup() {
        let content = "\
- [x] Real item

```
- [ ] Not a real item inside a code block
- [x] Also not a real item
```

- [ ] Another real item";

        let checklist = TodoChecklist::parse(content);
        assert_eq!(checklist.total(), 2);
        assert!(checklist.items[0].checked);
        assert_eq!(checklist.items[0].text, "Real item");
        assert!(!checklist.items[1].checked);
        assert_eq!(checklist.items[1].text, "Another real item");
    }

    #[test]
    fn parse_item_with_inline_code_preserves_code_text() {
        let content = "- [ ] Run `cargo test` and verify all pass";
        let checklist = TodoChecklist::parse(content);
        assert_eq!(checklist.total(), 1);
        assert!(!checklist.items[0].checked);
        assert_eq!(
            checklist.items[0].text,
            "Run cargo test and verify all pass"
        );
    }

    #[test]
    fn parse_multiline_item_joins_soft_breaks() {
        let content = "\
- [ ] First line
  continuation of the same item
- [x] Second task";

        let checklist = TodoChecklist::parse(content);
        assert_eq!(checklist.total(), 2);
        assert_eq!(
            checklist.items[0].text,
            "First line continuation of the same item"
        );
        assert!(checklist.items[1].checked);
    }
}
