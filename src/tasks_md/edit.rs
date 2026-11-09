//! The `tasks` command: edits the session's `tasks.md` in place.
//!
//! An edit is applied to the file's lines, the result is parsed with the same
//! [`TasksFile`] parser the harness reads the document with, and the edit is
//! refused, nothing written, when the result has a violation the file did not
//! have or any row but the addressed one changed. Rows are addressed by the
//! numbers [`numbered`] shows the model: their position over every row, from
//! 1.

use std::fmt;
use std::io::Write as _;
use std::path::Path;

use aither_agent::working_document::words;
use askama::Template;

use super::schema::violation_sentence;
use super::{Row, Section, TasksFile, numbered};

#[cfg(test)]
mod tests;

/// What the `tasks` command was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Appends a row to a section.
    Add {
        /// The row section it goes under.
        section: Section,
        /// The row's title line.
        title: String,
        /// The row's detail lines.
        details: Vec<String>,
    },
    /// Moves a `Queued` row to `Doing`.
    Start(usize),
    /// Deletes a row: its work is finished.
    Finish(usize),
    /// Marks a row `BLOCKING <what>`.
    Block {
        /// The row's number.
        row: usize,
        /// What it waits on.
        what: String,
    },
    /// Removes a row's `BLOCKING` mark.
    Unblock(usize),
    /// Rewrites a row's title and detail.
    Edit {
        /// The row's number.
        row: usize,
        /// The new title, when it changes.
        title: Option<String>,
        /// The new detail lines, when they change; a `BLOCKING` mark stays.
        details: Option<Vec<String>>,
    },
    /// Appends text to `Notes`.
    Note(String),
    /// Appends text to `Decisions`.
    Decide(String),
    /// Shows the numbered document.
    List,
}

/// Why the arguments name no operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Usage {
    /// No subcommand.
    NoCommand,
    /// A subcommand that does not exist.
    UnknownCommand(String),
    /// A row number that is not a positive integer, or missing.
    BadRow(String),
    /// A flag the subcommand does not take.
    UnknownFlag(String),
    /// A flag without its value.
    MissingValue(&'static str),
    /// `add` without `--title`.
    MissingTitle,
    /// `--section` naming no row section.
    BadSection(String),
    /// A subcommand that needs text, given none.
    MissingText,
    /// Arguments the subcommand does not take.
    Unexpected(String),
}

/// Why a well-formed operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The number names no row.
    NoSuchRow {
        /// The number given.
        row: usize,
        /// The rows the document has.
        rows: usize,
    },
    /// `start` on a row that is not under `Queued`.
    NotQueued {
        /// The row's number.
        row: usize,
        /// Where it is.
        section: Section,
    },
    /// `block` or `unblock` on a row of `Roots`.
    NotWork {
        /// The row's number.
        row: usize,
    },
    /// A title that is empty.
    EmptyTitle,
    /// A title of more than one line.
    MultilineTitle,
    /// A title that is a checkbox.
    CheckboxTitle,
    /// Text to append that is empty.
    EmptyText,
    /// A `BLOCKING` mark that names nothing, or more than one line.
    EmptyBlocker,
    /// The result would break the format or change another row; the
    /// sentences name each violation, when there is one.
    Malformed(Vec<String>),
}

/// What an applied operation did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Added as this row.
    Added(usize),
    /// Moved to `Doing` as this row.
    Started(usize),
    /// Deleted; this was its number.
    Finished(usize),
    /// Marked blocked.
    Blocked(usize),
    /// Mark removed.
    Unblocked(usize),
    /// Rewritten.
    Edited(usize),
    /// Text appended to `Notes`.
    Noted,
    /// Text appended to `Decisions`.
    Decided,
    /// Nothing changed.
    Listed,
}

/// An applied operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The document after it.
    pub content: String,
    /// What it did.
    pub outcome: Outcome,
}

/// Why the command failed.
#[derive(Debug)]
pub enum Error {
    /// The arguments name no operation.
    Usage(Usage),
    /// The operation was refused.
    Refused(Refusal),
    /// The environment names no `tasks.md`: the command runs outside a May
    /// session.
    Unset,
    /// The file could not be read or written.
    Io {
        /// The file.
        path: String,
        /// The failure.
        source: std::io::Error,
    },
}

#[derive(Template)]
#[template(path = "tasks_command_usage.txt", escape = "none")]
struct UsageTemplate<'a> {
    problem: Option<&'a Usage>,
}

#[derive(Template)]
#[template(path = "tasks_command_refused.txt", escape = "none")]
struct RefusedTemplate<'a> {
    refusal: &'a Refusal,
}

