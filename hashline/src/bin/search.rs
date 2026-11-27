#![allow(clippy::disallowed_methods)]
//! The `search` executable: line search (`rg`), structural search (`sg`) and
//! read-only line slicing (`sed`, `cat`, `head`, `tail`, `nl`) whose output
//! carries per-line hashline anchors — `[path]` sections of `N#HH|content`
//! lines an `edit` patch references directly.
//!
//! All subcommands run inside the calling process under the shell's sandbox
//! policy — no IPC, no daemon, no session state file.

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use may_hashline::{
    CaseMode, HeadMode, ReadMode, ReadOptions, RgOptions, RgOutputMode, SedOptions, SgOptions,
    SortOrder, TailMode,
};

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
        "sed" => sed(&args[1..]),
        "cat" => cat(&args[1..]),
        "head" => head(&args[1..]),
        "tail" => tail(&args[1..]),
        "nl" => nl(&args[1..]),
        other => {
            eprintln!(
                "search: unknown subcommand `{other}` — expected `rg`, `sg`, `sed`, `cat`, `head`, `tail`, or `nl`\n{HELP}"
            );
            ExitCode::from(2)
        }
    }
}

/// `search rg` argument parsing matching ripgrep's option grammar (including
/// bundled short flags like `-ni`, `-nC2`, `-A5`, `-uu` and `-e`/`-f` patterns).
fn rg(args: &[String]) -> ExitCode {
    let mut opts = RgOptions::default();
    let mut patterns: Vec<String> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut unrestricted: u8 = 0;
    let mut flags = true;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        if !flags || !arg.starts_with('-') || arg == "-" {
            positional.push(arg.clone());
            i += 1;
            continue;
        }
        if arg == "--" {
            flags = false;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match parse_rg_long(
                long,
                args,
                &mut i,
                &mut opts,
                &mut patterns,
                &mut unrestricted,
            ) {
                Ok(LongOutcome::Continue) => {}
                Ok(LongOutcome::Exit(code)) => return code,
                Err(code) => return code,
            }
            i += 1;
            continue;
        }
        // Short option or bundle (e.g. `-n`, `-ni`, `-nC2`, `-A10`, `-g*.py`).
        match parse_rg_short(
            &arg[1..],
            args,
            &mut i,
            &mut opts,
            &mut patterns,
            &mut unrestricted,
        ) {
            Ok(LongOutcome::Continue) => {}
            Ok(LongOutcome::Exit(code)) => return code,
            Err(code) => return code,
        }
        i += 1;
    }

    if unrestricted >= 1 {
        opts.no_ignore = true;
    }
    if unrestricted >= 2 {
        opts.hidden = true;
    }
    if unrestricted >= 3 {
        opts.binary = true;
    }

    let paths: Vec<PathBuf> = if opts.mode == RgOutputMode::Files || !patterns.is_empty() {
        positional.into_iter().map(PathBuf::from).collect()
    } else {
        let Some(first) = positional.first() else {
            eprintln!("search rg: missing pattern\nusage: search rg [options] <pattern> [path…]");
            return ExitCode::from(2);
        };
        patterns.push(first.clone());
        positional[1..].iter().map(PathBuf::from).collect()
    };

    match may_hashline::run_rg_many(&patterns, &paths, &opts) {
        Ok(res) => {
            print!("{}", res.output);
            if opts.mode == RgOutputMode::Quiet && !res.matched {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("search rg: {error}");
            ExitCode::from(2)
        }
    }
}

enum LongOutcome {
    Continue,
    Exit(ExitCode),
}

