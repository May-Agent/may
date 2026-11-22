//! The `search` executable: line and structural search whose output carries
//! hashline anchors — `[path#TAG]` sections of `N:content` lines an `edit`
//! patch references directly.
//!
//! `search rg` runs ripgrep's engine over an `ignore` walk; `search sg` runs
//! ast-grep's pattern engine. Both are ordinary sandbox processes — no IPC,
//! no daemon.

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

const VERSION: &str = concat!("search ", env!("CARGO_PKG_VERSION"));
const HELP: &str = include_str!("../../templates/search_help.txt");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(sub) = args.first() else {
        eprint!("{HELP}");
        return ExitCode::from(2);
    };
    match sub.as_str() {
        "-h" | "--help" => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        "-V" | "--version" => {
            println!("{VERSION}");
            ExitCode::SUCCESS
        }
        "rg" => rg(&args[1..]),
        "sg" => sg(&args[1..]),
        other => {
            eprintln!("search: unknown subcommand `{other}` — expected `rg` or `sg`\n{HELP}");
            ExitCode::from(2)
        }
    }
}

/// `search rg` argument parsing: flags until the pattern, paths after it.
fn rg(args: &[String]) -> ExitCode {
    let mut opts = may_hashline::RgOptions::default();
    let mut positional: Vec<String> = Vec::new();
    let mut flags = true;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match (flags, arg.as_str()) {
            (true, "--") => flags = false,
            (true, "-h" | "--help") => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            (true, "-F" | "--literal") => opts.literal = true,
            (true, "-i" | "--ignore-case") => opts.ignore_case = true,
            (true, "-S" | "--smart-case") => opts.smart_case = true,
            (true, "--hidden") => opts.hidden = true,
            (true, "-l" | "--files-with-matches") => opts.files_with_matches = true,
            (true, "-C" | "--context") => match take_value(args, &mut i, "--context") {
                Ok(value) => opts.context = value,
                Err(exit) => return exit,
            },
            (true, "-g" | "--glob") => match take_string(args, &mut i, "--glob") {
                Ok(value) => opts.globs.push(value),
                Err(exit) => return exit,
            },
            (true, "-m" | "--max-count" | "--limit") => match take_value(args, &mut i, "--limit") {
                Ok(value) => opts.limit = value,
                Err(exit) => return exit,
            },
            (true, _) if arg.starts_with("--context=") => {
                match parse_usize(&arg["--context=".len()..], "--context") {
                    Ok(value) => opts.context = value,
                    Err(exit) => return exit,
                }
            }
            (true, _) if arg.starts_with("--glob=") => {
                opts.globs.push(arg["--glob=".len()..].to_string());
            }
            (true, _) if arg.starts_with("--limit=") => {
                match parse_usize(&arg["--limit=".len()..], "--limit") {
                    Ok(value) => opts.limit = value,
                    Err(exit) => return exit,
                }
            }
            (true, _) if arg.starts_with('-') && arg.len() > 1 => {
                eprintln!("search rg: unknown option `{arg}` (see `search rg --help`)");
                return ExitCode::from(2);
            }
            _ => positional.push(arg.clone()),
        }
        i += 1;
    }
    let Some(pattern) = positional.first() else {
        eprintln!("search rg: missing pattern\nusage: search rg [options] <pattern> [path…]");
        return ExitCode::from(2);
    };
    let paths: Vec<PathBuf> = positional[1..].iter().map(PathBuf::from).collect();
    match may_hashline::run_rg(pattern, &paths, &opts) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("search rg: {error}");
            ExitCode::FAILURE
        }
    }
}

/// `search sg` argument parsing: `--lang` required, pattern positional.
fn sg(args: &[String]) -> ExitCode {
    let mut opts = may_hashline::SgOptions::default();
    let mut positional: Vec<String> = Vec::new();
    let mut flags = true;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match (flags, arg.as_str()) {
            (true, "--") => flags = false,
            (true, "-h" | "--help") => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            (true, "-l" | "--lang") => match take_string(args, &mut i, "--lang") {
                Ok(value) => opts.lang = value,
                Err(exit) => return exit,
            },
            (true, "-C" | "--context") => match take_value(args, &mut i, "--context") {
                Ok(value) => opts.context = value,
                Err(exit) => return exit,
            },
            (true, "-g" | "--glob") => match take_string(args, &mut i, "--glob") {
                Ok(value) => opts.globs.push(value),
                Err(exit) => return exit,
            },
            (true, "--hidden") => opts.hidden = true,
            (true, "-m" | "--max-count" | "--limit") => match take_value(args, &mut i, "--limit") {
                Ok(value) => opts.limit = value,
                Err(exit) => return exit,
            },
            (true, _) if arg.starts_with("--lang=") => {
                opts.lang = arg["--lang=".len()..].to_string();
            }
            (true, _) if arg.starts_with("--context=") => {
                match parse_usize(&arg["--context=".len()..], "--context") {
                    Ok(value) => opts.context = value,
                    Err(exit) => return exit,
                }
            }
            (true, _) if arg.starts_with("--glob=") => {
                opts.globs.push(arg["--glob=".len()..].to_string());
            }
            (true, _) if arg.starts_with("--limit=") => {
                match parse_usize(&arg["--limit=".len()..], "--limit") {
                    Ok(value) => opts.limit = value,
                    Err(exit) => return exit,
                }
            }
            (true, _) if arg.starts_with('-') && arg.len() > 1 => {
                eprintln!("search sg: unknown option `{arg}` (see `search sg --help`)");
                return ExitCode::from(2);
            }
            _ => positional.push(arg.clone()),
        }
        i += 1;
    }
    if opts.lang.is_empty() {
        eprintln!(
            "search sg: missing --lang\nusage: search sg <pattern> --lang <lang> [options] [path…]"
        );
        return ExitCode::from(2);
    }
    let Some(pattern) = positional.first() else {
        eprintln!(
            "search sg: missing pattern\nusage: search sg <pattern> --lang <lang> [options] [path…]"
        );
        return ExitCode::from(2);
    };
    // A pattern piped on stdin stands in for the positional.
    let pattern = if *pattern == "-" {
        let mut buf = String::new();
        if std::io::stdin().lock().read_to_string(&mut buf).is_err() {
            eprintln!("search sg: failed to read the pattern from stdin");
            return ExitCode::FAILURE;
        }
        buf
    } else {
        pattern.clone()
    };
    let paths: Vec<PathBuf> = positional[1..].iter().map(PathBuf::from).collect();
    match may_hashline::run_sg(&pattern, &paths, &opts) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("search sg: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Takes the value of a separated option and parses it as a number.
fn take_value(args: &[String], i: &mut usize, name: &str) -> Result<usize, ExitCode> {
    let value = take_string(args, i, name)?;
    parse_usize(&value, name)
}

/// Takes the value of a separated option.
fn take_string(args: &[String], i: &mut usize, name: &str) -> Result<String, ExitCode> {
    *i += 1;
    args.get(*i).cloned().ok_or_else(|| {
        eprintln!("search: {name} needs a value");
        ExitCode::from(2)
    })
}

/// Parses a `--context`/`--limit` value.
fn parse_usize(raw: &str, name: &str) -> Result<usize, ExitCode> {
    raw.parse::<usize>().map_err(|_| {
        eprintln!("search: {name} expects a number, got `{raw}`");
        ExitCode::from(2)
    })
}
