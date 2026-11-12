//! The `apply_patch` executable: reads a compact contextual patch on standard
//! input and applies it to the filesystem relative to the current directory.
//!
//! It is an ordinary sandbox-executed binary — all parsing, matching, and
//! writes happen in this process under the shell's sandbox policy.

use std::io::{self, Read};
use std::process::ExitCode;

/// Printed by `--version`; consumed by the tool-runtime initializer to verify
/// artifact identity, so it stays a bare `apply_patch <semver>` line.
const VERSION: &str = concat!("apply_patch ", env!("CARGO_PKG_VERSION"));

const HELP: &str = include_str!("../templates/help.txt");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => run(),
        [flag] if flag == "-h" || flag == "--help" => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        [flag] if flag == "-V" || flag == "--version" => {
            println!("{VERSION}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!(
                "apply_patch: unexpected arguments (takes no arguments; patch on stdin)\nusage: apply_patch < PATCH  (see --help)"
            );
            ExitCode::from(2)
        }
    }
}

/// Reads the patch from stdin and applies it under the current directory.
fn run() -> ExitCode {
    let mut input = Vec::new();
    let read_result = io::stdin().lock().read_to_end(&mut input);
    if let Err(error) = read_result {
        eprintln!("apply_patch: failed to read standard input: {error}");
        return ExitCode::FAILURE;
    }
    let Ok(patch) = String::from_utf8(input) else {
        eprintln!("apply_patch: patch input is not valid UTF-8");
        return ExitCode::FAILURE;
    };
    let base = match std::env::current_dir() {
        Ok(base) => base,
        Err(error) => {
            eprintln!("apply_patch: failed to resolve current directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    match may_patch::apply_patch_text(&patch, &base) {
        Ok(outcomes) => {
            for outcome in &outcomes {
                println!("{}", outcome.summary());
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("apply_patch: {error}");
            ExitCode::FAILURE
        }
    }
}