fn parse_rg_long(
    raw: &str,
    args: &[String],
    i: &mut usize,
    opts: &mut RgOptions,
    patterns: &mut Vec<String>,
    unrestricted: &mut u8,
) -> Result<LongOutcome, ExitCode> {
    let (name, attached) = match raw.split_once('=') {
        Some((k, v)) => (k, Some(v)),
        None => (raw, None),
    };
    let take_str = |args: &[String], i: &mut usize| -> Result<String, ExitCode> {
        if let Some(val) = attached {
            Ok(val.to_string())
        } else {
            take_string(args, i, &format!("--{name}"))
        }
    };
    let take_num = |args: &[String], i: &mut usize| -> Result<usize, ExitCode> {
        let s = take_str(args, i)?;
        parse_usize(&s, &format!("--{name}"))
    };

    match name {
        "help" => {
            print!("{HELP}");
            return Ok(LongOutcome::Exit(ExitCode::SUCCESS));
        }
        "version" => {
            println!("{VERSION}");
            return Ok(LongOutcome::Exit(ExitCode::SUCCESS));
        }
        "type-list" => {
            print!("{}", may_hashline::type_list());
            return Ok(LongOutcome::Exit(ExitCode::SUCCESS));
        }
        "regexp" => patterns.push(take_str(args, i)?),
        "file" => load_pattern_file(&take_str(args, i)?, patterns)?,
        "fixed-strings" | "literal" => opts.literal = true,
        "no-fixed-strings" => opts.literal = false,
        "ignore-case" => opts.case = CaseMode::Ignore,
        "case-sensitive" | "no-ignore-case" => opts.case = CaseMode::Sensitive,
        "smart-case" => opts.case = CaseMode::Smart,
        "no-smart-case" => opts.case = CaseMode::Sensitive,
        "word-regexp" => opts.word = true,
        "no-word-regexp" => opts.word = false,
        "line-regexp" => opts.line_regexp = true,
        "no-line-regexp" => opts.line_regexp = false,
        "invert-match" => opts.invert_match = true,
        "no-invert-match" => opts.invert_match = false,
        "multiline" => opts.multiline = true,
        "no-multiline" => opts.multiline = false,
        "multiline-dotall" => {
            opts.multiline = true;
            opts.multiline_dotall = true;
        }
        "no-multiline-dotall" => opts.multiline_dotall = false,
        "context" => {
            let n = take_num(args, i)?;
            opts.before_context = n;
            opts.after_context = n;
        }
        "before-context" => opts.before_context = take_num(args, i)?,
        "after-context" => opts.after_context = take_num(args, i)?,
        "passthru" => opts.passthru = true,
        "no-passthru" => opts.passthru = false,
        "hidden" => opts.hidden = true,
        "no-hidden" => opts.hidden = false,
        "follow" => opts.follow_links = true,
        "no-follow" => opts.follow_links = false,
        "text" | "binary" => opts.binary = true,
        "no-text" | "no-binary" => opts.binary = false,
        "max-depth" => opts.max_depth = Some(take_num(args, i)?),
        "no-ignore" => opts.no_ignore = true,
        "ignore" => opts.no_ignore = false,
        "no-ignore-vcs" => opts.no_ignore_vcs = true,
        "no-ignore-dot" => opts.no_ignore_dot = true,
        "no-ignore-global" => opts.no_ignore_global = true,
        "no-ignore-parent" => opts.no_ignore_parent = true,
        "no-require-git" => opts.require_git = false,
        "require-git" => opts.require_git = true,
        "one-file-system" => opts.one_file_system = true,
        "unrestricted" => *unrestricted = unrestricted.saturating_add(1),
        "glob" => opts.globs.push(take_str(args, i)?),
        "iglob" => opts.iglobs.push(take_str(args, i)?),
        "glob-case-insensitive" => opts.glob_case_insensitive = true,
        "no-glob-case-insensitive" => opts.glob_case_insensitive = false,
        "type" => opts.types.push(take_str(args, i)?),
        "type-not" => opts.types_not.push(take_str(args, i)?),
        "type-add" => opts.type_adds.push(take_str(args, i)?),
        "max-count" => opts.max_count = Some(take_num(args, i)? as u64),
        "limit" => opts.limit = take_num(args, i)?,
        "files-with-matches" => opts.mode = RgOutputMode::FilesWithMatches,
        "files-without-match" => opts.mode = RgOutputMode::FilesWithoutMatch,
        "count" => opts.mode = RgOutputMode::Count,
        "files" => opts.mode = RgOutputMode::Files,
        "quiet" => opts.mode = RgOutputMode::Quiet,
        "sort" => {
            let _ = take_str(args, i)?;
            opts.sort = Some(SortOrder::Ascending);
        }
        "sortr" => {
            let _ = take_str(args, i)?;
            opts.sort = Some(SortOrder::Descending);
        }
        "sort-files" => opts.sort = Some(SortOrder::Ascending),
        // Accepted display/compatibility flags that `rg` callers pass routinely;
        // hashline output always carries `[path]` and `N#HH|content`.
        "line-number" | "no-line-number" | "with-filename" | "no-filename" | "heading"
        | "no-heading" | "line-buffered" | "block-buffered" | "no-messages"
        | "no-ignore-messages" | "no-config" | "mmap" | "no-mmap" | "crlf" | "no-crlf"
        | "column" | "no-column" | "pretty" | "trim" => {}
        "color" | "colors" | "threads" | "max-columns" | "path-separator" | "context-separator" => {
            let _ = take_str(args, i)?;
        }
        _ => {
            eprintln!("search rg: unknown option `--{name}` (see `search rg --help`)");
            return Err(ExitCode::from(2));
        }
    }
    Ok(LongOutcome::Continue)
}

