//! The files an `apply_patch` patch changes, read from the patch text alone.
//!
//! The patch is not applied or matched against any file, so a hunk carries
//! the line numbers it has within itself: both sides count from 1 at the
//! hunk's first line. A new file is numbered from its first line.

use serde::{Deserialize, Serialize};

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const UPDATE: &str = "*** Update File: ";
const DELETE: &str = "*** Delete File: ";
const MOVE: &str = "*** Move to: ";
const END_OF_FILE: &str = "*** End of File";

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
    /// Line number on the old side; absent for an added line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<u32>,
    /// Line number on the new side; absent for a removed line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<u32>,
}

/// Reads `text` as a patch, or `None` when it is not one.
pub(super) fn parse(text: &str) -> Option<Vec<FileEdit>> {
    let body = body_lines(text)?;
    let mut edits: Vec<FileEdit> = Vec::new();
    let mut open: Option<OpenFile> = None;
    for line in body {
        let directive = line.trim_end();
        if let Some(path) = directive.strip_prefix(ADD) {
            finish(&mut edits, open.take());
            open = Some(OpenFile::new(path, Kind::Add));
        } else if let Some(path) = directive.strip_prefix(UPDATE) {
            finish(&mut edits, open.take());
            open = Some(OpenFile::new(path, Kind::Update));
        } else if let Some(path) = directive.strip_prefix(DELETE) {
            finish(&mut edits, open.take());
            open = Some(OpenFile::new(path, Kind::Delete));
        } else {
            open.as_mut()?.feed(line)?;
        }
    }
    finish(&mut edits, open);
    (!edits.is_empty()).then_some(edits)
}

/// The lines between the envelope markers. A patch that starts with a file
/// directive needs no envelope.
fn body_lines(text: &str) -> Option<Vec<&str>> {
    let mut lines: Vec<&str> = text.trim().lines().collect();
    if lines.first().is_some_and(|line| line.starts_with("```")) {
        lines.remove(0);
        if lines.last().is_some_and(|line| line.trim() == "```") {
            lines.pop();
        }
    }
    let enveloped = lines.first().map(|line| line.trim_end()) == Some(BEGIN);
    if enveloped {
        lines.remove(0);
        if lines.last().map(|line| line.trim_end()) != Some(END) {
            return None;
        }
        lines.pop();
    } else if !lines
        .first()
        .is_some_and(|line| [ADD, UPDATE, DELETE].iter().any(|d| line.starts_with(d)))
    {
        return None;
    }
    Some(lines)
}

#[derive(Clone, Copy)]
enum Kind {
    Add,
    Update,
    Delete,
}

struct OpenFile {
    path: String,
    kind: Kind,
    moved_to: Option<String>,
    added: Vec<String>,
    hunks: Vec<(Option<String>, RawLines)>,
}

type RawLines = Vec<(DiffOp, String)>;

impl OpenFile {
    fn new(path: &str, kind: Kind) -> Self {
        Self {
            path: path.trim().to_owned(),
            kind,
            moved_to: None,
            added: Vec::new(),
            hunks: Vec::new(),
        }
    }

    /// Takes one body line; `None` when the line cannot belong to the file.
    fn feed(&mut self, line: &str) -> Option<()> {
        match self.kind {
            Kind::Delete => {
                // The lines of a delete section are the expected contents.
                Some(())
            }
            Kind::Add => {
                self.added.push(line.strip_prefix('+')?.to_owned());
                Some(())
            }
            Kind::Update => self.feed_update(line),
        }
    }

