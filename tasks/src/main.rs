//! `tasks`: edits the session's `tasks.md`, which `MAY_TASKS` names.
//!
//! An ordinary program on the sandbox `PATH`, not an IPC command: it shares
//! the harness's `tasks.md` parser through the `may-tasks` library and edits the file
//! directly.

use std::process::ExitCode;

use may_tasks::edit;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = std::env::var_os(may_tasks::ENV).map_or(Err(edit::Error::Unset), |path| {
        edit::run(path.as_ref(), &args)
    });
    match result {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprint!("{error}");
            ExitCode::FAILURE
        }
    }
}
