//! The error every hashline operation reports through.

use std::path::PathBuf;

/// The one error type the parser, applier, and searchers report through.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A patch line could not be parsed.
    #[error("line {line}: {detail}")]
    Parse {
        /// The 1-indexed line of patch input that failed.
        line: usize,
        /// What is wrong with it.
        detail: String,
    },
    /// A patch named a file outside the base directory or unreadable.
    #[error("{path}: {detail}")]
    File {
        /// The path the section named.
        path: PathBuf,
        /// What went wrong with it.
        detail: String,
    },
    /// One or more line anchors in a section did not match the live file.
    #[error("{path}: hash mismatch (no files were modified):\n{details}\n{context}")]
    HashMismatch {
        /// The path the section named.
        path: PathBuf,
        /// Per-anchor mismatch descriptions and any moved-line hints.
        details: String,
        /// Live `[path]` section with `N#HH|content` context around mismatches
        /// and hinted lines.
        context: String,
    },
    /// An op addressed lines the file does not have, or ranges overlapped.
    #[error("{path}: {detail}")]
    Anchoring {
        /// The path the section named.
        path: PathBuf,
        /// What the op got wrong.
        detail: String,
    },
    /// A `N#HH*` op's language could not resolve a block.
    #[error("{path}: line {line} does not begin a block{hint}")]
    NoBlock {
        /// The path the section named.
        path: PathBuf,
        /// The anchored line.
        line: usize,
        /// A hint such as " for this file type" when known.
        hint: &'static str,
    },
    /// A register was pasted before a `YANK` or `CUT` filled it.
    #[error(
        "{path}: register `{name}` is empty — a `YANK {name} …` or `CUT … @{name}` must precede the paste that uses it"
    )]
    EmptyRegister {
        /// The path the section named.
        path: PathBuf,
        /// The register name.
        name: String,
    },
    /// A filesystem operation failed.
    #[error("{path}: {source}")]
    Io {
        /// The path touched.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A search pattern or option could not be built.
    #[error("{0}")]
    Usage(String),
}
