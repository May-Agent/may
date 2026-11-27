//! `search rg`: line search over the ripgrep engine — `grep-regex` matches
//! and `grep-searcher` streams lines with context — walked by `ignore` so
//! `.gitignore` behaves the way `rg` does. Matched files are emitted as
//! `[path]` sections of `N#HH|content` lines, the anchors an `edit` call
//! pastes back.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use grep_searcher::{Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use ignore::types::TypesBuilder;

use crate::error::Error;
use crate::{display_path, hashed_line, section_header};

/// Case sensitivity mode for `search rg` (last CLI flag wins, matching `rg`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseMode {
    /// Case-sensitive matching (`-s` / `--case-sensitive`, default).
    #[default]
    Sensitive,
    /// Case-insensitive matching (`-i` / `--ignore-case`).
    Ignore,
    /// Case-insensitive unless a pattern has an uppercase letter (`-S` / `--smart-case`).
    Smart,
}

/// Output mode for `search rg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RgOutputMode {
    /// Emit `[path]` followed by `N#HH|content` matching/context lines.
    #[default]
    Matches,
    /// Emit only `[path]` headers of matching files (`-l` / `--files-with-matches`).
    FilesWithMatches,
    /// Emit only `[path]` headers of non-matching files (`--files-without-match`).
    FilesWithoutMatch,
    /// Emit `[path] (N matches)` per matching file (`-c` / `--count`).
    Count,
    /// Emit `[path]` for every file that would be searched (`--files`).
    Files,
    /// Emit nothing (`-q` / `--quiet`); callers check whether any match occurred.
    Quiet,
}

/// Optional deterministic file ordering (`--sort path` / `--sortr path`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    /// Ascending path order (`--sort path`).
    Ascending,
    /// Descending path order (`--sortr path`).
    Descending,
}

/// Knobs for `search rg`.
#[derive(Debug, Clone)]
pub struct RgOptions {
    /// `true` matches patterns literally (`-F` / `--fixed-strings` / `--literal`).
    pub literal: bool,
    /// Case sensitivity mode (`-s`, `-i`, `-S`).
    pub case: CaseMode,
    /// Match only whole words (`-w` / `--word-regexp`).
    pub word: bool,
    /// Match only whole lines (`-x` / `--line-regexp`).
    pub line_regexp: bool,
    /// Invert matching (`-v` / `--invert-match`).
    pub invert_match: bool,
    /// Allow patterns to match across multiple lines (`-U` / `--multiline`).
    pub multiline: bool,
    /// Make `.` match newlines in multiline mode (`--multiline-dotall`).
    pub multiline_dotall: bool,
    /// Lines of context before each match (`-B` / `--before-context`, `-C`).
    pub before_context: usize,
    /// Lines of context after each match (`-A` / `--after-context`, `-C`).
    pub after_context: usize,
    /// Print both matching and non-matching lines (`--passthru`).
    pub passthru: bool,
    /// Search hidden files too (`-.` / `--hidden`).
    pub hidden: bool,
    /// Follow symbolic links (`-L` / `--follow`).
    pub follow_links: bool,
    /// Search binary files as text (`-a` / `--text` / `--binary`).
    pub binary: bool,
    /// Maximum directory traversal depth (`-d` / `--max-depth`).
    pub max_depth: Option<usize>,
    /// Disable all ignore files (`--no-ignore`).
    pub no_ignore: bool,
    /// Disable VCS ignore files (`--no-ignore-vcs`).
    pub no_ignore_vcs: bool,
    /// Disable `.ignore` / `.rgignore` files (`--no-ignore-dot`).
    pub no_ignore_dot: bool,
    /// Disable global gitignore files (`--no-ignore-global`).
    pub no_ignore_global: bool,
    /// Disable parent directory ignore files (`--no-ignore-parent`).
    pub no_ignore_parent: bool,
    /// Apply `.gitignore` even outside a git repo (`--no-require-git`).
    pub require_git: bool,
    /// Stay on a single filesystem (`--one-file-system`).
    pub one_file_system: bool,
    /// `-g` / `--glob` filters on the searched paths.
    pub globs: Vec<String>,
    /// `--iglob` case-insensitive glob filters.
    pub iglobs: Vec<String>,
    /// Process `-g` globs case-insensitively (`--glob-case-insensitive`).
    pub glob_case_insensitive: bool,
    /// `-t` / `--type` file types to include.
    pub types: Vec<String>,
    /// `-T` / `--type-not` file types to exclude.
    pub types_not: Vec<String>,
    /// `--type-add` custom type definitions.
    pub type_adds: Vec<String>,
    /// Maximum matching lines per file (`-m` / `--max-count`).
    pub max_count: Option<u64>,
    /// Total result lines to emit across all files before stopping (`--limit`).
    pub limit: usize,
    /// Output mode (`Matches`, `-l`, `--files-without-match`, `-c`, `--files`, `-q`).
    pub mode: RgOutputMode,
    /// Optional path sorting (`--sort` / `--sortr`).
    pub sort: Option<SortOrder>,
}

