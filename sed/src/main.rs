//! The `aither-sed` executable.

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(may_sed::run(std::env::args_os().skip(1).collect()))
}
