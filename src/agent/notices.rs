//! The text May's agents read for aither's alerts, notices and refusals.
//!
//! aither renders each through the [`Notices`] the builder is given; May
//! words the families it owns and keeps aither's wording for the rest.

use aither_agent::{DocumentAlert, DocumentAlertItem, DocumentShown, Notices, WorkEvent};
use aither_sandbox::SandboxNotices;
use askama::Template;

/// May's wording: `tasks.md` and its alerts in its own words, aither's for
/// every other family.
#[derive(Debug, Clone, Copy, Default)]
pub struct MayNotices;

#[derive(Template)]
#[template(path = "tasks_alert.txt", escape = "none")]
struct TasksAlertTemplate<'a> {
    document: &'a str,
    items: &'a [DocumentAlertItem],
    held: bool,
}

#[derive(Template)]
#[template(path = "tasks_document.txt", escape = "none")]
struct TasksDocumentTemplate<'a> {
    document: &'a str,
    content: Option<&'a str>,
    changes: Option<&'a str>,
    trees: Option<&'a str>,
}

impl SandboxNotices for MayNotices {}

impl Notices for MayNotices {
    fn working_document(&self, shown: &DocumentShown<'_>) -> String {
        let numbered = shown.content.map(crate::tasks_md::numbered);
        TasksDocumentTemplate {
            document: shown.document,
            content: numbered.as_deref(),
            changes: shown.changes,
            trees: shown.trees,
        }
        .render()
        .expect("the tasks document template renders typed input")
    }

    fn document_alert(&self, alert: &DocumentAlert<'_>) -> String {
        TasksAlertTemplate {
            document: alert.document,
            items: alert.items,
            held: alert.held,
        }
        .render()
        .expect("the tasks alert template renders typed input")
    }
}

/// The notices every May agent, its forks and subagents render through.
#[must_use]
pub fn may_notices() -> aither_agent::AnyNotices {
    aither_agent::AnyNotices::new(MayNotices)
}

/// A command's text refused for its language, as the error its caller
/// reads.
#[must_use]
pub fn language_refused(
    refusal: aither_agent::LanguageRefusal,
) -> aither_sandbox::Refused<aither_agent::LanguageRefusal> {
    aither_sandbox::Refused::rendered(refusal, |refusal| MayNotices.language_refusal(refusal))
}

#[cfg(test)]
mod tests {
    use aither_agent::WorkRef;

    use super::*;

    fn rendered(items: &[DocumentAlertItem], held: bool) -> String {
        MayNotices.document_alert(&DocumentAlert {
            document: "tasks.md",
            items,
            held,
        })
    }

    #[test]
    fn the_shown_document_numbers_its_rows() {
        let text = MayNotices.working_document(&DocumentShown {
            document: "tasks.md",
            content: Some("## Doing\n- wire it\n  detail\n\n## Queued\n- 写文档\n"),
            changes: None,
            trees: None,
        });
        assert_eq!(
            text,
            "<tasks_document>\nContents of tasks.md:\n## Doing\n1. wire it\n  detail\n\n## Queued\n2. 写文档\n\n</tasks_document>"
        );
    }

    #[test]
    fn an_unrecorded_question_asks_for_its_blocking_mark() {
        let text = rendered(
            &[DocumentAlertItem::Event {
                event: WorkEvent::QuestionAsked("req-7".to_string()),
                rows: Vec::new(),
            }],
            false,
        );
        assert_eq!(
            text,
            "<tasks_alert>\nQuestion req-7 was asked; no row of tasks.md names it. Mark the row that waits on its answer `BLOCKING req-7`.\n</tasks_alert>"
        );
    }

    #[test]
    fn a_held_alert_names_each_finding_and_asks_for_an_update() {
        let text = rendered(
            &[
                DocumentAlertItem::Event {
                    event: WorkEvent::PullRequestMerged(WorkRef::pull_request(12)),
                    rows: vec!["#3 PR #12 open".to_string()],
                },
                DocumentAlertItem::DeadBlocker {
                    row: "#4 pick a db BLOCKING req-7".to_string(),
                    blocker: "req-7".to_string(),
                },
                DocumentAlertItem::Problem("Line 9 of tasks.md is not a `- ` row.".to_string()),
            ],
            true,
        );
        assert_eq!(
            text,
            "<tasks_alert>\nPR #12 was merged, and tasks.md still says: \"#3 PR #12 open\"\nThe row \"#4 pick a db BLOCKING req-7\" is marked BLOCKING req-7, which no longer waits.\nLine 9 of tasks.md is not a `- ` row.\nBring tasks.md up to date before ending the turn.\n</tasks_alert>"
        );
    }
}