#[derive(Template)]
#[template(path = "tasks_command_done.txt", escape = "none")]
struct DoneTemplate<'a> {
    outcome: Outcome,
    document: &'a str,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(usage) => UsageTemplate {
                problem: Some(usage),
            }
            .render_into(f)
            .map_err(|_| fmt::Error),
            Self::Refused(refusal) => RefusedTemplate { refusal }
                .render_into(f)
                .map_err(|_| fmt::Error),
            Self::Unset => write!(
                f,
                "{} is not set, so there is no tasks.md to edit; tasks runs only inside a May session.",
                super::ENV
            ),
            Self::Io { path, source } => write!(f, "{path}: {source}"),
        }
    }
}

impl std::error::Error for Error {}

/// The usage text.
///
/// # Panics
/// Never: the template renders typed input.
#[must_use]
pub fn usage() -> String {
    UsageTemplate { problem: None }
        .render()
        .expect("the tasks usage template renders typed input")
}

fn row_number(arguments: &mut std::slice::Iter<'_, String>) -> Result<usize, Usage> {
    let given = arguments
        .next()
        .ok_or_else(|| Usage::BadRow(String::new()))?;
    given
        .parse::<usize>()
        .ok()
        .filter(|row| *row > 0)
        .ok_or_else(|| Usage::BadRow(given.clone()))
}

fn no_more(arguments: &mut std::slice::Iter<'_, String>) -> Result<(), Usage> {
    arguments
        .next()
        .map_or(Ok(()), |extra| Err(Usage::Unexpected(extra.clone())))
}

fn rest(arguments: std::slice::Iter<'_, String>) -> String {
    arguments.map(String::as_str).collect::<Vec<_>>().join(" ")
}

/// A detail value as lines: trimmed, blank lines dropped.
fn detail_lines(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
}

/// Reads the `tasks` command's arguments.
///
/// # Errors
/// Fails with the [`Usage`] the arguments break.
pub fn parse(args: &[String]) -> Result<Op, Usage> {
    let mut arguments = args.iter();
    let command = arguments.next().ok_or(Usage::NoCommand)?;
    match command.as_str() {
        "add" | "edit" => {
            let adding = command == "add";
            let row = if adding {
                0
            } else {
                row_number(&mut arguments)?
            };
            let mut section = Section::Queued;
            let mut title = None;
            let mut details: Option<Vec<String>> = None;
            while let Some(flag) = arguments.next() {
                let mut value =
                    |name: &'static str| arguments.next().cloned().ok_or(Usage::MissingValue(name));
                match flag.as_str() {
                    "--title" => title = Some(value("--title")?),
                    "--detail" => details
                        .get_or_insert_with(Vec::new)
                        .extend(detail_lines(&value("--detail")?)),
                    "--clear-detail" if !adding => details = Some(Vec::new()),
                    "--section" if adding => {
                        let given = value("--section")?;
                        section = match given.as_str() {
                            "roots" => Section::Roots,
                            "doing" => Section::Doing,
                            "queued" => Section::Queued,
                            _ => return Err(Usage::BadSection(given)),
                        };
                    }
                    other => return Err(Usage::UnknownFlag(other.to_string())),
                }
            }
            if adding {
                Ok(Op::Add {
                    section,
                    title: title.ok_or(Usage::MissingTitle)?,
                    details: details.unwrap_or_default(),
                })
            } else {
                Ok(Op::Edit {
                    row,
                    title,
                    details,
                })
            }
        }
        "start" | "finish" | "unblock" => {
            let row = row_number(&mut arguments)?;
            no_more(&mut arguments)?;
            Ok(match command.as_str() {
                "start" => Op::Start(row),
                "finish" => Op::Finish(row),
                _ => Op::Unblock(row),
            })
        }
        "block" => {
            let row = row_number(&mut arguments)?;
            let what = rest(arguments);
            if what.is_empty() {
                return Err(Usage::MissingText);
            }
            Ok(Op::Block { row, what })
        }
        "note" | "decide" => {
            let text = rest(arguments);
            if text.trim().is_empty() {
                return Err(Usage::MissingText);
            }
            Ok(if command == "note" {
                Op::Note(text)
            } else {
                Op::Decide(text)
            })
        }
        "list" => {
            no_more(&mut arguments)?;
            Ok(Op::List)
        }
        other => Err(Usage::UnknownCommand(other.to_string())),
    }
}

/// A row as the parser reads it: its section and text.
type Item = (Section, String);

fn items(file: &TasksFile) -> Vec<Item> {
    file.rows()
        .iter()
        .map(|row| (row.section(), row.text().to_string()))
        .collect()
}