fn parse_rg_short(
    cluster: &str,
    args: &[String],
    i: &mut usize,
    opts: &mut RgOptions,
    patterns: &mut Vec<String>,
    unrestricted: &mut u8,
) -> Result<LongOutcome, ExitCode> {
    let chars = cluster.char_indices().peekable();
    for (idx, ch) in chars {
        let rest = &cluster[idx + ch.len_utf8()..];
        let take_rest_or_next =
            |args: &[String], i: &mut usize, name: &str| -> Result<String, ExitCode> {
                if !rest.is_empty() {
                    Ok(rest.to_string())
                } else {
                    take_string(args, i, name)
                }
            };
        match ch {
            'h' => {
                print!("{HELP}");
                return Ok(LongOutcome::Exit(ExitCode::SUCCESS));
            }
            'V' => {
                println!("{VERSION}");
                return Ok(LongOutcome::Exit(ExitCode::SUCCESS));
            }
            'e' => {
                patterns.push(take_rest_or_next(args, i, "-e")?);
                break;
            }
            'f' => {
                let path = take_rest_or_next(args, i, "-f")?;
                load_pattern_file(&path, patterns)?;
                break;
            }
            'F' => opts.literal = true,
            'i' => opts.case = CaseMode::Ignore,
            's' => opts.case = CaseMode::Sensitive,
            'S' => opts.case = CaseMode::Smart,
            'w' => opts.word = true,
            'x' => opts.line_regexp = true,
            'v' => opts.invert_match = true,
            'U' => opts.multiline = true,
            'a' => opts.binary = true,
            '.' => opts.hidden = true,
            'L' => opts.follow_links = true,
            'u' => *unrestricted = unrestricted.saturating_add(1),
            'l' => opts.mode = RgOutputMode::FilesWithMatches,
            'c' => opts.mode = RgOutputMode::Count,
            'q' => opts.mode = RgOutputMode::Quiet,
            'C' => {
                let n = parse_usize(&take_rest_or_next(args, i, "-C")?, "-C")?;
                opts.before_context = n;
                opts.after_context = n;
                break;
            }
            'B' => {
                opts.before_context = parse_usize(&take_rest_or_next(args, i, "-B")?, "-B")?;
                break;
            }
            'A' => {
                opts.after_context = parse_usize(&take_rest_or_next(args, i, "-A")?, "-A")?;
                break;
            }
            'g' => {
                opts.globs.push(take_rest_or_next(args, i, "-g")?);
                break;
            }
            't' => {
                opts.types.push(take_rest_or_next(args, i, "-t")?);
                break;
            }
            'T' => {
                opts.types_not.push(take_rest_or_next(args, i, "-T")?);
                break;
            }
            'd' => {
                opts.max_depth = Some(parse_usize(&take_rest_or_next(args, i, "-d")?, "-d")?);
                break;
            }
            'm' => {
                opts.max_count =
                    Some(parse_usize(&take_rest_or_next(args, i, "-m")?, "-m")? as u64);
                break;
            }
            'j' | 'M' => {
                let _ = take_rest_or_next(args, i, &format!("-{ch}"))?;
                break;
            }
            // Compatibility no-ops (`-n` always prints line numbers, `-H`/`-I`/`-N`/`-p`).
            'n' | 'N' | 'H' | 'I' | 'p' => {}
            other => {
                eprintln!("search rg: unknown option `-{other}` (see `search rg --help`)");
                return Err(ExitCode::from(2));
            }
        }
    }
    Ok(LongOutcome::Continue)
}

fn load_pattern_file(path: &str, patterns: &mut Vec<String>) -> Result<(), ExitCode> {
    let text = if path == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .lock()
            .read_to_string(&mut buf)
            .map_err(|e| {
                eprintln!("search rg: failed to read patterns from stdin: {e}");
                ExitCode::FAILURE
            })?;
        buf
    } else {
        std::fs::read_to_string(path).map_err(|e| {
            eprintln!("search rg: {path}: {e}");
            ExitCode::FAILURE
        })?
    };
    for line in text.lines() {
        patterns.push(line.to_string());
    }
    Ok(())
}

