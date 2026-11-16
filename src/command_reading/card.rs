//! The card a `terminal` call of one of May's own commands renders as.
//!
//! A card is built from what the command did, not from its output text: the
//! `tasks` card from the rows the command changed, the `subagent` card from
//! the run the command tracked, the `ask_user` card from the question the
//! command opened and, once given, its answer. A call a card cannot
//! represent has none, and clients show the ordinary command row.

use serde::{Deserialize, Serialize};

use crate::tasks_md::edit::Op;
use crate::tasks_md::{Row, Section, TasksFile};

/// What a `terminal` call of a May command shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Card {
    /// `tasks`: the row it changed.
    Tasks {
        /// The changes, in the order the command made them.
        changes: Vec<RowChange>,
    },
    /// `subagent`: one run.
    Subagent {
        /// The subagent type the run uses; `resume` for a resumed run.
        agent: String,
        /// The brief's title: its first line.
        title: String,
        /// Where the run is.
        state: SubagentState,
        /// When the run started; clients compute elapsed time from it.
        #[schema(value_type = String)]
        started_at: jiff::Timestamp,
        /// When the run ended, once it did.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Option<String>)]
        finished_at: Option<jiff::Timestamp>,
        /// The run's full result, once it finished.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
    /// `ask_user`: a question put to the user.
    AskUser {
        /// The request the question was stored under.
        request_id: String,
        /// The questions asked.
        questions: Vec<AskedCard>,
        /// The answers once given, one per question.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answers: Option<Vec<AnswerCard>>,
    },
}

/// How a `tasks` command changed a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum RowChangeKind {
    /// A row was added.
    Added,
    /// A queued row moved to doing.
    Started,
    /// A row was deleted: its work is finished.
    Finished,
    /// A row was marked `BLOCKING`.
    Blocked,
    /// A row lost its `BLOCKING` mark.
    Unblocked,
    /// A row's title or detail was rewritten.
    Edited,
}

/// One row changed by a `tasks` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RowChange {
    /// What happened to the row.
    pub change: RowChangeKind,
    /// The row before the command; absent for an added row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<RowView>,
    /// The row after the command; absent for a finished row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<RowView>,
}

/// A `tasks.md` row as a card shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RowView {
    /// The section heading it sits under.
    pub section: String,
    /// Its title line.
    pub title: String,
    /// Its detail lines, joined by newlines; empty when it has none.
    pub detail: String,
}

/// Where a subagent run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum SubagentState {
    /// The run is working.
    Running,
    /// The run returned its result.
    Done,
    /// The daemon stopped while the run was working; it never returned.
    Interrupted,
}

/// A question an `ask_user` card shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskedCard {
    /// Short section label; answers are matched to questions by it.
    pub section: String,
    /// The question text.
    pub question: String,
    /// The options offered.
    pub options: Vec<String>,
    /// Whether several options may be chosen.
    pub multi_select: bool,
    /// Whether the question asks for a secret, whose value is never shown.
    pub secret: bool,
}

/// An answer an `ask_user` card shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AnswerCard {
    /// The section of the question answered.
    pub section: String,
    /// What was chosen or typed; `(skipped)` for a skipped question.
    pub text: String,
}

impl From<&Row> for RowView {
    fn from(row: &Row) -> Self {
        Self {
            section: row.section().heading().to_owned(),
            title: row.title().to_owned(),
            detail: row.details().collect::<Vec<_>>().join("\n"),
        }
    }
}

impl Card {
    /// The card of a question just opened, unanswered.
    #[must_use]
    pub fn asked(request: &crate::session::PersistedAskUserRequest) -> Self {
        Self::AskUser {
            request_id: request.id.clone(),
            questions: request
                .questions
                .iter()
                .map(|question| AskedCard {
                    section: question.section.clone(),
                    question: question.question.clone(),
                    options: question.option.clone(),
                    multi_select: question.multi_select,
                    secret: question.secret.is_some(),
                })
                .collect(),
            answers: None,
        }
    }

    /// This card with the answers committed for its question. The value of a
    /// secret question is never shown. Any other card is unchanged.
    #[must_use]
    pub fn answered(mut self, committed: &[crate::ask_user::QuestionAnswer]) -> Self {
        if let Self::AskUser {
            questions, answers, ..
        } = &mut self
        {
            *answers = Some(
                committed
                    .iter()
                    .map(|answer| AnswerCard {
                        section: answer.section.clone(),
                        text: if questions
                            .iter()
                            .any(|q| q.secret && q.section == answer.section)
                        {
                            "(hidden)".to_owned()
                        } else {
                            answer.answer.to_string()
                        },
                    })
                    .collect(),
            );
        }
        self
    }

