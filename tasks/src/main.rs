//! `tasks`: edits the session's `tasks.md`, which `MAY_TASKS` names.
//!
//! The container image builds this binary; `may` runs the same code when it
//! is invoked under the name `tasks` ([`may_tasks::run`]).

fn main() -> std::process::ExitCode {
    may_tasks::run(std::env::args().skip(1))
}
