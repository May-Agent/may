//! `tasks.md` on aither's working-document mechanism: the schema that reads
//! it, the stop rule that keeps a run going while open rows remain, and the
//! line limit past which it must be rewritten.

use std::path::{Path, PathBuf};

use aither_agent::{
    Block, ContextSource, DocumentRow, DocumentSchema, DocumentSpec, ParsedDocument, StopDecision,
    StopHook, TurnBoundaryContext,
};
use askama::Template;

use super::{FILE, LineViolation, TasksFile, Violation};

/// Lines `tasks.md` may hold; an edit that takes it past them earns one
/// alert asking for a rewrite.
pub const LINE_LIMIT: usize = 60;

/// The `tasks.md` every agent with a sandbox keeps, a subagent's in its own
/// space.
#[must_use]
pub fn document_spec() -> DocumentSpec {
    DocumentSpec::new(FILE, TasksSchema)
}

/// The `tasks.md` of `sandbox_dir` as an agent watches it, taking the
/// events recorded on `events`.
#[cfg(test)]
pub(crate) fn working_document(
    sandbox_dir: &Path,
    events: aither_agent::WorkEvents,
) -> aither_agent::WorkingDocument {
    aither_agent::WorkingDocument::new(&document_spec(), sandbox_dir, events)
        .with_notices(crate::agent::notices::may_notices())
}

/// Reads `tasks.md`: rows of `Doing` and `Queued` are open work, `Roots`
/// rows name the roots, a `BLOCKING <what>` mark is what a row waits on, and
/// every violation of the format is a problem.
#[derive(Debug, Clone, Copy, Default)]
pub struct TasksSchema;

#[derive(Template)]
#[template(path = "tasks_violation.txt", escape = "none")]
struct ViolationTemplate<'a> {
    line: usize,
    violation: &'a Violation,
}

fn problem(violation: &LineViolation) -> String {
    ViolationTemplate {
        line: violation.line,
        violation: &violation.violation,
    }
    .render()
    .expect("the tasks.md violation template renders typed input")
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
            document.push_problem(problem(violation));
        }
        document
    }
}

#[derive(Template)]
#[template(path = "tasks_continue.txt", escape = "none")]
struct ContinueTemplate;

/// The stop rule of `tasks.md`.
///
/// A document that fell behind — an event it left unrecorded, a row naming
/// a finished item or a dead mark, a format violation — keeps the run going
/// with the alert naming each; otherwise a row under `Doing` or `Queued`
/// that nothing blocks keeps it going with a reminder to continue.
#[derive(Debug, Clone, Copy, Default)]
pub struct TasksStopHook;

impl StopHook for TasksStopHook {
    fn on_turn_end(
        &self,
        ctx: &TurnBoundaryContext<'_>,
    ) -> impl Future<Output = StopDecision> + Send {
        let decision = match ctx.document {
            Some(review) if !review.findings().is_empty() => StopDecision::Hold { reminder: None },
            Some(review) if review.actionable_rows().next().is_some() => StopDecision::Hold {
                reminder: Some(
                    ContinueTemplate
                        .render()
                        .expect("the tasks.md continue template is static"),
                ),
            },
            _ => StopDecision::End,
        };
        std::future::ready(decision)
    }
}

#[derive(Template)]
#[template(path = "tasks_over_limit.txt", escape = "none")]
struct OverLimitTemplate {
    lines: usize,
    limit: usize,
}

/// Alerts once when a turn leaves `tasks.md` past [`LINE_LIMIT`] lines, and
/// again only after it came back under the limit and crossed it anew.
#[derive(Debug)]
pub struct TasksLineLimit {
    path: PathBuf,
    over_limit: bool,
}

impl TasksLineLimit {
    /// Watches the `tasks.md` of `sandbox_dir`.
    #[must_use]
    pub fn new(sandbox_dir: &Path) -> Self {
        Self {
            path: sandbox_dir.join(FILE),
            over_limit: false,
        }
    }

    /// The alert for a document of `lines` lines, once per crossing.
    fn observe(&mut self, lines: usize) -> Option<Block> {
        if lines <= LINE_LIMIT {
            self.over_limit = false;
            return None;
        }
        if self.over_limit {
            return None;
        }
        self.over_limit = true;
        Some(Block::new(
            "tasks_alert",
            OverLimitTemplate {
                lines,
                limit: LINE_LIMIT,
            }
            .render()
            .expect("the tasks.md line limit template renders typed input"),
        ))
    }
}

impl ContextSource for TasksLineLimit {
    async fn on_tool_turn_end(&mut self) -> Option<Block> {
        let lines = match async_fs::read_to_string(&self.path).await {
            Ok(content) => TasksFile::parse(&content).lines(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => {
                tracing::warn!(path = %self.path.display(), %error, "failed to read tasks.md for its line limit");
                return None;
            }
        };
        self.observe(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_marks_roots_and_violations_reach_the_parsed_document() {
        let document = TasksSchema.parse(
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
    fn every_violation_reads_as_a_sentence() {
        let document = TasksSchema.parse("stray\n## Plans\n## Doing\nplain\n## Doing\n");
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

    #[test]
    fn the_line_limit_alerts_once_per_crossing() {
        let mut limit = TasksLineLimit::new(Path::new("/nonexistent"));
        assert!(limit.observe(LINE_LIMIT).is_none());
        let alert = limit.observe(LINE_LIMIT + 5).expect("the crossing alerts");
        assert_eq!(
            alert.content(),
            "<tasks_alert>\ntasks.md has 65 lines, more than its limit of 60. Rewrite it to hold only current state: delete finished rows and stale notes, and merge duplicates.\n</tasks_alert>"
        );
        assert!(limit.observe(LINE_LIMIT + 9).is_none(), "said once");
        assert!(limit.observe(3).is_none());
        assert!(limit.observe(LINE_LIMIT + 1).is_some(), "a new crossing");
    }
}
