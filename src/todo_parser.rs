//! Markdown checklist parser for `tasks.md`.
//!
//! Extracts structured checklist items from standard Markdown checkbox syntax
//! (`- [ ]`, `- [x]`, `* [ ]`, `* [x]`).

/// A parsed checklist document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoChecklist {
    pub items: Vec<TodoChecklistItem>,
}

/// A single checklist item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoChecklistItem {
    pub text: String,
    pub checked: bool,
}

impl TodoChecklist {
    /// Parse a markdown string into a structured checklist.
    ///
    /// Extracts lines matching `- [ ]`/`- [x]`/`* [ ]`/`* [x]` patterns
    /// (case-insensitive `x`, leading whitespace allowed). Non-checklist
    /// lines are ignored.
    #[must_use]
    pub fn parse(content: &str) -> Self {
        let items = content.lines().filter_map(parse_checklist_line).collect();
        Self { items }
    }

    /// Total number of items.
    #[must_use]
    pub fn total(&self) -> usize {
        self.items.len()
    }

    /// Number of completed (checked) items.
    #[must_use]
    pub fn completed(&self) -> usize {
        self.items.iter().filter(|item| item.checked).count()
    }

    /// Returns `true` if the checklist is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Attempt to parse a single line as a checklist item.
fn parse_checklist_line(line: &str) -> Option<TodoChecklistItem> {
    let trimmed = line.trim_start();

    // Must start with `- ` or `* `
    let rest = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))?;

    // Must continue with `[ ]` or `[x]`/`[X]`
    let (checked, after_bracket) = if let Some(tail) = rest.strip_prefix("[ ] ") {
        (false, tail)
    } else if let Some(tail) = rest
        .strip_prefix("[x] ")
        .or_else(|| rest.strip_prefix("[X] "))
    {
        (true, tail)
    } else if rest == "[ ]" {
        (false, "")
    } else if rest == "[x]" || rest == "[X]" {
        (true, "")
    } else {
        return None;
    };

    let text = after_bracket.trim().to_string();
    Some(TodoChecklistItem { checked, text })
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
    fn parse_empty_text_items() {
        let content = "- [ ]";
        let checklist = TodoChecklist::parse(content);
        assert_eq!(checklist.total(), 1);
        assert_eq!(checklist.items[0].text, "");
        assert!(!checklist.items[0].checked);
    }
}
