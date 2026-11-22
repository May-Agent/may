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
    /// A file's live content no longer matches the tag the patch named.
    #[error(
        "{path}: stale tag {expected} — the file changed since the search or read it came from (live tag {live}); search or read it again and re-anchor the edit"
    )]
    StaleTag {
        /// The path the section named.
        path: PathBuf,
        /// The tag the patch named.
        expected: String,
        /// The tag the live file computes to.
        live: String,
    },
    /// An op addressed lines the file does not have, or ranges overlapped.
    #[error("{path}: {detail}")]
    Anchoring {
        /// The path the section named.
        path: PathBuf,
        /// What the op got wrong.
        detail: String,
    },
    /// A `N*` op's language could not resolve a block.
    #[error("{path}: line {line} does not begin a block{hint}")]
    NoBlock {
        /// The path the section named.
        path: PathBuf,
        /// The anchored line.
        line: usize,
        /// A hint such as " for this file type" when known.
        hint: &'static str,
    },
    /// A register was pasted before a `CUT` filled it.
    #[error(
        "{path}: register @{name} is empty — a `CUT … @{name}` must precede the `PUT … @{name}` that uses it"
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
