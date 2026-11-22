//! A working-document schema unlike any embedder's, for tests.

use super::{DocumentRow, DocumentSchema, ParsedDocument, WorkRef, words};

/// `* ` lines are open work, `root ` lines name roots, `~ ` lines are rows
/// that are no work, `waits <blocker>` marks what a row waits on, and a
/// `[ ]` line breaks the format.
pub struct ListSchema;

impl DocumentSchema for ListSchema {
    fn parse(&self, content: &str) -> ParsedDocument {
        let mut document = ParsedDocument::new();
        for (index, line) in content.lines().enumerate() {
            let number = index + 1;
            if let Some(text) = line.strip_prefix("root ") {
                for root in WorkRef::parse_all(text) {
                    document.push_root(root);
                }
                document.push_row(DocumentRow::new(number, text).naming(WorkRef::parse_all(text)));
            } else if let Some(text) = line.strip_prefix("* ") {
                let mut row = DocumentRow::new(number, text)
                    .open()
                    .naming(WorkRef::parse_all(text));
                let mut words = words(text);
                if words.any(|word| word == "waits")
                    && let Some(blocker) = words.next()
                {
                    row = row.blocked_on(blocker);
                }
                document.push_row(row);
            } else if let Some(text) = line.strip_prefix("~ ") {
                document.push_row(DocumentRow::new(number, text).naming(WorkRef::parse_all(text)));
            } else if line.starts_with("[ ]") {
                document.push_problem(format!("Line {number} of plan.md is a checkbox."));
            }
        }
        document
    }
}