/// `search sg` argument parsing: `--lang` required, pattern positional or `-p`/`--pattern`.
fn sg(args: &[String]) -> ExitCode {
    let mut opts = SgOptions::default();
    let mut explicit_pattern: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut flags = true;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        if !flags || !arg.starts_with('-') || arg == "-" {
            positional.push(arg.clone());
            i += 1;
            continue;
        }
        if arg == "--" {
            flags = false;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (long, None),
            };
            let take_str = |args: &[String], i: &mut usize| -> Result<String, ExitCode> {
                if let Some(val) = attached {
                    Ok(val.to_string())
                } else {
                    take_string(args, i, &format!("--{name}"))
                }
            };
            match name {
                "help" => {
                    print!("{HELP}");
                    return ExitCode::SUCCESS;
                }
                "lang" => match take_str(args, &mut i) {
                    Ok(v) => opts.lang = v,
                    Err(c) => return c,
                },
                "pattern" => match take_str(args, &mut i) {
                    Ok(v) => explicit_pattern = Some(v),
                    Err(c) => return c,
                },
                "context" => {
                    match take_str(args, &mut i).and_then(|s| parse_usize(&s, "--context")) {
                        Ok(n) => {
                            opts.before_context = n;
                            opts.after_context = n;
                        }
                        Err(c) => return c,
                    }
                }
                "before-context" => {
                    match take_str(args, &mut i).and_then(|s| parse_usize(&s, "--before-context")) {
                        Ok(n) => opts.before_context = n,
                        Err(c) => return c,
                    }
                }
                "after-context" => {
                    match take_str(args, &mut i).and_then(|s| parse_usize(&s, "--after-context")) {
                        Ok(n) => opts.after_context = n,
                        Err(c) => return c,
                    }
                }
                "glob" => match take_str(args, &mut i) {
                    Ok(v) => opts.globs.push(v),
                    Err(c) => return c,
                },
                "hidden" => opts.hidden = true,
                "max-count" | "limit" => {
                    match take_str(args, &mut i).and_then(|s| parse_usize(&s, "--limit")) {
                        Ok(n) => opts.limit = n,
                        Err(c) => return c,
                    }
                }
                "line-number" => {}
                _ => {
                    eprintln!("search sg: unknown option `--{name}` (see `search sg --help`)");
                    return ExitCode::from(2);
                }
            }
            i += 1;
            continue;
        }
        // Short cluster for `search sg`.
        let cluster = &arg[1..];
        for (idx, ch) in cluster.char_indices() {
            let rest = &cluster[idx + ch.len_utf8()..];
            let take_rest =
                |args: &[String], i: &mut usize, name: &str| -> Result<String, ExitCode> {
                    if !rest.is_empty() {
                        Ok(rest.to_string())
                    } else {
                        take_string(args, i, name)
                    }
                };
            match ch {
                'h' => {
                    print!("{HELP}");
                    return ExitCode::SUCCESS;
                }
                'l' => {
                    match take_rest(args, &mut i, "-l") {
                        Ok(v) => opts.lang = v,
                        Err(c) => return c,
                    }
                    break;
                }
                'p' => {
                    match take_rest(args, &mut i, "-p") {
                        Ok(v) => explicit_pattern = Some(v),
                        Err(c) => return c,
                    }
                    break;
                }
                'C' => {
                    match take_rest(args, &mut i, "-C").and_then(|s| parse_usize(&s, "-C")) {
                        Ok(n) => {
                            opts.before_context = n;
                            opts.after_context = n;
                        }
                        Err(c) => return c,
                    }
                    break;
                }
                'B' => {
                    match take_rest(args, &mut i, "-B").and_then(|s| parse_usize(&s, "-B")) {
                        Ok(n) => opts.before_context = n,
                        Err(c) => return c,
                    }
                    break;
                }
                'A' => {
                    match take_rest(args, &mut i, "-A").and_then(|s| parse_usize(&s, "-A")) {
                        Ok(n) => opts.after_context = n,
                        Err(c) => return c,
                    }
                    break;
                }
                'g' => {
                    match take_rest(args, &mut i, "-g") {
                        Ok(v) => opts.globs.push(v),
                        Err(c) => return c,
                    }
                    break;
                }
                'm' => {
                    match take_rest(args, &mut i, "-m").and_then(|s| parse_usize(&s, "-m")) {
                        Ok(n) => opts.limit = n,
                        Err(c) => return c,
                    }
                    break;
                }
                'n' => {}
                other => {
                    eprintln!("search sg: unknown option `-{other}` (see `search sg --help`)");
                    return ExitCode::from(2);
                }
            }
        }
        i += 1;
    }

    if opts.lang.is_empty() {
        eprintln!(
            "search sg: missing --lang\nusage: search sg <pattern> --lang <lang> [options] [path…]"
        );
        return ExitCode::from(2);
    }
    let (pattern_raw, paths): (String, Vec<PathBuf>) = if let Some(pat) = explicit_pattern {
        (pat, positional.into_iter().map(PathBuf::from).collect())
    } else {
        let Some(first) = positional.first() else {
            eprintln!(
                "search sg: missing pattern\nusage: search sg <pattern> --lang <lang> [options] [path…]"
            );
            return ExitCode::from(2);
        };
        (
            first.clone(),
            positional[1..].iter().map(PathBuf::from).collect(),
        )
    };
    let pattern = if pattern_raw == "-" {
        let mut buf = String::new();
        if std::io::stdin().lock().read_to_string(&mut buf).is_err() {
            eprintln!("search sg: failed to read the pattern from stdin");
            return ExitCode::FAILURE;
        }
        buf
    } else {
        pattern_raw
    };
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