/// The rows regardless of order, which a move changes.
fn unordered(items: Vec<Item>) -> Vec<(usize, String)> {
    let mut keyed: Vec<(usize, String)> = items
        .into_iter()
        .map(|(section, text)| {
            let index = Section::ALL
                .iter()
                .position(|other| *other == section)
                .unwrap_or_default();
            (index, text)
        })
        .collect();
    keyed.sort();
    keyed
}

fn text_of(title: &str, details: &[String]) -> String {
    std::iter::once(title)
        .chain(details.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn row_lines(title: &str, details: &[String]) -> Vec<String> {
    std::iter::once(format!("- {title}"))
        .chain(details.iter().map(|detail| format!("  {detail}")))
        .collect()
}

fn is_blocking_line(line: &str) -> bool {
    words(line).any(|word| word == "BLOCKING")
}

fn checked_title(title: &str) -> Result<String, Refusal> {
    let title = title.trim();
    if title.is_empty() {
        return Err(Refusal::EmptyTitle);
    }
    if title.lines().count() > 1 {
        return Err(Refusal::MultilineTitle);
    }
    if ["[ ]", "[x]", "[X]"]
        .iter()
        .any(|checkbox| title.starts_with(checkbox))
    {
        return Err(Refusal::CheckboxTitle);
    }
    Ok(title.to_string())
}

fn row_at(file: &TasksFile, row: usize) -> Result<&Row, Refusal> {
    row.checked_sub(1)
        .and_then(|index| file.rows().get(index))
        .ok_or_else(|| Refusal::NoSuchRow {
            row,
            rows: file.rows().len(),
        })
}

fn details_of(row: &Row) -> Vec<String> {
    row.details().map(ToString::to_string).collect()
}

fn unblocked_details(row: &Row) -> Vec<String> {
    row.details()
        .filter(|line| !is_blocking_line(line))
        .map(ToString::to_string)
        .collect()
}

/// Removes `row`'s lines.
fn remove_row(lines: &mut Vec<String>, row: &Row) {
    lines.drain(row.line() - 1..row.last_line());
}

/// Rewrites `row` in place as `title` and `details`.
fn replace_row(lines: &mut Vec<String>, row: &Row, title: &str, details: &[String]) {
    lines.splice(row.line() - 1..row.last_line(), row_lines(title, details));
}

/// Appends `new` to the end of `section`, creating the section in its place
/// when the document has none.
fn append_to_section(lines: &mut Vec<String>, section: Section, new: Vec<String>) {
    let file = TasksFile::parse(&lines.join("\n"));
    if let Some(span) = file.spans().iter().find(|span| span.section == section) {
        let at = span.last_line;
        lines.splice(at..at, new);
        return;
    }
    let order = |section: Section| Section::ALL.iter().position(|other| *other == section);
    let following = file
        .spans()
        .iter()
        .find(|span| order(span.section) > order(section));
    let heading = format!("## {}", section.heading());
    if let Some(span) = following {
        let at = span.heading - 1;
        let block: Vec<String> = std::iter::once(heading)
            .chain(new)
            .chain(std::iter::once(String::new()))
            .collect();
        lines.splice(at..at, block);
    } else {
        if lines.last().is_some_and(|last| !last.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(heading);
        lines.extend(new);
    }
}

/// Applies `op` to `content`.
///
/// # Errors
/// Fails with the [`Refusal`] when the operation addresses no row, is not
/// valid for its row, or would leave the document malformed.
pub fn apply(content: &str, op: &Op) -> Result<Applied, Refusal> {
    let old = TasksFile::parse(content);
    let mut lines: Vec<String> = content.lines().map(ToString::to_string).collect();
    let mut expected = items(&old);
    // The section and text of the row an add or start leaves behind, to
    // number it.
    let mut placed: Option<Item> = None;
    let outcome = match op {
        Op::List => {
            return Ok(Applied {
                content: content.to_string(),
                outcome: Outcome::Listed,
            });
        }
        Op::Add {
            section,
            title,
            details,
        } => {
            let title = checked_title(title)?;
            append_to_section(&mut lines, *section, row_lines(&title, details));
            let text = text_of(&title, details);
            expected.push((*section, text.clone()));
            placed = Some((*section, text));
            Outcome::Added(0)
        }
        Op::Start(number) => {
            let row = row_at(&old, *number)?;
            if row.section() != Section::Queued {
                return Err(Refusal::NotQueued {
                    row: *number,
                    section: row.section(),
                });
            }
            remove_row(&mut lines, row);
            append_to_section(
                &mut lines,
                Section::Doing,
                row_lines(row.title(), &details_of(row)),
            );
            expected[number - 1].0 = Section::Doing;
            placed = Some((Section::Doing, row.text().to_string()));
            Outcome::Started(0)
        }
        Op::Finish(number) => {
            let row = row_at(&old, *number)?;
            remove_row(&mut lines, row);
            expected.remove(number - 1);
            Outcome::Finished(*number)
        }
        Op::Block { row: number, what } => {
            let row = row_at(&old, *number)?;
            if !row.section().is_open_work() {
                return Err(Refusal::NotWork { row: *number });
            }
            let what = what.trim();
            if what.is_empty() || what.lines().count() > 1 {
                return Err(Refusal::EmptyBlocker);
            }
            let mut details = unblocked_details(row);
            details.push(format!("BLOCKING {what}"));
            replace_row(&mut lines, row, row.title(), &details);
            expected[number - 1].1 = text_of(row.title(), &details);
            Outcome::Blocked(*number)
        }
        Op::Unblock(number) => {
            let row = row_at(&old, *number)?;
            if !row.section().is_open_work() {
                return Err(Refusal::NotWork { row: *number });
            }
            let details = unblocked_details(row);
            replace_row(&mut lines, row, row.title(), &details);
            expected[number - 1].1 = text_of(row.title(), &details);
            Outcome::Unblocked(*number)
        }
        Op::Edit {
            row: number,
            title,
            details,
        } => {
            let row = row_at(&old, *number)?;
            let title = match title {
                Some(title) => checked_title(title)?,
                None => row.title().to_string(),
            };
            let details = details.as_ref().map_or_else(
                || details_of(row),
                |new| {
                    new.iter()
                        .cloned()
                        .chain(
                            row.details()
                                .filter(|line| is_blocking_line(line))
                                .map(ToString::to_string),
                        )
                        .collect()
                },
            );
            replace_row(&mut lines, row, &title, &details);
            expected[number - 1].1 = text_of(&title, &details);
            Outcome::Edited(*number)
        }
        Op::Note(text) | Op::Decide(text) => {
            let (section, outcome) = if matches!(op, Op::Note(_)) {
                (Section::Notes, Outcome::Noted)
            } else {
                (Section::Decisions, Outcome::Decided)
            };
            let new: Vec<String> = text.lines().map(ToString::to_string).collect();
            if new.iter().all(|line| line.trim().is_empty()) {
                return Err(Refusal::EmptyText);
            }
            append_to_section(&mut lines, section, new);
            outcome
        }
    };
    let mut written = lines.join("\n");
    written.push('\n');
    let new = TasksFile::parse(&written);
    // A section heading the edit swallowed (into a code fence) is gone.
    let sections_kept = old
        .spans()
        .iter()
        .all(|span| new.spans().iter().any(|kept| kept.section == span.section));
    if new.violations().len() > old.violations().len()
        || !sections_kept
        || unordered(items(&new)) != unordered(expected)
    {
        return Err(Refusal::Malformed(
            new.violations().iter().map(violation_sentence).collect(),
        ));
    }
    let number_of_placed = || {
        placed
            .as_ref()
            .and_then(|(section, text)| {
                new.rows()
                    .iter()
                    .rposition(|row| row.section() == *section && row.text() == text)
            })
            .map_or(0, |index| index + 1)
    };
    let outcome = match outcome {
        Outcome::Added(_) => Outcome::Added(number_of_placed()),
        Outcome::Started(_) => Outcome::Started(number_of_placed()),
        other => other,
    };
    Ok(Applied {
        content: written,
        outcome,
    })
}

/// A row an answered question held up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unblocked {
    /// The row's number, as [`numbered`] shows it.
    pub number: usize,
    /// Its title, after the mark is gone.
    pub title: String,
}

/// `content` with every `BLOCKING <request_id>` mark removed, and the rows
/// that held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleared {
    /// The document after it; `content` itself when no mark named the id.
    pub content: String,
    /// The rows whose mark is gone, in document order: pending again.
    pub rows: Vec<Unblocked>,
}

/// The line without its trailing `BLOCKING <request_id>` mark, markdown
/// around the mark included; `None` when the line's tail is not exactly the
/// mark.
fn without_mark(line: &str, request_id: &str) -> Option<String> {
    let at = line.find("BLOCKING")?;
    if !words(&line[at..]).eq(["BLOCKING", request_id]) {
        return None;
    }
    Some(
        line[..at]
            .trim_end_matches(|c: char| c.is_whitespace() || "([*`:,;-\u{2014}".contains(c))
            .to_string(),
    )
}

/// Removes the `BLOCKING <request_id>` mark of every row it marks, so those
/// rows are pending again.
///
/// A row whose mark is not the tail of one of its lines keeps it, and is not
/// among the rows returned.
///
/// # Errors
/// Fails with [`Refusal::Malformed`] when the result would break the format
/// or change a row that carried no such mark; nothing is to be written then.
pub fn clear_question(content: &str, request_id: &str) -> Result<Cleared, Refusal> {
    let old = TasksFile::parse(content);
    let mut lines: Vec<String> = content.lines().map(ToString::to_string).collect();
    // Back to front, so a row's line numbers hold while an earlier one is
    // rewritten.
    for row in old
        .rows()
        .iter()
        .rev()
        .filter(|row| row.blocking_mark() == Some(request_id))
    {
        let Some(title) = without_mark(row.title(), request_id)
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty())
            .or_else(|| (!is_blocking_line(row.title())).then(|| row.title().to_string()))
        else {
            continue;
        };
        let details: Vec<String> = row
            .details()
            .filter_map(|line| {
                without_mark(line, request_id).map_or_else(
                    || Some(line.to_string()),
                    |rest| Some(rest.trim().to_string()).filter(|rest| !rest.is_empty()),
                )
            })
            .collect();
        replace_row(&mut lines, row, &title, &details);
    }
    let mut written = lines.join("\n");
    written.push('\n');
    let new = TasksFile::parse(&written);
    let kept = new.rows().len() == old.rows().len()
        && old.rows().iter().zip(new.rows()).all(|(before, after)| {
            before.section() == after.section()
                && (before.blocking_mark() == Some(request_id) || before.text() == after.text())
        });
    if new.violations().len() > old.violations().len() || !kept {
        return Err(Refusal::Malformed(
            new.violations().iter().map(violation_sentence).collect(),
        ));
    }
    let rows: Vec<Unblocked> = old
        .rows()
        .iter()
        .zip(new.rows())
        .enumerate()
        .filter(|(_, (before, after))| {
            before.blocking_mark() == Some(request_id) && after.blocking_mark() != Some(request_id)
        })
        .map(|(index, (_, after))| Unblocked {
            number: index + 1,
            title: after.title().to_string(),
        })
        .collect();
    Ok(Cleared {
        content: if rows.is_empty() {
            content.to_string()
        } else {
            written
        },
        rows,
    })
}