    /// This card with a running subagent settled as interrupted. Any other
    /// card, a finished subagent included, is unchanged.
    #[must_use]
    pub fn interrupted(mut self) -> Self {
        if let Self::Subagent { state, .. } = &mut self
            && *state == SubagentState::Running
        {
            *state = SubagentState::Interrupted;
        }
        self
    }

    /// The title of a subagent brief: its first non-empty line without
    /// markdown heading marks.
    #[must_use]
    pub fn brief_title(brief: &str) -> String {
        brief
            .lines()
            .map(|line| line.trim().trim_start_matches('#').trim())
            .find(|line| !line.is_empty())
            .unwrap_or_default()
            .to_owned()
    }

    /// The `tasks` card for `op`, which turned `before` into `after`; `None`
    /// when the command changed nothing the card represents (a listing, a
    /// note, a refusal).
    #[must_use]
    pub fn tasks(op: &Op, before: &str, after: &str) -> Option<Self> {
        let before = TasksFile::parse(before);
        let after = TasksFile::parse(after);
        let (removed, added) = row_difference(before.rows(), after.rows());
        let row = |number: usize| before.rows().get(number.checked_sub(1)?);
        let change = match op {
            Op::Add { .. } => RowChange {
                change: RowChangeKind::Added,
                before: None,
                after: Some(RowView::from(*added.first()?)),
            },
            Op::Start(number) => RowChange {
                change: RowChangeKind::Started,
                before: Some(RowView::from(row(*number)?)),
                after: Some(RowView::from(*added.first()?)),
            },
            Op::Finish(number) => RowChange {
                change: RowChangeKind::Finished,
                before: Some(RowView::from(row(*number)?)),
                after: None,
            },
            Op::Block { row: number, .. } => {
                rewritten(RowChangeKind::Blocked, row(*number)?, &added)?
            }
            Op::Unblock(number) => rewritten(RowChangeKind::Unblocked, row(*number)?, &added)?,
            Op::Edit { row: number, .. } => {
                rewritten(RowChangeKind::Edited, row(*number)?, &added)?
            }
            Op::Note(_) | Op::Decide(_) | Op::List => return None,
        };
        // A row the command was asked to change but the file kept is no change.
        let moved = !removed.is_empty() || !added.is_empty();
        moved.then(|| Self::Tasks {
            changes: vec![change],
        })
    }
}

fn rewritten(kind: RowChangeKind, before: &Row, added: &[&Row]) -> Option<RowChange> {
    Some(RowChange {
        change: kind,
        before: Some(RowView::from(before)),
        after: Some(RowView::from(*added.first()?)),
    })
}

