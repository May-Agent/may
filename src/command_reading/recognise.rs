//! Maps a command name and its words to what it does.
//!
//! Every recogniser is strict: a flag it does not know, a word that is not
//! literal where it matters, or a shape it was not written for returns
//! `None`, and the command stays unknown.

use std::path::Path;

use super::shell::Arg;
use super::{BuildTool, Intent, ListTool, ReadRange, SearchTool, patch};

pub(super) fn recognise(
    name: &str,
    args: &[Arg],
    stdin: Option<&str>,
    dir: Option<&Path>,
) -> Option<Vec<Intent>> {
    if name == "apply_patch" {
        return match (args, stdin) {
            ([], Some(text)) => Some(vec![Intent::Edit {
                files: patch::parse(text, dir)?,
            }]),
            _ => None,
        };
    }
    if stdin.is_some() {
        return None;
    }
    match name {
        "cat" => cat(args),
        "head" => head_or_tail(args, true),
        "tail" => head_or_tail(args, false),
        "sed" => sed(args),
        "nl" => reads(&plain_paths(args)?, ReadRange::Whole),
        "rg" => search(args, SearchTool::Rg),
        "grep" => search(args, SearchTool::Grep),
        "fd" => search(args, SearchTool::Fd),
        "find" => find(args),
        "ls" => list(args, ListTool::Ls),
        "tree" => list(args, ListTool::Tree),
        "git" => git(args),
        "cargo" => build(args, BuildTool::Cargo),
        "bun" => build(args, BuildTool::Bun),
        "npm" => build(args, BuildTool::Npm),
        "uv" => build(args, BuildTool::Uv),
        "make" => build(args, BuildTool::Make),
        _ => None,
    }
}

fn reads(paths: &[String], range: ReadRange) -> Option<Vec<Intent>> {
    (!paths.is_empty()).then(|| {
        paths
            .iter()
            .map(|path| Intent::Read {
                path: path.clone(),
                range,
            })
            .collect()
    })
}

/// Literal words, none of them a flag.
fn plain_paths(args: &[Arg]) -> Option<Vec<String>> {
    args.iter()
        .map(|arg| {
            arg.literal()
                .filter(|text| !text.starts_with('-') && !text.is_empty())
                .map(str::to_owned)
        })
        .collect()
}

fn cat(args: &[Arg]) -> Option<Vec<Intent>> {
    let rest = match args.split_first() {
        Some((first, rest)) if first.literal() == Some("-n") => rest,
        _ => args,
    };
    reads(&plain_paths(rest)?, ReadRange::Whole)
}

fn count(text: &str) -> Option<u64> {
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

fn head_or_tail(args: &[Arg], head: bool) -> Option<Vec<Intent>> {
    let mut lines = 10;
    let mut paths = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let text = arg.literal()?;
        if text == "-n" {
            lines = count(iter.next()?.literal()?)?;
        } else if let Some(value) = text.strip_prefix("--lines=") {
            lines = count(value)?;
        } else if let Some(value) = text.strip_prefix("-n") {
            lines = count(value)?;
        } else if let Some(value) = text.strip_prefix('-').filter(|v| !v.is_empty()) {
            lines = count(value)?;
        } else {
            paths.push(text.to_owned());
        }
    }
    let range = if head {
        ReadRange::Head { lines }
    } else {
        ReadRange::Tail { lines }
    };
    reads(&paths, range)
}

/// `sed -n 'A,Bp' file...` and `sed -n 'Ap' file...`.
fn sed(args: &[Arg]) -> Option<Vec<Intent>> {
    let [flag, script, files @ ..] = args else {
        return None;
    };
    if flag.literal()? != "-n" {
        return None;
    }
    let body = script.literal()?.strip_suffix('p')?;
    let (start, end) = if let Some((start, end)) = body.split_once(',') {
        (count(start)?, count(end)?)
    } else {
        let line = count(body)?;
        (line, line)
    };
    if start == 0 || end < start {
        return None;
    }
    reads(&plain_paths(files)?, ReadRange::Lines { start, end })
}