/// `search sed` argument parsing: read-only line-selection slices matching
/// `sed -n '<script>' <file…>` (supports `-n`, `-e`, `-E`/`-r`, `-f`, `--limit`).
fn sed(args: &[String]) -> ExitCode {
    let mut opts = SedOptions::default();
    let mut scripts: Vec<String> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut flags = true;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        if !flags || !arg.starts_with('-') || arg == "-" {
            positional.push(arg.clone());
            i += 1;
            continue;
        }
        if arg == "--" {
            flags = false;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (long, None),
            };
            let take_str = |args: &[String], i: &mut usize| -> Result<String, ExitCode> {
                if let Some(val) = attached {
                    Ok(val.to_string())
                } else {
                    take_string(args, i, &format!("--{name}"))
                }
            };
            match name {
                "help" => {
                    print!("{HELP}");
                    return ExitCode::SUCCESS;
                }
                "quiet" | "silent" => {}
                "regexp-extended" => opts.extended_regex = true,
                "expression" => match take_str(args, &mut i) {
                    Ok(v) => scripts.push(v),
                    Err(c) => return c,
                },
                "file" => {
                    let path = match take_str(args, &mut i) {
                        Ok(v) => v,
                        Err(c) => return c,
                    };
                    match std::fs::read_to_string(&path) {
                        Ok(content) => scripts.push(content),
                        Err(e) => {
                            eprintln!("search sed: {path}: {e}");
                            return ExitCode::FAILURE;
                        }
                    }
                }
                "limit" => match take_str(args, &mut i).and_then(|s| parse_usize(&s, "--limit")) {
                    Ok(n) => opts.limit = Some(n),
                    Err(c) => return c,
                },
                "in-place" => {
                    eprintln!(
                        "search sed: `--in-place` is not supported — `search sed` is read-only; use `edit` to modify existing files"
                    );
                    return ExitCode::from(2);
                }
                _ => {
                    eprintln!("search sed: unknown option `--{name}` (see `search sed --help`)");
                    return ExitCode::from(2);
                }
            }
            i += 1;
            continue;
        }
        // Short cluster (e.g. `-n`, `-nE`, `-ne`, `-e1,50p`).
        let cluster = &arg[1..];
        for (idx, ch) in cluster.char_indices() {
            let rest = &cluster[idx + ch.len_utf8()..];
            let take_rest =
                |args: &[String], i: &mut usize, name: &str| -> Result<String, ExitCode> {
                    if !rest.is_empty() {
                        Ok(rest.to_string())
                    } else {
                        take_string(args, i, name)
                    }
                };
            match ch {
                'h' => {
                    print!("{HELP}");
                    return ExitCode::SUCCESS;
                }
                'n' => {}
                'E' | 'r' => opts.extended_regex = true,
                'e' => {
                    match take_rest(args, &mut i, "-e") {
                        Ok(v) => scripts.push(v),
                        Err(c) => return c,
                    }
                    break;
                }
                'f' => {
                    let path = match take_rest(args, &mut i, "-f") {
                        Ok(v) => v,
                        Err(c) => return c,
                    };
                    match std::fs::read_to_string(&path) {
                        Ok(content) => scripts.push(content),
                        Err(e) => {
                            eprintln!("search sed: {path}: {e}");
                            return ExitCode::FAILURE;
                        }
                    }
                    break;
                }
                'i' => {
                    eprintln!(
                        "search sed: `-i` is not supported — `search sed` is read-only; use `edit` to modify existing files"
                    );
                    return ExitCode::from(2);
                }
                other => {
                    eprintln!("search sed: unknown option `-{other}` (see `search sed --help`)");
                    return ExitCode::from(2);
                }
            }
        }
        i += 1;
    }

    let paths: Vec<PathBuf> = if !scripts.is_empty() {
        positional.into_iter().map(PathBuf::from).collect()
    } else {
        let Some(first) = positional.first() else {
            eprintln!("search sed: missing script\nusage: search sed -n '<range>p' <file…>");
            return ExitCode::from(2);
        };
        scripts.push(first.clone());
        positional[1..].iter().map(PathBuf::from).collect()
    };

    match may_hashline::run_sed(&scripts, &paths, &opts) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

