//! `search sg`: structural search over the ast-grep engine — a `Pattern`
//! matched against each file's syntax tree, walked by `ignore` the way
//! `search rg` is. Matches are emitted as `[path]` sections of the whole
//! matched node, `N#HH|content` lines, ready for an `edit` anchor.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use ast_grep_core::{AstGrep, Pattern};
use ast_grep_language::SupportLang;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

use crate::error::Error;
use crate::{display_path, hashed_line, section_header};

/// Knobs for `search sg`.
#[derive(Debug, Clone)]
pub struct SgOptions {
    /// The language pattern and files are parsed as (`--lang`).
    pub lang: String,
    /// Lines of context before each matched node (`-B` / `-C`).
    pub before_context: usize,
    /// Lines of context after each matched node (`-A` / `-C`).
    pub after_context: usize,
    /// Search hidden files too.
    pub hidden: bool,
    /// `-g` glob filters on the searched paths.
    pub globs: Vec<String>,
    /// Matched nodes to emit in total before stopping.
    pub limit: usize,
}

impl Default for SgOptions {
    fn default() -> Self {
        Self {
            lang: String::new(),
            before_context: 0,
            after_context: 0,
            hidden: false,
            globs: Vec::new(),
            limit: 200,
        }
    }
}

/// Runs `search sg <pattern> --lang <lang> [path…]` and returns the hashline
/// output.
///
/// # Errors
/// Returns [`Error::Usage`] for an unknown language or a pattern that does
/// not parse.
pub fn run_sg(pattern: &str, paths: &[PathBuf], opts: &SgOptions) -> Result<String, Error> {
    let lang = support_lang(&opts.lang)?;
    let pattern = Pattern::new(pattern, lang);
    let mut out = String::new();
    let mut remaining = opts.limit;
    'paths: for root in search_roots(paths) {
        let mut walker = WalkBuilder::new(&root);
        walker.hidden(!opts.hidden);
        if !opts.globs.is_empty() {
            let override_root = if root.is_file() {
                root.parent().unwrap_or_else(|| Path::new("."))
            } else {
                &root
            };
            let mut overrides = OverrideBuilder::new(override_root);
            for glob in &opts.globs {
                overrides
                    .add(glob)
                    .map_err(|error| Error::Usage(format!("search sg: -g {glob}: {error}")))?;
            }
            walker.overrides(
                overrides
                    .build()
                    .map_err(|error| Error::Usage(format!("search sg: -g: {error}")))?,
            );
        }
        for entry in walker.build() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            let path = entry.path();
            if lang.file_types().matched(path, false).is_none() {
                continue;
            }
            let bytes = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            let text = String::from_utf8_lossy(&bytes);
            let grep = AstGrep::str(text.as_ref(), lang);
            let ranges: Vec<(usize, usize)> = grep
                .root()
                .find_all(pattern.clone())
                .map(|node| (node.start_pos().line(), node.end_pos().line()))
                .collect();
            if ranges.is_empty() {
                continue;
            }
            emit_file(&mut out, path, &text, &ranges, opts, &mut remaining);
            if remaining == 0 {
                break 'paths;
            }
        }
    }
    Ok(out)
}

/// Resolves a `--lang` name or alias to its language.
fn support_lang(name: &str) -> Result<SupportLang, Error> {
    SupportLang::from_str(name)
        .ok()
        .ok_or_else(|| Error::Usage(format!("search sg: unknown language `{name}`")))
}

/// The paths a search walks; the current directory when none are named.
fn search_roots(paths: &[PathBuf]) -> Vec<PathBuf> {
    if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths.to_vec()
    }
}

/// Appends one file's `[path]` section of matched node spans to `out`.
fn emit_file(
    out: &mut String,
    path: &Path,
    text: &str,
    ranges: &[(usize, usize)],
    opts: &SgOptions,
    remaining: &mut usize,
) {
    let display = display_path(path);
    let _ = writeln!(out, "{}", section_header(&display));
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    let mut previous = 0usize;
    let mut emitted = false;
    for &(start, end) in ranges {
        if *remaining == 0 {
            break;
        }
        let from = start.saturating_sub(opts.before_context);
        let to = (end + opts.after_context).min(lines.len().saturating_sub(1));
        for number in from..=to {
            let content = lines
                .get(number)
                .copied()
                .unwrap_or_default()
                .strip_suffix('\r')
                .unwrap_or_else(|| lines.get(number).copied().unwrap_or_default());
            if emitted && number > previous + 1 {
                let _ = writeln!(out, "--");
            }
            if number <= previous && emitted {
                continue;
            }
            let _ = writeln!(out, "{}", hashed_line(number + 1, content));
            previous = number;
            emitted = true;
        }
        *remaining -= 1;
    }
}
