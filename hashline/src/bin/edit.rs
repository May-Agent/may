//! The `edit` executable: reads a hashline patch on standard input and
//! applies it to files relative to the current directory.
//!
//! It is an ordinary sandbox-executed binary — parsing, tag verification,
//! block resolution, and writes all happen in this process under the shell's
//! sandbox policy.

use std::io::{self, Read};
use std::process::ExitCode;

use may_hashline::{AstResolver, FileOutcome, apply_patch, report_header};

const VERSION: &str = concat!("edit ", env!("CARGO_PKG_VERSION"));
const HELP: &str = include_str!("../../templates/edit_help.txt");

/// Lines of a changed region echoed back before the rest elides.
const ECHO_HEAD: usize = 40;

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
                "edit: unexpected arguments (takes no arguments; patch on stdin)\nusage: edit < PATCH  (see --help)"
            );
            ExitCode::from(2)
        }
    }
}

/// Reads the patch from stdin and applies it under the current directory.
fn run() -> ExitCode {
    let mut patch = String::new();
    if let Err(error) = io::stdin().lock().read_to_string(&mut patch) {
        eprintln!("edit: failed to read standard input: {error}");
        return ExitCode::FAILURE;
    }
    if patch.trim().is_empty() {
        eprintln!("edit: empty patch (see `edit --help` for the format)");
        return ExitCode::from(2);
    }
    let base = match std::env::current_dir() {
        Ok(base) => base,
        Err(error) => {
            eprintln!("edit: failed to resolve current directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    match apply_patch(&base, &patch, &AstResolver) {
        Ok(report) => {
            for section in &report.sections {
                print!("{}", report_header(section));
                println!();
                match &section.outcome {
                    FileOutcome::Removed => {}
                    FileOutcome::Unchanged => println!("(unchanged)"),
                    FileOutcome::Modified | FileOutcome::Moved { .. } => {
                        if let Some(range) = &section.changed {
                            echo_range(section, range);
                        }
                    }
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("edit: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Prints a changed span of new-numbering lines after a header.
fn echo_range(section: &may_hashline::SectionReport, range: &may_hashline::Range) {
    let path = match &section.outcome {
        FileOutcome::Moved { dest } => dest.clone(),
        _ => section.path.clone(),
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let count = range.end - range.start + 1;
    if count <= ECHO_HEAD {
        for (offset, line) in lines[range.start - 1..range.end].iter().enumerate() {
            println!("{}:{line}", range.start + offset);
        }
    } else {
        for (offset, line) in lines[range.start - 1..range.start - 1 + ECHO_HEAD]
            .iter()
            .enumerate()
        {
            println!("{}:{line}", range.start + offset);
        }
        println!("… {} more changed lines", count - ECHO_HEAD);
    }
}
