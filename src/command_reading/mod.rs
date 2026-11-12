//! How May reads a terminal call before it runs.
//!
//! A script is parsed with `brush-parser` into a [`CommandReading`]: one
//! [`Step`] per command, each either an [`Intent`] May recognises or an
//! explicit [`Intent::Unknown`] carrying the command's source text. A
//! command is recognised only from literal words; a variable, a command
//! substitution or a glob where a path is expected makes the command
//! unknown, never a guessed path. Clients, the approval prompt and the
//! audit log render the reading instead of decoding shell themselves.

mod card;
mod describe;
mod patch;
mod recognise;
mod shell;

use std::path::Path;

use serde::{Deserialize, Serialize};

pub use card::{AnswerCard, AskedCard, Card, RowChange, RowChangeKind, RowView, SubagentState};
pub use patch::{DiffLine, DiffOp, FileChange, FileEdit, Hunk};

/// A terminal call's script, read into the commands it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandReading {
    /// The script's commands in order.
    pub steps: Vec<Step>,
}

/// One command of a script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    /// How the command follows the one before it.
    pub link: Link,
    /// What the command does.
    pub intent: Intent,
    /// The command in plain words; the source text for an unknown one.
    pub line: String,
}

/// How a command follows the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum Link {
    /// The first command.
    Start,
    /// `|`: it reads the previous command's output.
    Pipe,
    /// `&&`: it runs when the previous command succeeded.
    And,
    /// `||`: it runs when the previous command failed.
    Or,
    /// `;` or a newline: it runs after the previous command.
    Then,
}

/// What one command does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Intent {
    /// `apply_patch`: the files it changes, from the patch text.
    Edit {
        /// The files the patch adds, deletes, moves and updates.
        files: Vec<FileEdit>,
    },
    /// `cat`, `head`, `tail`, `sed -n`, `nl`.
    Read {
        /// The file read.
        path: String,
        /// Which part of it.
        range: ReadRange,
    },
    /// `rg`, `grep`, `fd`, `find`.
    Search {
        /// The program.
        tool: SearchTool,
        /// The pattern or name searched for; absent when listing files.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
        /// Where it searches; empty means the current directory.
        paths: Vec<String>,
    },
    /// `ls`, `tree`.
    List {
        /// The program.
        tool: ListTool,
        /// What it lists; empty means the current directory.
        paths: Vec<String>,
    },
    /// `git`.
    Git {
        /// The subcommand, when it is the first word after `git`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subcommand: Option<String>,
    },
    /// `cargo`, `bun`, `npm`, `uv`, `make`.
    Build {
        /// The program.
        tool: BuildTool,
        /// The subcommand, when it is the first word after the program.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subcommand: Option<String>,
        /// The script or target a `run` or `make` names.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    /// `tasks`: an edit of the session's `tasks.md` the command's arguments
    /// name; a listing is not one.
    Tasks {
        /// The command's arguments.
        args: Vec<String>,
    },
    /// Anything May does not understand.
    Unknown {
        /// The command as written.
        source: String,
    },
}

/// The part of a file a read covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ReadRange {
    /// The whole file.
    Whole,
    /// The first `lines` lines.
    Head {
        /// How many.
        lines: u64,
    },
    /// The last `lines` lines.
    Tail {
        /// How many.
        lines: u64,
    },
    /// Lines `start` to `end`, both inclusive.
    Lines {
        /// First line.
        start: u64,
        /// Last line.
        end: u64,
    },
}

/// A program that searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum SearchTool {
    /// `rg`
    Rg,
    /// `grep`
    Grep,
    /// `fd`
    Fd,
    /// `find`
    Find,
}

/// A program that lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum ListTool {
    /// `ls`
    Ls,
    /// `tree`
    Tree,
}

/// A build or test program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum BuildTool {
    /// `cargo`
    Cargo,
    /// `bun`
    Bun,
    /// `npm`
    Npm,
    /// `uv`
    Uv,
    /// `make`
    Make,
}

impl CommandReading {
    /// Reads `script`. A script that does not parse is one unknown step.
    #[must_use]
    pub fn of(script: &str) -> Self {
        shell::read(script, None)
    }

    /// Reads `script` as it runs in `dir`: the files an `apply_patch` updates
    /// are read from there to give each hunk its absolute line numbers.
    /// Reads files, so it runs off the event loop.
    #[must_use]
    pub fn of_in(script: &str, dir: &Path) -> Self {
        shell::read(script, Some(dir))
    }

    /// Whether a step edits a file in place, so reading the script in the
    /// directory it runs in adds line numbers.
    #[must_use]
    pub fn updates_files(&self) -> bool {
        self.steps.iter().any(|step| {
            matches!(&step.intent, Intent::Edit { files }
                if files.iter().any(|file| matches!(&file.change, FileChange::Update { hunks, .. } if !hunks.is_empty())))
        })
    }

    /// The `tasks.md` edits the script makes, in order.
    pub fn tasks_edits(&self) -> impl Iterator<Item = crate::tasks_md::edit::Op> + '_ {
        self.steps.iter().filter_map(|step| match &step.intent {
            Intent::Tasks { args } => crate::tasks_md::edit::parse(args).ok(),
            _ => None,
        })
    }

    /// Whether any step is something May recognised; a reading without one
    /// adds nothing over the raw script.
    #[must_use]
    pub fn recognises_any(&self) -> bool {
        self.steps
            .iter()
            .any(|step| !matches!(step.intent, Intent::Unknown { .. }))
    }
}

impl Step {
    pub(crate) fn new(link: Link, intent: Intent) -> Self {
        let words = describe::describe(&intent);
        Self {
            link,
            intent,
            line: words,
        }
    }
}

#[cfg(test)]
mod tests;