/// [`clear_question`] on the file at `path`, written back whole when a mark
/// went; an absent file has none.
///
/// # Errors
/// Fails when the file cannot be read or written, or the result would be
/// malformed.
pub fn clear_question_in(path: &Path, request_id: &str) -> Result<Vec<Unblocked>, Error> {
    let io = |source| Error::Io {
        path: path.display().to_string(),
        source,
    };
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io(error)),
    };
    let cleared = clear_question(&content, request_id).map_err(Error::Refused)?;
    if cleared.content != content {
        let directory = path.parent().unwrap_or_else(|| Path::new("."));
        let mut staged = tempfile::NamedTempFile::new_in(directory).map_err(io)?;
        staged.write_all(cleared.content.as_bytes()).map_err(io)?;
        staged.persist(path).map_err(|error| io(error.error))?;
    }
    Ok(cleared.rows)
}

/// Runs the command on the file at `path`: reads it (an absent file is an
/// empty document), applies the arguments' operation and, when it changed
/// the document, writes it back whole.
///
/// Returns the text the command prints.
///
/// # Errors
/// Fails when the arguments name no operation, the operation is refused, or
/// the file cannot be read or written (a missing directory included).
///
/// # Panics
/// Never: the template renders typed input.
pub fn run(path: &Path, args: &[String]) -> Result<String, Error> {
    let io = |source| Error::Io {
        path: path.display().to_string(),
        source,
    };
    if matches!(args, [flag] if flag == "--help" || flag == "-h") {
        return Ok(usage());
    }
    let op = parse(args).map_err(Error::Usage)?;
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(io(error)),
    };
    let applied = apply(&content, &op).map_err(Error::Refused)?;
    if applied.content != content {
        let directory = path.parent().unwrap_or_else(|| Path::new("."));
        let mut staged = tempfile::NamedTempFile::new_in(directory).map_err(io)?;
        staged.write_all(applied.content.as_bytes()).map_err(io)?;
        staged.persist(path).map_err(|error| io(error.error))?;
    }
    Ok(DoneTemplate {
        outcome: applied.outcome,
        document: &numbered(&applied.content),
    }
    .render()
    .expect("the tasks done template renders typed input"))
}