/// The flags one program accepts.
struct Flags {
    short: &'static str,
    short_value: &'static str,
    long: &'static [&'static str],
    long_value: &'static [&'static str],
    /// The short flag whose value is the pattern.
    short_pattern: Option<char>,
    /// The long flag whose value is the pattern.
    long_pattern: Option<&'static str>,
}

struct Walk<'a> {
    positionals: Vec<&'a Arg>,
    flag_pattern: Option<String>,
    /// Whether the long flag `--files` was given.
    files_only: bool,
}

fn walk<'a>(args: &'a [Arg], flags: &Flags) -> Option<Walk<'a>> {
    let mut out = Walk {
        positionals: Vec::new(),
        flag_pattern: None,
        files_only: false,
    };
    let mut iter = args.iter();
    let mut rest_positional = false;
    while let Some(arg) = iter.next() {
        let Some(text) = arg.literal() else {
            out.positionals.push(arg);
            continue;
        };
        if rest_positional || text == "-" || !text.starts_with('-') {
            out.positionals.push(arg);
        } else if text == "--" {
            rest_positional = true;
        } else if let Some(long) = text.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (long, None),
            };
            if flags.long_pattern == Some(name) {
                let value = match inline {
                    Some(value) => value,
                    None => iter.next()?.literal()?.to_owned(),
                };
                if out.flag_pattern.replace(value).is_some() {
                    return None;
                }
            } else if flags.long_value.contains(&name) {
                if inline.is_none() {
                    iter.next()?.literal()?;
                }
            } else if flags.long.contains(&name) && inline.is_none() {
                out.files_only |= name == "files";
            } else {
                return None;
            }
        } else {
            let cluster = &text[1..];
            for (offset, flag) in cluster.char_indices() {
                if flags.short_pattern == Some(flag) || flags.short_value.contains(flag) {
                    let tail = &cluster[offset + flag.len_utf8()..];
                    let value = if tail.is_empty() {
                        iter.next()?.literal()?.to_owned()
                    } else {
                        tail.to_owned()
                    };
                    if flags.short_pattern == Some(flag)
                        && out.flag_pattern.replace(value).is_some()
                    {
                        return None;
                    }
                    break;
                } else if !flags.short.contains(flag) {
                    return None;
                }
            }
        }
    }
    Some(out)
}

fn literals(args: &[&Arg]) -> Option<Vec<String>> {
    args.iter()
        .map(|arg| arg.literal().map(str::to_owned))
        .collect()
}

fn search(args: &[Arg], tool: SearchTool) -> Option<Vec<Intent>> {
    let flags = match tool {
        SearchTool::Rg => Flags {
            short: "nilwFSsvcHuNaoU",
            short_value: "gtTABCmj",
            long: &[
                "hidden",
                "no-heading",
                "files",
                "ignore-case",
                "smart-case",
                "line-number",
                "files-with-matches",
                "fixed-strings",
                "word-regexp",
                "no-ignore",
                "count",
                "invert-match",
                "follow",
                "with-filename",
                "no-filename",
            ],
            long_value: &[
                "glob",
                "type",
                "type-not",
                "after-context",
                "before-context",
                "context",
                "max-count",
                "threads",
                "max-depth",
            ],
            short_pattern: Some('e'),
            long_pattern: Some("regexp"),
        },
        SearchTool::Grep => Flags {
            short: "rRnilwFEvcHhsoIaP",
            short_value: "ABCm",
            long: &[
                "recursive",
                "line-number",
                "ignore-case",
                "files-with-matches",
                "word-regexp",
                "fixed-strings",
                "extended-regexp",
                "invert-match",
                "count",
                "with-filename",
                "no-filename",
                "no-messages",
            ],
            long_value: &["include", "exclude", "exclude-dir", "max-count"],
            short_pattern: Some('e'),
            long_pattern: Some("regexp"),
        },
        SearchTool::Fd => Flags {
            short: "HIiasLu",
            short_value: "tefdE",
            long: &[
                "hidden",
                "no-ignore",
                "ignore-case",
                "case-sensitive",
                "follow",
            ],
            long_value: &["type", "extension", "exclude", "max-depth"],
            short_pattern: None,
            long_pattern: None,
        },
        SearchTool::Find => return None,
    };
    let walked = walk(args, &flags)?;
    let mut positionals = walked.positionals.as_slice();
    let pattern = if walked.files_only {
        None
    } else if let Some(pattern) = walked.flag_pattern {
        Some(pattern)
    } else if let Some((first, rest)) = positionals.split_first() {
        positionals = rest;
        Some(first.literal()?.to_owned())
    } else if tool == SearchTool::Fd {
        None
    } else {
        // `grep` and `rg` without a pattern read standard input.
        return None;
    };
    Some(vec![Intent::Search {
        tool,
        pattern,
        paths: literals(positionals)?,
    }])
}

