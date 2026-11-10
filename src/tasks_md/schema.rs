//! `tasks.md` on aither's working-document mechanism: the schema that reads
//! it, the stop rule that keeps a run going while open rows remain, and the
//! line limit past which it must be rewritten.

#[cfg(test)]
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use askama::Template;

use crate::harness::working_document::{DocumentRow, DocumentSchema, DocumentSpec, ParsedDocument};

use super::{FILE, TasksFile, violation_sentence};

/// Lines `tasks.md` may hold unless `[tasks] line_limit` says otherwise.
pub const DEFAULT_LINE_LIMIT: usize = 60;

/// Lines `tasks.md` may hold; an edit that leaves it past them earns an
/// alert asking for a rewrite.
///
/// A clone shares the limit, so the daemon keeps one, hands it to every
/// document it opens and sets it from each config change.
#[derive(Debug, Clone)]
pub struct LineLimit(Arc<AtomicUsize>);

impl LineLimit {
    /// A limit of `lines` lines.
    #[must_use]
    pub fn new(lines: usize) -> Self {
        Self(Arc::new(AtomicUsize::new(lines)))
    }

    /// Moves every holder of this limit to `lines`.
    pub fn set(&self, lines: usize) {
        self.0.store(lines, Ordering::Relaxed);
    }

