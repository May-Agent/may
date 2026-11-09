//! The files an `apply_patch` patch changes.
//!
//! The text is parsed by `aither-patch`, the parser the harness applies a
//! patch with, so a patch reads exactly as it will apply. Given the directory
//! the patch runs in, each hunk is located in the file it edits with the
//! applier's own matching and its lines carry absolute line numbers; without
//! the directory, or when a hunk does not place, an updated line carries no
//! number. A new file is numbered from its first line.

use std::path::Path;

use aither_patch::{Action, HunkLine, locate_hunks};
use serde::{Deserialize, Serialize};

/// One file a patch changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileEdit {
    /// The file as the patch names it.
    pub path: String,
    /// What the patch does to it.
    pub change: FileChange,
}

/// What a patch does to one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FileChange {
    /// The file is created with these lines.
    Add {
        /// The file's lines, all added.
        lines: Vec<DiffLine>,
    },
    /// The file is deleted.
    Delete,
    /// The file is edited, and moved when `moved_to` is set.
    Update {
        /// Where the file moves to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        moved_to: Option<String>,
        /// The edited regions.
        hunks: Vec<Hunk>,
    },
}

/// A run of context, removed and added lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Hunk {
    /// The `@@` anchor line, when the patch gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    /// The hunk's lines in order.
    pub lines: Vec<DiffLine>,
}

/// How a diff line relates to the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum DiffOp {
    /// Unchanged, shown for position.
    Context,
    /// Inserted.
    Added,
    /// Taken out.
    Removed,
}

/// One line of a diff with its line numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiffLine {
    /// Whether the line is context, added or removed.
    pub op: DiffOp,
    /// The line without its marker.
    pub text: String,
    /// Line number in the file before the patch; absent for an added line and
    /// for a line of a hunk that was not located.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<u32>,
    /// Line number in the file after the patch; absent for a removed line and
    /// for a line of a hunk that was not located.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<u32>,
}

/// Reads `text` as a patch, or `None` when `aither-patch` does not parse it.
/// `dir` is where the patch runs: the files it updates are read from there to
/// number the hunks.
pub(super) fn parse(text: &str, dir: Option<&Path>) -> Option<Vec<FileEdit>> {
    let actions = aither_patch::parse(text).ok()?;
    (!actions.is_empty()).then(|| actions.iter().map(|action| edit(action, dir)).collect())
}

fn edit(action: &Action, dir: Option<&Path>) -> FileEdit {
    match action {
        Action::Add { path, lines } => FileEdit {
            path: path.clone(),
            change: FileChange::Add {
                lines: lines
                    .iter()
                    .zip(1..)
                    .map(|(text, number)| DiffLine {
                        op: DiffOp::Added,
                        text: text.clone(),
                        old: None,
                        new: Some(number),
                    })
                    .collect(),
            },
        },
        Action::Delete { path, .. } => FileEdit {
            path: path.clone(),
            change: FileChange::Delete,
        },
        Action::Update {
            path,
            new_path,
            hunks,
        } => {
            let file = dir.and_then(|dir| std::fs::read_to_string(dir.join(path)).ok());
            let starts = file.map(|content| {
                let lines: Vec<String> = content.lines().map(String::from).collect();
                locate_hunks(&lines, hunks).ranges
            });
            let mut shift = 0_i64;
            let hunks = hunks
                .iter()
                .enumerate()
                .map(|(index, hunk)| {
                    let start = starts
                        .as_ref()
                        .and_then(|ranges| ranges[index].as_ref())
                        .map(|range| range.start);
                    let lines = number(hunk, start, shift);
                    shift += hunk
                        .lines
                        .iter()
                        .map(|line| match line {
                            HunkLine::Context(_) => 0,
                            HunkLine::Delete(_) => -1,
                            HunkLine::Insert(_) => 1,
                        })
                        .sum::<i64>();
                    Hunk {
                        anchor: hunk.anchor.clone(),
                        lines,
                    }
                })
                .collect();
            FileEdit {
                path: path.clone(),
                change: FileChange::Update {
                    moved_to: new_path.clone(),
                    hunks,
                },
            }
        }
    }
}