/// `search cat [-n|-b] [--limit N] <file…>`: prints whole file(s) with hashline anchors.
fn cat(args: &[String]) -> ExitCode {
    let mut opts = ReadOptions::default();
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut flags = true;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        if !flags || !arg.starts_with('-') || arg == "-" {
            paths.push(PathBuf::from(arg));
            i += 1;
            continue;
        }
        if arg == "--" {
            flags = false;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (long, None),
            };
            match name {
                "help" => {
                    print!("{HELP}");
                    return ExitCode::SUCCESS;
                }
                "version" => {
                    println!("{VERSION}");
                    return ExitCode::SUCCESS;
                }
                "number" | "number-nonblank" => {}
                "limit" => {
                    let raw = match attached {
                        Some(v) => v.to_string(),
                        None => match take_string(args, &mut i, "--limit") {
                            Ok(v) => v,
                            Err(c) => return c,
                        },
                    };
                    match parse_usize(&raw, "--limit") {
                        Ok(n) => opts.limit = Some(n),
                        Err(c) => return c,
                    }
                }
                "show-all" | "show-nonprinting" | "show-ends" | "show-tabs" | "squeeze-blank" => {
                    eprintln!(
                        "search cat: `--{name}` is not supported — hashline prints plain verbatim lines"
                    );
                    return ExitCode::from(2);
                }
                _ => {
                    eprintln!("search cat: unknown option `--{name}` (see `search --help`)");
                    return ExitCode::from(2);
                }
            }
            i += 1;
            continue;
        }
        for ch in arg[1..].chars() {
            match ch {
                'h' => {
                    print!("{HELP}");
                    return ExitCode::SUCCESS;
                }
                'V' => {
                    println!("{VERSION}");
                    return ExitCode::SUCCESS;
                }
                'n' | 'b' => {}
                'A' | 'v' | 'e' | 'E' | 't' | 'T' | 's' => {
                    eprintln!(
                        "search cat: `-{ch}` is not supported — hashline prints plain verbatim lines"
                    );
                    return ExitCode::from(2);
                }
                other => {
                    eprintln!("search cat: unknown option `-{other}` (see `search --help`)");
                    return ExitCode::from(2);
                }
            }
        }
        i += 1;
    }

    run_read_cli(ReadMode::Cat, &paths, &opts)
}

/// `search head [-n N | -N] [--limit N] <file…>`: prints leading lines with hashline anchors.
fn head(args: &[String]) -> ExitCode {
    let mut mode = HeadMode::First(10);
    let mut opts = ReadOptions::default();
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut flags = true;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        if !flags || !arg.starts_with('-') || arg == "-" {
            paths.push(PathBuf::from(arg));
            i += 1;
            continue;
        }
        if arg == "--" {
            flags = false;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (long, None),
            };
            match name {
                "help" => {
                    print!("{HELP}");
                    return ExitCode::SUCCESS;
                }
                "version" => {
                    println!("{VERSION}");
                    return ExitCode::SUCCESS;
                }
                "lines" => {
                    let raw = match attached {
                        Some(v) => v.to_string(),
                        None => match take_string(args, &mut i, "--lines") {
                            Ok(v) => v,
                            Err(c) => return c,
                        },
                    };
                    match parse_head_lines(&raw, "--lines") {
                        Ok(m) => mode = m,
                        Err(c) => return c,
                    }
                }
                "limit" => {
                    let raw = match attached {
                        Some(v) => v.to_string(),
                        None => match take_string(args, &mut i, "--limit") {
                            Ok(v) => v,
                            Err(c) => return c,
                        },
                    };
                    match parse_usize(&raw, "--limit") {
                        Ok(n) => opts.limit = Some(n),
                        Err(c) => return c,
                    }
                }
                "bytes" => {
                    eprintln!(
                        "search head: `--bytes` is not supported — hashline operates on lines"
                    );
                    return ExitCode::from(2);
                }
                _ => {
                    eprintln!("search head: unknown option `--{name}` (see `search --help`)");
                    return ExitCode::from(2);
                }
            }
            i += 1;
            continue;
        }
        let cluster = &arg[1..];
        if !cluster.is_empty() && cluster.bytes().all(|b| b.is_ascii_digit()) {
            match parse_usize(cluster, arg) {
                Ok(n) => mode = HeadMode::First(n),
                Err(c) => return c,
            }
            i += 1;
            continue;
        }
        if let Some((idx, ch)) = cluster.char_indices().next() {
            let rest = &cluster[idx + ch.len_utf8()..];
            match ch {
                'h' => {
                    print!("{HELP}");
                    return ExitCode::SUCCESS;
                }
                'V' => {
                    println!("{VERSION}");
                    return ExitCode::SUCCESS;
                }
                'n' => {
                    let raw = if !rest.is_empty() {
                        rest.to_string()
                    } else {
                        match take_string(args, &mut i, "-n") {
                            Ok(v) => v,
                            Err(c) => return c,
                        }
                    };
                    match parse_head_lines(&raw, "-n") {
                        Ok(m) => mode = m,
                        Err(c) => return c,
                    }
                }
                'c' => {
                    eprintln!("search head: `-c` is not supported — hashline operates on lines");
                    return ExitCode::from(2);
                }
                other => {
                    eprintln!("search head: unknown option `-{other}` (see `search --help`)");
                    return ExitCode::from(2);
                }
            }
        }
        i += 1;
    }

    run_read_cli(ReadMode::Head(mode), &paths, &opts)
}