/// The rows only `before` holds and the rows only `after` holds, matched by
/// section and text.
fn row_difference<'a>(before: &'a [Row], after: &'a [Row]) -> (Vec<&'a Row>, Vec<&'a Row>) {
    let key = |row: &Row| -> (Section, String) { (row.section(), row.text().to_owned()) };
    let mut unmatched: Vec<&Row> = before.iter().collect();
    let mut added = Vec::new();
    for row in after {
        let found = unmatched
            .iter()
            .position(|candidate| key(candidate) == key(row));
        match found {
            Some(index) => {
                unmatched.remove(index);
            }
            None => added.push(row),
        }
    }
    (unmatched, added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(args: &[&str]) -> Op {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        crate::tasks_md::edit::parse(&args).expect("the arguments name an operation")
    }

    const BEFORE: &str = "## Doing\n- fix the parser\n\n## Queued\n- write docs\n  cover the CLI\n";

    #[test]
    fn an_added_row_is_one_added_change() {
        let after = format!("{BEFORE}- ship it\n");
        let card = Card::tasks(&op(&["add", "--title", "ship it"]), BEFORE, &after);
        let Some(Card::Tasks { changes }) = card else {
            panic!("a tasks card is built");
        };
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, RowChangeKind::Added);
        assert_eq!(changes[0].before, None);
        assert_eq!(changes[0].after.as_ref().expect("after").title, "ship it");
    }

    #[test]
    fn a_started_row_moves_between_sections() {
        let after = "## Doing\n- fix the parser\n- write docs\n  cover the CLI\n\n## Queued\n";
        let Some(Card::Tasks { changes }) = Card::tasks(&op(&["start", "2"]), BEFORE, after) else {
            panic!("a tasks card is built");
        };
        let change = &changes[0];
        assert_eq!(change.change, RowChangeKind::Started);
        let (before, after) = (
            change.before.as_ref().unwrap(),
            change.after.as_ref().unwrap(),
        );
        assert_eq!(
            (before.section.as_str(), after.section.as_str()),
            ("Queued", "Doing")
        );
        assert_eq!(after.detail, "cover the CLI");
    }

    #[test]
    fn a_finished_row_has_no_after() {
        let after = "## Doing\n\n## Queued\n- write docs\n  cover the CLI\n";
        let Some(Card::Tasks { changes }) = Card::tasks(&op(&["finish", "1"]), BEFORE, after)
        else {
            panic!("a tasks card is built");
        };
        assert_eq!(changes[0].change, RowChangeKind::Finished);
        assert_eq!(changes[0].after, None);
        assert_eq!(changes[0].before.as_ref().unwrap().title, "fix the parser");
    }

    #[test]
    fn a_blocked_row_shows_before_and_after() {
        let after = "## Doing\n- fix the parser BLOCKING review\n\n## Queued\n- write docs\n  cover the CLI\n";
        let Some(Card::Tasks { changes }) =
            Card::tasks(&op(&["block", "1", "--on", "review"]), BEFORE, after)
        else {
            panic!("a tasks card is built");
        };
        assert_eq!(changes[0].change, RowChangeKind::Blocked);
        assert!(
            changes[0]
                .after
                .as_ref()
                .unwrap()
                .title
                .contains("BLOCKING")
        );
    }

    #[test]
    fn a_listing_or_an_unchanged_file_has_no_card() {
        assert_eq!(Card::tasks(&op(&["list"]), BEFORE, BEFORE), None);
        assert_eq!(Card::tasks(&op(&["finish", "1"]), BEFORE, BEFORE), None);
    }

    /// The web UI reads these shapes (`web-ui/src/lib/types.ts`).
    #[test]
    fn cards_serialize_in_the_shape_the_web_ui_reads() {
        let subagent = Card::Subagent {
            agent: "explorer".to_owned(),
            title: "Map".to_owned(),
            state: SubagentState::Running,
            started_at: jiff::Timestamp::UNIX_EPOCH,
            finished_at: None,
            result: None,
        };
        let json = serde_json::to_value(&subagent).expect("serialize");
        assert_eq!(json["kind"], "subagent");
        assert_eq!(json["state"], "running");
        assert_eq!(json["startedAt"], "1970-01-01T00:00:00Z");
        assert!(json.get("finishedAt").is_none());

        let ask = Card::AskUser {
            request_id: "r".to_owned(),
            questions: vec![AskedCard {
                section: "s".to_owned(),
                question: "q".to_owned(),
                options: Vec::new(),
                multi_select: true,
                secret: false,
            }],
            answers: None,
        };
        let json = serde_json::to_value(&ask).expect("serialize");
        assert_eq!(json["kind"], "askUser");
        assert_eq!(json["requestId"], "r");
        assert_eq!(json["questions"][0]["multiSelect"], true);
    }

    #[test]
    fn a_secret_answer_is_never_shown() {
        let card = Card::AskUser {
            request_id: "r".to_owned(),
            questions: vec![AskedCard {
                section: "TOKEN".to_owned(),
                question: "Token?".to_owned(),
                options: Vec::new(),
                multi_select: false,
                secret: true,
            }],
            answers: None,
        };
        let committed = [crate::ask_user::QuestionAnswer {
            section: "TOKEN".to_owned(),
            answer: crate::ask_user::AnswerValue::Custom("hunter2".to_owned()),
        }];
        let Card::AskUser {
            answers: Some(answers),
            ..
        } = card.answered(&committed)
        else {
            panic!("answered");
        };
        assert_eq!(answers[0].text, "(hidden)");
    }

    #[test]
    fn a_brief_title_is_its_first_line_without_heading_marks() {
        assert_eq!(
            Card::brief_title("\n## Port the parser\n\nbody"),
            "Port the parser"
        );
        assert_eq!(Card::brief_title(""), "");
    }
}