/// The hunk's lines, numbered from where its old side starts in the file
/// (zero-based `start`); `shift` is how many lines the earlier hunks of the
/// file added net. A hunk that was not located is left unnumbered.
fn number(hunk: &aither_patch::Hunk, start: Option<usize>, shift: i64) -> Vec<DiffLine> {
    let mut position = start.map(|start| {
        let old = u32::try_from(start + 1).expect("a file has fewer than 2^32 lines");
        let new = u32::try_from(i64::from(old) + shift)
            .expect("a located hunk's new line number is positive");
        (old, new)
    });
    hunk.lines
        .iter()
        .map(|line| {
            let (op, text) = match line {
                HunkLine::Context(text) => (DiffOp::Context, text),
                HunkLine::Delete(text) => (DiffOp::Removed, text),
                HunkLine::Insert(text) => (DiffOp::Added, text),
            };
            let (old, new) = match (&mut position, op) {
                (None, _) => (None, None),
                (Some((old, new)), DiffOp::Context) => {
                    let numbers = (Some(*old), Some(*new));
                    *old += 1;
                    *new += 1;
                    numbers
                }
                (Some((old, _)), DiffOp::Removed) => {
                    let numbers = (Some(*old), None);
                    *old += 1;
                    numbers
                }
                (Some((_, new)), DiffOp::Added) => {
                    let numbers = (None, Some(*new));
                    *new += 1;
                    numbers
                }
            };
            DiffLine {
                op,
                text: text.clone(),
                old,
                new,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_kind_of_file_change() {
        let patch = "\
*** Begin Patch
*** Update File: src/main.rs
@@ fn main
 let args = parse();
-run(args);
+run(&args);
*** Add File: notes/todo.txt
+first
+second
*** Update File: old/name.rs
*** Move to: new/name.rs
*** Delete File: stale.txt
*** End Patch";
        let edits = parse(patch, None).expect("a patch");
        assert_eq!(edits.len(), 4);
        assert_eq!(edits[0].path, "src/main.rs");
        let FileChange::Update { hunks, moved_to } = &edits[0].change else {
            panic!("update expected");
        };
        assert_eq!(moved_to, &None);
        assert_eq!(hunks[0].anchor.as_deref(), Some("fn main"));
        assert!(
            hunks[0]
                .lines
                .iter()
                .all(|l| l.old.is_none() && l.new.is_none())
        );
        let FileChange::Add { lines } = &edits[1].change else {
            panic!("add expected");
        };
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].new, Some(2));
        assert_eq!(lines[1].text, "second");
        let FileChange::Update { moved_to, hunks } = &edits[2].change else {
            panic!("move expected");
        };
        assert_eq!(moved_to.as_deref(), Some("new/name.rs"));
        assert_eq!(hunks, &[]);
        assert_eq!(edits[3].change, FileChange::Delete);
    }

    #[test]
    fn text_that_is_not_a_patch_is_refused() {
        assert!(parse("hello", None).is_none());
        assert!(parse("*** Begin Patch\n*** End Patch", None).is_none());
    }

    #[test]
    fn located_hunks_carry_absolute_line_numbers() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(
            dir.path().join("a.txt"),
            "one\ntwo\nthree\nfour\nfive\nsix\nseven\n",
        )
        .expect("write");
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n two\n-three\n+3\n+3b\n four\n@@\n-six\n+6\n seven\n*** End Patch";
        let edits = parse(patch, Some(dir.path())).expect("a patch");
        let FileChange::Update { hunks, .. } = &edits[0].change else {
            panic!("update expected");
        };
        let numbers = |hunk: &Hunk| {
            hunk.lines
                .iter()
                .map(|l| (l.old, l.new))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            numbers(&hunks[0]),
            [
                (Some(2), Some(2)),
                (Some(3), None),
                (None, Some(3)),
                (None, Some(4)),
                (Some(4), Some(5)),
            ]
        );
        assert_eq!(
            numbers(&hunks[1]),
            [(Some(6), None), (None, Some(7)), (Some(7), Some(8))]
        );
    }

    #[test]
    fn a_hunk_that_does_not_place_stays_unnumbered() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("a.txt"), "one\n").expect("write");
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-absent\n+x\n*** End Patch";
        let edits = parse(patch, Some(dir.path())).expect("a patch");
        let FileChange::Update { hunks, .. } = &edits[0].change else {
            panic!("update expected");
        };
        assert!(
            hunks[0]
                .lines
                .iter()
                .all(|l| l.old.is_none() && l.new.is_none())
        );
    }
}