    fn feed_update(&mut self, line: &str) -> Option<()> {
        if let Some(target) = line.trim_end().strip_prefix(MOVE) {
            self.moved_to = Some(target.trim().to_owned());
            return Some(());
        }
        if line.trim_end() == END_OF_FILE {
            return Some(());
        }
        if let Some(anchor) = line.strip_prefix("@@") {
            let anchor = anchor.trim();
            self.hunks
                .push(((!anchor.is_empty()).then(|| anchor.to_owned()), Vec::new()));
            return Some(());
        }
        let (op, text) = match line.chars().next() {
            Some('+') => (DiffOp::Added, &line[1..]),
            Some('-') => (DiffOp::Removed, &line[1..]),
            Some(' ') => (DiffOp::Context, &line[1..]),
            None => (DiffOp::Context, ""),
            Some(_) => return None,
        };
        if self.hunks.is_empty() {
            self.hunks.push((None, Vec::new()));
        }
        self.hunks.last_mut()?.1.push((op, text.to_owned()));
        Some(())
    }
}

fn finish(edits: &mut Vec<FileEdit>, file: Option<OpenFile>) {
    let Some(file) = file else { return };
    let change = match file.kind {
        Kind::Delete => FileChange::Delete,
        Kind::Add => FileChange::Add {
            lines: number(
                file.added
                    .into_iter()
                    .map(|text| (DiffOp::Added, text))
                    .collect(),
            ),
        },
        Kind::Update => FileChange::Update {
            moved_to: file.moved_to,
            hunks: file
                .hunks
                .into_iter()
                .filter_map(|(anchor, lines)| {
                    let lines = trim_blank_edges(lines);
                    (!lines.is_empty()).then(|| Hunk {
                        anchor,
                        lines: number(lines),
                    })
                })
                .collect(),
        },
    };
    edits.push(FileEdit {
        path: file.path,
        change,
    });
}

/// Empty context lines at a hunk's edges are cosmetic.
fn trim_blank_edges(mut lines: Vec<(DiffOp, String)>) -> Vec<(DiffOp, String)> {
    let blank = |line: &(DiffOp, String)| line.0 == DiffOp::Context && line.1.is_empty();
    while lines.last().is_some_and(blank) {
        lines.pop();
    }
    let leading = lines.iter().take_while(|line| blank(line)).count();
    lines.drain(..leading);
    lines
}

fn number(lines: Vec<(DiffOp, String)>) -> Vec<DiffLine> {
    let (mut old, mut new) = (1_u32, 1_u32);
    lines
        .into_iter()
        .map(|(op, text)| {
            let line = DiffLine {
                op,
                text,
                old: (op != DiffOp::Added).then_some(old),
                new: (op != DiffOp::Removed).then_some(new),
            };
            if op != DiffOp::Added {
                old += 1;
            }
            if op != DiffOp::Removed {
                new += 1;
            }
            line
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
        let edits = parse(patch).expect("a patch");
        assert_eq!(edits.len(), 4);
        assert_eq!(edits[0].path, "src/main.rs");
        let FileChange::Update { hunks, moved_to } = &edits[0].change else {
            panic!("update expected");
        };
        assert_eq!(moved_to, &None);
        assert_eq!(hunks[0].anchor.as_deref(), Some("fn main"));
        let numbers: Vec<_> = hunks[0].lines.iter().map(|l| (l.old, l.new)).collect();
        assert_eq!(
            numbers,
            [(Some(1), Some(1)), (Some(2), None), (None, Some(2))]
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
    fn a_patch_without_envelope_but_with_a_directive_is_read() {
        let edits = parse("*** Add File: a.txt\n+hi").expect("a patch");
        assert_eq!(edits[0].path, "a.txt");
    }

    #[test]
    fn text_that_is_not_a_patch_is_refused() {
        assert!(parse("hello").is_none());
        assert!(parse("*** Begin Patch\n*** Update File: a\n@@\n x").is_none());
        assert!(parse("*** Begin Patch\n*** Add File: a\nno plus\n*** End Patch").is_none());
        assert!(parse("*** Begin Patch\n*** End Patch").is_none());
    }

    #[test]
    fn blank_context_at_the_edges_is_dropped() {
        let edits = parse("*** Begin Patch\n*** Update File: a\n@@\n\n x\n+y\n\n*** End Patch")
            .expect("a patch");
        let FileChange::Update { hunks, .. } = &edits[0].change else {
            panic!("update expected");
        };
        assert_eq!(hunks[0].lines.len(), 2);
    }
}