impl Default for RgOptions {
    fn default() -> Self {
        Self {
            literal: false,
            case: CaseMode::Sensitive,
            word: false,
            line_regexp: false,
            invert_match: false,
            multiline: false,
            multiline_dotall: false,
            before_context: 0,
            after_context: 0,
            passthru: false,
            hidden: false,
            follow_links: false,
            binary: false,
            max_depth: None,
            no_ignore: false,
            no_ignore_vcs: false,
            no_ignore_dot: false,
            no_ignore_global: false,
            no_ignore_parent: false,
            require_git: true,
            one_file_system: false,
            globs: Vec::new(),
            iglobs: Vec::new(),
            glob_case_insensitive: false,
            types: Vec::new(),
            types_not: Vec::new(),
            type_adds: Vec::new(),
            max_count: None,
            limit: 200,
            mode: RgOutputMode::Matches,
            sort: None,
        }
    }
}

/// Result of running `search rg`.
#[derive(Debug, Clone)]
pub struct RgRunResult {
    /// Formatted hashline output.
    pub output: String,
    /// Whether at least one file or line matched.
    pub matched: bool,
}

/// Collected lines of one file's search, tagged match-vs-context.
struct Collected {
    /// `(line number, text, is-match)` in file order.
    lines: Vec<(usize, String, bool)>,
    /// How many match lines `lines` holds.
    matched: usize,
}

impl Sink for Collected {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        self.matched += 1;
        let text = String::from_utf8_lossy(mat.bytes());
        let base_line = mat.line_number().unwrap_or_default() as usize;
        // Multiline matches can span several lines; emit each with its exact line number.
        let mut raw_lines = text.split('\n').collect::<Vec<_>>();
        if text.ends_with('\n') {
            raw_lines.pop();
        }
        if raw_lines.is_empty() {
            self.lines.push((base_line, String::new(), true));
        } else {
            for (offset, part) in raw_lines.into_iter().enumerate() {
                self.lines.push((
                    base_line + offset,
                    part.trim_end_matches('\r').to_string(),
                    true,
                ));
            }
        }
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        let text = String::from_utf8_lossy(ctx.bytes());
        self.lines.push((
            ctx.line_number().unwrap_or_default() as usize,
            text.trim_end_matches(['\r', '\n']).to_string(),
            false,
        ));
        Ok(true)
    }
}

/// Runs `search rg <pattern> [path…]` and returns the hashline output.
///
/// # Errors
/// Returns [`Error::Usage`] when the pattern or filter options cannot be built.
pub fn run_rg(pattern: &str, paths: &[PathBuf], opts: &RgOptions) -> Result<String, Error> {
    let patterns = [pattern.to_string()];
    Ok(run_rg_many(&patterns, paths, opts)?.output)
}

