//! `search rg`: line search over the ripgrep engine — `grep-regex` matches
//! and `grep-searcher` streams lines with context — walked by `ignore` so
//! `.gitignore` behaves the way `rg` does. Matched files are emitted as
//! `[path#TAG]` sections of `N:content` lines, the anchors an `edit` call
//! pastes back.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use grep_searcher::{Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

use crate::error::Error;
use crate::{file_tag, numbered_line, section_header};

/// Knobs for `search rg`.
#[derive(Debug, Clone)]
pub struct RgOptions {
    /// `true` matches the pattern literally (`rg -F`).
    pub literal: bool,
    /// Case-insensitive matching (`rg -i`).
    pub ignore_case: bool,
    /// Smart case: insensitive unless the pattern has an uppercase letter
    /// (`rg -S`).
    pub smart_case: bool,
    /// Lines of context around each match (`rg -C`).
    pub context: usize,
    /// Search hidden files too (`rg --hidden`).
    pub hidden: bool,
    /// `-g` glob filters on the searched paths.
    pub globs: Vec<String>,
    /// Match lines to emit in total before stopping; `usize::MAX` for none.
    pub limit: usize,
    /// Emit only the `[path#TAG]` headers of matching files (`rg -l`).
    pub files_with_matches: bool,
}

impl Default for RgOptions {
    fn default() -> Self {
        Self {
            literal: false,
            ignore_case: false,
            smart_case: false,
            context: 0,
            hidden: false,
            globs: Vec::new(),
            limit: 200,
            files_with_matches: false,
        }
    }
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
        self.lines.push((
            mat.line_number().unwrap_or_default() as usize,
            text.trim_end().to_string(),
            true,
        ));
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
            text.trim_end().to_string(),
            false,
        ));
        Ok(true)
    }
}

/// Runs `search rg <pattern> [path…]` and returns the hashline output.
///
/// # Errors
/// Returns [`Error::Usage`] when the pattern cannot be compiled, and
/// [`Error::Io`] when a matching file cannot be read back for its tag.
pub fn run_rg(pattern: &str, paths: &[PathBuf], opts: &RgOptions) -> Result<String, Error> {
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.ignore_case)
        .case_smart(opts.smart_case)
        .fixed_strings(opts.literal)
        .build(pattern)
        .map_err(|error| Error::Usage(format!("search rg: {error}")))?;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(opts.context)
        .after_context(opts.context)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();
    let mut out = String::new();
    let mut remaining = opts.limit;
    'paths: for root in search_roots(paths) {
        let mut walker = WalkBuilder::new(&root);
        walker.hidden(!opts.hidden);
        if !opts.globs.is_empty() {
            let mut overrides = OverrideBuilder::new(&root);
            for glob in &opts.globs {
                overrides
                    .add(glob)
                    .map_err(|error| Error::Usage(format!("search rg: -g {glob}: {error}")))?;
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
            let mut collected = Collected {
                lines: Vec::new(),
                matched: 0,
            };
            if searcher
                .search_path(&matcher, path, &mut collected)
                .is_err()
            {
                continue;
            }
            if collected.matched == 0 {
                continue;
            }
            emit_file(&mut out, path, &collected, opts, &mut remaining)?;
            if remaining == 0 {
                break 'paths;
            }
        }
    }
    Ok(out)
}

/// The paths a search walks; the current directory when none are named.
fn search_roots(paths: &[PathBuf]) -> Vec<PathBuf> {
    if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths.to_vec()
    }
}

/// Appends one file's `[path#TAG]` section to `out`.
fn emit_file(
    out: &mut String,
    path: &Path,
    collected: &Collected,
    opts: &RgOptions,
    remaining: &mut usize,
) -> Result<(), Error> {
    let bytes = std::fs::read(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let text = String::from_utf8_lossy(&bytes);
    let tag = file_tag(&text);
    let display = path.display().to_string();
    let _ = writeln!(out, "{}", section_header(&display, &tag));
    if opts.files_with_matches {
        return Ok(());
    }
    let mut previous = 0;
    for (number, text, _matched) in &collected.lines {
        if *remaining == 0 {
            break;
        }
        if *number > previous + 1 && previous != 0 {
            let _ = writeln!(out, "--");
        }
        let _ = writeln!(out, "{}", numbered_line(*number, text));
        previous = *number;
        *remaining -= 1;
    }
    Ok(())
}