/// `search tail [-n N | -N | -n +N] [--limit N] <file…>`: prints trailing lines
/// or lines from `+N` through EOF with real 1-indexed file line numbers.
fn tail(args: &[String]) -> ExitCode {
    let mut mode = TailMode::Last(10);
    let mut opts = ReadOptions::default();
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut flags = true;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        if !flags || !arg.starts_with('-') || arg == "-" {
            paths.push(PathBuf::from(arg));
            i += 1;
            continue;
        }
        if arg == "--" {
            flags = false;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (long, None),
            };
            match name {
                "help" => {
                    print!("{HELP}");
                    return ExitCode::SUCCESS;
                }
                "version" => {
                    println!("{VERSION}");
                    return ExitCode::SUCCESS;
                }
                "lines" => {
                    let raw = match attached {
                        Some(v) => v.to_string(),
                        None => match take_string(args, &mut i, "--lines") {
                            Ok(v) => v,
                            Err(c) => return c,
                        },
                    };
                    match parse_tail_lines(&raw, "--lines") {
                        Ok(m) => mode = m,
                        Err(c) => return c,
                    }
                }
                "limit" => {
                    let raw = match attached {
                        Some(v) => v.to_string(),
                        None => match take_string(args, &mut i, "--limit") {
                            Ok(v) => v,
                            Err(c) => return c,
                        },
                    };
                    match parse_usize(&raw, "--limit") {
                        Ok(n) => opts.limit = Some(n),
                        Err(c) => return c,
                    }
                }
                "bytes" => {
                    eprintln!(
                        "search tail: `--bytes` is not supported — hashline operates on lines"
                    );
                    return ExitCode::from(2);
                }
                "follow" => {
                    eprintln!(
                        "search tail: `--follow` is not supported — use bare `tail -f` for live streams"
                    );
                    return ExitCode::from(2);
                }
                _ => {
                    eprintln!("search tail: unknown option `--{name}` (see `search --help`)");
                    return ExitCode::from(2);
                }
            }
            i += 1;
            continue;
        }
        let cluster = &arg[1..];
        if !cluster.is_empty() && cluster.bytes().all(|b| b.is_ascii_digit()) {
            match parse_usize(cluster, arg) {
                Ok(n) => mode = TailMode::Last(n),
                Err(c) => return c,
            }
            i += 1;
            continue;
        }
        if let Some((idx, ch)) = cluster.char_indices().next() {
            let rest = &cluster[idx + ch.len_utf8()..];
            match ch {
                'h' => {
                    print!("{HELP}");
                    return ExitCode::SUCCESS;
                }
                'V' => {
                    println!("{VERSION}");
                    return ExitCode::SUCCESS;
                }
                'n' => {
                    let raw = if !rest.is_empty() {
                        rest.to_string()
                    } else {
                        match take_string(args, &mut i, "-n") {
                            Ok(v) => v,
                            Err(c) => return c,
                        }
                    };
                    match parse_tail_lines(&raw, "-n") {
                        Ok(m) => mode = m,
                        Err(c) => return c,
                    }
                }
                'c' => {
                    eprintln!("search tail: `-c` is not supported — hashline operates on lines");
                    return ExitCode::from(2);
                }
                'f' | 'F' => {
                    eprintln!(
                        "search tail: `-{ch}` is not supported — use bare `tail -f` for live streams"
                    );
                    return ExitCode::from(2);
                }
                other => {
                    eprintln!("search tail: unknown option `-{other}` (see `search --help`)");
                    return ExitCode::from(2);
                }
            }
        }
        i += 1;
    }

    run_read_cli(ReadMode::Tail(mode), &paths, &opts)
}