/// Runs `search rg` with one or more patterns (or none when `opts.mode == RgOutputMode::Files`).
///
/// # Errors
/// Returns [`Error::Usage`] when patterns or filter options are invalid.
pub fn run_rg_many(
    patterns: &[String],
    paths: &[PathBuf],
    opts: &RgOptions,
) -> Result<RgRunResult, Error> {
    let matcher = if opts.mode == RgOutputMode::Files {
        None
    } else {
        let mut builder = RegexMatcherBuilder::new();
        builder
            .case_insensitive(opts.case == CaseMode::Ignore)
            .case_smart(opts.case == CaseMode::Smart)
            .fixed_strings(opts.literal)
            .word(opts.word)
            .whole_line(opts.line_regexp)
            .multi_line(opts.multiline)
            .dot_matches_new_line(opts.multiline_dotall);
        let matcher = if patterns.len() == 1 {
            builder.build(&patterns[0])
        } else {
            builder.build_many(patterns)
        }
        .map_err(|error| Error::Usage(format!("search rg: {error}")))?;
        Some(matcher)
    };

    let mut searcher_builder = SearcherBuilder::new();
    searcher_builder
        .line_number(true)
        .before_context(opts.before_context)
        .after_context(opts.after_context)
        .invert_match(opts.invert_match)
        .multi_line(opts.multiline)
        .passthru(opts.passthru)
        .max_matches(opts.max_count)
        .binary_detection(if opts.binary {
            BinaryDetection::none()
        } else {
            BinaryDetection::quit(b'\x00')
        });
    let mut searcher = searcher_builder.build();

    let types = build_types(opts)?;
    let mut out = String::new();
    let mut remaining = opts.limit;
    let mut any_matched = false;

    'paths: for root in search_roots(paths) {
        let mut walker = WalkBuilder::new(&root);
        walker
            .hidden(!opts.hidden)
            .follow_links(opts.follow_links)
            .max_depth(opts.max_depth)
            .ignore(!opts.no_ignore && !opts.no_ignore_dot)
            .git_ignore(!opts.no_ignore && !opts.no_ignore_vcs)
            .git_global(!opts.no_ignore && !opts.no_ignore_vcs && !opts.no_ignore_global)
            .git_exclude(!opts.no_ignore && !opts.no_ignore_vcs)
            .parents(!opts.no_ignore && !opts.no_ignore_parent)
            .require_git(opts.require_git)
            .same_file_system(opts.one_file_system);

        if let Some(sort) = opts.sort {
            match sort {
                SortOrder::Ascending => {
                    walker.sort_by_file_path(Ord::cmp);
                }
                SortOrder::Descending => {
                    walker.sort_by_file_path(|a, b| b.cmp(a));
                }
            }
        }
        if let Some(types) = &types {
            walker.types(types.clone());
        }
        if !opts.globs.is_empty() || !opts.iglobs.is_empty() {
            let override_root = if root.is_file() {
                root.parent().unwrap_or_else(|| Path::new("."))
            } else {
                &root
            };
            let mut overrides = OverrideBuilder::new(override_root);
            if opts.glob_case_insensitive {
                overrides.case_insensitive(true).map_err(|error| {
                    Error::Usage(format!("search rg: --glob-case-insensitive: {error}"))
                })?;
            }
            for glob in &opts.globs {
                overrides
                    .add(glob)
                    .map_err(|error| Error::Usage(format!("search rg: -g {glob}: {error}")))?;
            }
            if !opts.iglobs.is_empty() {
                overrides
                    .case_insensitive(true)
                    .map_err(|error| Error::Usage(format!("search rg: --iglob: {error}")))?;
                for glob in &opts.iglobs {
                    overrides.add(glob).map_err(|error| {
                        Error::Usage(format!("search rg: --iglob {glob}: {error}"))
                    })?;
                }
            }
            walker.overrides(
                overrides
                    .build()
                    .map_err(|error| Error::Usage(format!("search rg: -g: {error}")))?,
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
            if opts.mode == RgOutputMode::Files {
                any_matched = true;
                emit_header_only(&mut out, path, &mut remaining);
                if remaining == 0 {
                    break 'paths;
                }
                continue;
            }
            let matcher = matcher.as_ref().expect("matcher built for non-Files mode");
            let mut collected = Collected {
                lines: Vec::new(),
                matched: 0,
            };
            if searcher.search_path(matcher, path, &mut collected).is_err() {
                continue;
            }
            match opts.mode {
                RgOutputMode::FilesWithoutMatch => {
                    if collected.matched == 0 {
                        any_matched = true;
                        emit_header_only(&mut out, path, &mut remaining);
                        if remaining == 0 {
                            break 'paths;
                        }
                    }
                }
                RgOutputMode::Quiet => {
                    if collected.matched > 0 {
                        return Ok(RgRunResult {
                            output: String::new(),
                            matched: true,
                        });
                    }
                }
                RgOutputMode::FilesWithMatches => {
                    if collected.matched > 0 {
                        any_matched = true;
                        emit_header_only(&mut out, path, &mut remaining);
                        if remaining == 0 {
                            break 'paths;
                        }
                    }
                }
                RgOutputMode::Count => {
                    if collected.matched > 0 {
                        any_matched = true;
                        emit_count(&mut out, path, collected.matched, &mut remaining);
                        if remaining == 0 {
                            break 'paths;
                        }
                    }
                }
                RgOutputMode::Matches => {
                    if collected.matched == 0 && !opts.passthru {
                        continue;
                    }
                    if collected.matched > 0 {
                        any_matched = true;
                    }
                    emit_file(&mut out, path, &collected, &mut remaining);
                    if remaining == 0 {
                        break 'paths;
                    }
                }
                RgOutputMode::Files => unreachable!(),
            }
        }
    }

    Ok(RgRunResult {
        output: out,
        matched: any_matched,
    })
}