    /// The lines a document may hold now.
    #[must_use]
    pub fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for LineLimit {
    fn default() -> Self {
        Self::new(DEFAULT_LINE_LIMIT)
    }
}

/// The `tasks.md` every agent with a sandbox keeps, a subagent's in its own
/// space.
#[must_use]
pub fn document_spec(limit: &LineLimit) -> DocumentSpec {
    DocumentSpec::new(
        FILE,
        TasksSchema {
            limit: limit.clone(),
        },
    )
}

/// The `tasks.md` of `sandbox_dir` as an agent watches it, taking the
/// events recorded on `events`.
#[cfg(test)]
pub(crate) fn working_document(
    sandbox_dir: &Path,
    events: crate::harness::working_document::WorkEvents,
) -> crate::harness::working_document::WorkingDocument {
    crate::harness::working_document::WorkingDocument::new(
        &document_spec(&LineLimit::default()),
        sandbox_dir,
        events,
    )
    .with_continue_reminder(continue_reminder())
}

/// Reads `tasks.md`: rows of `Doing` and `Queued` are open work, `Roots`
/// rows name the roots, a `BLOCKING <what>` mark is what a row waits on, and
/// every violation of the format is a problem.
///
/// A document past its line limit is a problem too. aither reads and parses
/// the document as each turn ends, so the limit costs no read of its own, in
/// a subagent's `tasks.md` as in the main agent's.
#[derive(Debug, Clone, Default)]
pub struct TasksSchema {
    limit: LineLimit,
}

impl DocumentSchema for TasksSchema {
    fn parse(&self, content: &str) -> ParsedDocument {
        let file = TasksFile::parse(content);
        let mut document = ParsedDocument::new();
        for row in file.rows() {
            let mut parsed = DocumentRow::new(row.line(), row.text()).naming(row.references());
            if row.section().is_open_work() {
                parsed = parsed.open();
            }
            if let Some(mark) = row.blocking_mark() {
                parsed = parsed.blocked_on(mark);
            }
            document.push_row(parsed);
        }
        for root in file.roots() {
            document.push_root(root);
        }
        for violation in file.violations() {
            document.push_problem(violation_sentence(violation));
        }
        let limit = self.limit.get();
        if file.lines() > limit {
            document.push_problem(
                OverLimitTemplate {
                    lines: file.lines(),
                    limit,
                }
                .render()
                .expect("the tasks.md line limit template renders typed input"),
            );
        }
        document
    }
}

#[derive(Template)]
#[template(path = "tasks_continue.txt", escape = "none")]
struct ContinueTemplate;

/// The reminder of the stop rule of `tasks.md`.
///
/// A document that fell behind — an event it left unrecorded, a row naming
/// a finished item or a dead mark, a format violation — keeps the run going
/// with the alert naming each; otherwise a row under `Doing` or `Queued`
/// that nothing blocks keeps it going with this reminder to continue. The
/// working document applies the rule as the model ends its turn.
///
/// # Panics
/// Panics when the static template fails to render.
#[must_use]
pub fn continue_reminder() -> String {
    ContinueTemplate
        .render()
        .expect("the tasks.md continue template is static")
}

#[derive(Template)]
#[template(path = "tasks_over_limit.txt", escape = "none")]
struct OverLimitTemplate {
    lines: usize,
    limit: usize,
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    #[test]
    fn rows_marks_roots_and_violations_reach_the_parsed_document() {
        let document = TasksSchema::default().parse(
            "## Roots\n- May-Agent/may#764\n## Doing\n- #12 wire it BLOCKING req-7\n- [ ] ticked\n## Queued\n- PR #13 land\n## Notes\n- #99 not a row\n",
        );
        let rows: Vec<_> = document
            .rows()
            .iter()
            .map(|row| {
                (
                    row.line(),
                    row.is_open(),
                    row.blocker(),
                    row.references().len(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (2, false, None, 1),
                (4, true, Some("req-7"), 1),
                (7, true, None, 1)
            ]
        );
        assert_eq!(document.roots().len(), 1);
        assert_eq!(
            document.problems(),
            ["Line 5 of tasks.md is a checkbox; rows are not ticked, a finished row is deleted."]
        );
    }

    #[test]
    fn a_row_is_a_title_with_detail_and_its_mark_may_sit_in_the_detail() {
        let document =
            TasksSchema::default().parse("## Doing\n- wire it\n  see #12\n  BLOCKING req-7\n");
        let row = &document.rows()[0];
        assert_eq!(row.summary(), "wire it");
        assert_eq!(row.text(), "wire it\nsee #12\nBLOCKING req-7");
        assert_eq!(row.blocker(), Some("req-7"));
        assert_eq!(row.references().len(), 1);
        assert!(row.is_open());
    }

    #[test]
    fn every_violation_reads_as_a_sentence() {
        let document = TasksSchema::default().parse("stray\n## Plans\n## Doing\nplain\n## Doing\n");
        assert_eq!(
            document.problems(),
            [
                "Line 1 of tasks.md is outside every section.",
                "Line 2 of tasks.md starts `## Plans`, which is no section.",
                "Line 4 of tasks.md is not a `- ` row.",
                "Line 5 of tasks.md repeats `## Doing`.",
            ]
        );
    }

    /// A document of `lines` lines, one `Notes` row each after the heading.
    fn document_of(lines: usize) -> String {
        let mut content = String::from("## Notes\n");
        for number in 1..lines {
            _ = writeln!(content, "- note {number}");
        }
        content
    }

    #[test]
    fn a_document_past_the_line_limit_is_a_problem() {
        let schema = TasksSchema::default();
        assert_eq!(
            schema
                .parse(&document_of(DEFAULT_LINE_LIMIT))
                .problems()
                .len(),
            0
        );
        assert_eq!(
            schema
                .parse(&document_of(DEFAULT_LINE_LIMIT + 5))
                .problems(),
            [
                "tasks.md has 65 lines, more than its limit of 60. Rewrite it to hold only current state: delete finished rows and stale notes, and merge duplicates."
            ]
        );
    }

    #[test]
    fn the_line_limit_follows_the_handle_every_spec_was_made_with() {
        let limit = LineLimit::default();
        let schema = TasksSchema {
            limit: limit.clone(),
        };
        let content = document_of(40);
        assert_eq!(schema.parse(&content).problems().len(), 0);
        limit.set(30);
        assert_eq!(schema.parse(&content).problems().len(), 1);
    }
}