/// `find PATH... [-name P] [-type T] [-maxdepth N]`; every other predicate,
/// `-exec` and `-delete` above all, leaves the command unknown.
fn find(args: &[Arg]) -> Option<Vec<Intent>> {
    let split = args
        .iter()
        .position(|arg| {
            arg.literal()
                .is_none_or(|text| text.starts_with(['-', '(', '!']))
        })
        .unwrap_or(args.len());
    let (paths, predicates) = args.split_at(split);
    let paths = plain_paths(paths)?;
    let mut pattern = None;
    let mut iter = predicates.iter();
    while let Some(predicate) = iter.next() {
        let value = iter.next()?.literal()?;
        match predicate.literal()? {
            "-name" | "-iname" | "-path" | "-ipath" => {
                if pattern.replace(value.to_owned()).is_some() {
                    return None;
                }
            }
            "-type" => {
                if !matches!(value, "f" | "d" | "l") {
                    return None;
                }
            }
            "-maxdepth" | "-mindepth" => {
                count(value)?;
            }
            _ => return None,
        }
    }
    Some(vec![Intent::Search {
        tool: SearchTool::Find,
        pattern,
        paths,
    }])
}

fn list(args: &[Arg], tool: ListTool) -> Option<Vec<Intent>> {
    let flags = match tool {
        ListTool::Ls => Flags {
            short: "alhRFtrSdA1iLpkcs",
            short_value: "",
            long: &[],
            long_value: &[],
            short_pattern: None,
            long_pattern: None,
        },
        ListTool::Tree => Flags {
            short: "aCdfFhi",
            short_value: "LI",
            long: &[],
            long_value: &[],
            short_pattern: None,
            long_pattern: None,
        },
    };
    let walked = walk(args, &flags)?;
    Some(vec![Intent::List {
        tool,
        paths: literals(&walked.positionals)?,
    }])
}

fn git(args: &[Arg]) -> Option<Vec<Intent>> {
    let mut rest = args;
    while let [flag, directory, tail @ ..] = rest {
        if flag.literal() != Some("-C") {
            break;
        }
        directory.literal()?;
        rest = tail;
    }
    let subcommand = match rest.first() {
        None => None,
        Some(first) => Some(
            first
                .literal()
                .filter(|text| !text.starts_with('-'))?
                .to_owned(),
        ),
    };
    Some(vec![Intent::Git { subcommand }])
}

fn build(args: &[Arg], tool: BuildTool) -> Option<Vec<Intent>> {
    if tool == BuildTool::Make {
        let target = match args {
            [] => None,
            [only] => Some(only.literal().filter(|t| !t.starts_with('-'))?.to_owned()),
            _ => return None,
        };
        return Some(vec![Intent::Build {
            tool,
            subcommand: None,
            target,
        }]);
    }
    let rest = match args.split_first() {
        Some((toolchain, rest))
            if tool == BuildTool::Cargo
                && toolchain.literal().is_some_and(|t| t.starts_with('+')) =>
        {
            rest
        }
        _ => args,
    };
    let subcommand = match rest.first() {
        None => None,
        Some(first) => Some(first.literal()?.to_owned()),
    }
    .filter(|text| !text.starts_with('-'));
    let target = match (tool, subcommand.as_deref(), rest.get(1)) {
        (BuildTool::Npm | BuildTool::Bun, Some("run"), Some(script)) => script
            .literal()
            .filter(|text| !text.starts_with('-'))
            .map(str::to_owned),
        _ => None,
    };
    Some(vec![Intent::Build {
        tool,
        subcommand,
        target,
    }])
}