/// Lists all built-in file types supported by `-t` / `-T` (`--type-list`).
#[must_use]
pub fn type_list() -> String {
    let mut builder = TypesBuilder::new();
    builder.add_defaults();
    let mut out = String::new();
    for def in builder.definitions() {
        let _ = writeln!(out, "{}: {}", def.name(), def.globs().join(", "));
    }
    out
}

fn build_types(opts: &RgOptions) -> Result<Option<ignore::types::Types>, Error> {
    if opts.types.is_empty() && opts.types_not.is_empty() && opts.type_adds.is_empty() {
        return Ok(None);
    }
    let mut builder = TypesBuilder::new();
    builder.add_defaults();
    for spec in &opts.type_adds {
        builder
            .add_def(spec)
            .map_err(|error| Error::Usage(format!("search rg: --type-add {spec}: {error}")))?;
    }
    for name in &opts.types {
        builder.select(name);
    }
    for name in &opts.types_not {
        builder.negate(name);
    }
    builder
        .build()
        .map(Some)
        .map_err(|error| Error::Usage(format!("search rg: -t/-T: {error}")))
}

/// The paths a search walks; the current directory when none are named.
fn search_roots(paths: &[PathBuf]) -> Vec<PathBuf> {
    if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths.to_vec()
    }
}

fn emit_header_only(out: &mut String, path: &Path, remaining: &mut usize) {
    if *remaining == 0 {
        return;
    }
    let _ = writeln!(out, "{}", section_header(&display_path(path)));
    *remaining -= 1;
}

fn emit_count(out: &mut String, path: &Path, count: usize, remaining: &mut usize) {
    if *remaining == 0 {
        return;
    }
    let unit = if count == 1 { "match" } else { "matches" };
    let _ = writeln!(
        out,
        "{} ({count} {unit})",
        section_header(&display_path(path))
    );
    *remaining -= 1;
}

/// Appends one file's `[path]` section to `out`.
fn emit_file(out: &mut String, path: &Path, collected: &Collected, remaining: &mut usize) {
    let _ = writeln!(out, "{}", section_header(&display_path(path)));
    let mut previous = 0;
    for (number, text, _matched) in &collected.lines {
        if *remaining == 0 {
            break;
        }
        if *number > previous + 1 && previous != 0 {
            let _ = writeln!(out, "--");
        }
        let _ = writeln!(out, "{}", hashed_line(*number, text));
        previous = *number;
        *remaining -= 1;
    }
}