/// `search nl [standard nl flags] [--limit N] <file…>`: prints whole file(s)
/// as `N#HH|content`, accepting standard `nl` formatting flags as no-ops.
fn nl(args: &[String]) -> ExitCode {
    let mut opts = ReadOptions::default();
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut flags = true;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        if !flags || !arg.starts_with('-') || arg == "-" {
            paths.push(PathBuf::from(arg));
            i += 1;
            continue;
        }
        if arg == "--" {
            flags = false;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (long, None),
            };
            match name {
                "help" => {
                    print!("{HELP}");
                    return ExitCode::SUCCESS;
                }
                "version" => {
                    println!("{VERSION}");
                    return ExitCode::SUCCESS;
                }
                "limit" => {
                    let raw = match attached {
                        Some(v) => v.to_string(),
                        None => match take_string(args, &mut i, "--limit") {
                            Ok(v) => v,
                            Err(c) => return c,
                        },
                    };
                    match parse_usize(&raw, "--limit") {
                        Ok(n) => opts.limit = Some(n),
                        Err(c) => return c,
                    }
                }
                "no-renumber" => {}
                "body-numbering"
                | "header-numbering"
                | "footer-numbering"
                | "section-delimiter"
                | "line-increment"
                | "join-blank-lines"
                | "number-format"
                | "number-separator"
                | "starting-line-number"
                | "number-width" => {
                    if attached.is_none()
                        && let Err(c) = take_string(args, &mut i, &format!("--{name}"))
                    {
                        return c;
                    }
                }
                _ => {
                    eprintln!("search nl: unknown option `--{name}` (see `search --help`)");
                    return ExitCode::from(2);
                }
            }
            i += 1;
            continue;
        }
        let cluster = &arg[1..];
        for (idx, ch) in cluster.char_indices() {
            let rest = &cluster[idx + ch.len_utf8()..];
            match ch {
                'h' if rest.is_empty() => {
                    print!("{HELP}");
                    return ExitCode::SUCCESS;
                }
                'h' => {
                    // Attached `-ha` / `-ht` / `-hn` header-numbering style no-op.
                    break;
                }
                'V' => {
                    println!("{VERSION}");
                    return ExitCode::SUCCESS;
                }
                'p' => {}
                'b' | 'f' | 'n' | 'w' | 's' | 'v' | 'i' | 'l' | 'd' => {
                    if rest.is_empty()
                        && let Err(c) = take_string(args, &mut i, &format!("-{ch}"))
                    {
                        return c;
                    }
                    break;
                }
                other => {
                    eprintln!("search nl: unknown option `-{other}` (see `search --help`)");
                    return ExitCode::from(2);
                }
            }
        }
        i += 1;
    }

    run_read_cli(ReadMode::Nl, &paths, &opts)
}

fn run_read_cli(mode: ReadMode, paths: &[PathBuf], opts: &ReadOptions) -> ExitCode {
    match may_hashline::run_read(mode, paths, opts) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

fn parse_head_lines(raw: &str, name: &str) -> Result<HeadMode, ExitCode> {
    if let Some(rest) = raw.strip_prefix('-') {
        return parse_usize(rest, name).map(HeadMode::AllButLast);
    }
    let trimmed = raw.strip_prefix('+').unwrap_or(raw);
    parse_usize(trimmed, name).map(HeadMode::First)
}

fn parse_tail_lines(raw: &str, name: &str) -> Result<TailMode, ExitCode> {
    if let Some(rest) = raw.strip_prefix('+') {
        return parse_usize(rest, name).map(TailMode::FromLine);
    }
    let trimmed = raw.strip_prefix('-').unwrap_or(raw);
    parse_usize(trimmed, name).map(TailMode::Last)
}

/// Takes the value of a separated option.
fn take_string(args: &[String], i: &mut usize, name: &str) -> Result<String, ExitCode> {
    *i += 1;
    args.get(*i).cloned().ok_or_else(|| {
        eprintln!("search: {name} needs a value");
        ExitCode::from(2)
    })
}

/// Parses a numeric option value.
fn parse_usize(raw: &str, name: &str) -> Result<usize, ExitCode> {
    raw.parse::<usize>().map_err(|_| {
        eprintln!("search: {name} expects a number, got `{raw}`");
        ExitCode::from(2)
    })
}
