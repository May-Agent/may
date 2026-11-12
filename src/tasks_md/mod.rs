//! `tasks.md` in the harness.
//!
//! The parser and the `tasks` command's editing come from `may-tasks`; the
//! schema that reads the file for aither's working-document mechanism is here
//! because it needs aither's document types.

pub use may_tasks::*;

pub mod schema;
