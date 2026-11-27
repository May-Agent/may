//! `search sed`: read-only line-selection slices of files whose output carries
//! hashline anchors — `[path]` sections of `N#HH|content` lines an `edit`
//! patch references directly.
//!
//! Only verbatim line selection (`p` / `q` over numeric, `$`, `+N` or `/regex/`
//! addresses, with optional `!` negation and `;`/newline separation) is
//! accepted. Mutating or text-transforming sed commands (`-i`, `s///`, `d`,
//! `a`, `i`, `c`, `w`, …) fail fast because transformed text would not match
//! the file's real lines.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, Sink, SinkMatch};

use crate::error::Error;
use crate::{display_path, hashed_line, section_header, split_display_lines};

/// Knobs for `search sed`.
#[derive(Debug, Clone, Default)]
pub struct SedOptions {
    /// Treat regex addresses as extended regular expressions (`-E` / `-r`).
    pub extended_regex: bool,
    /// Stop emitting lines across all files after this many lines (`usize::MAX`
    /// for unlimited).
    pub limit: Option<usize>,
}

/// One address in a `sed` print/quit statement.
#[derive(Debug)]
enum Addr {
    /// Exact 1-indexed line number.
    Line(usize),
    /// Last line of the file (`$`).
    Last,
    /// Relative offset `+N` (only valid as range end).
    Relative(usize),
    /// Regular expression `/pattern/` (optional `i`/`I` case-insensitive).
    Regex(RegexMatcher),
}

/// Address range selecting lines for a statement.
#[derive(Debug)]
enum RangeSpec {
    /// No address: matches every line.
    All,
    /// Single address.
    Single(Addr),
    /// Two-address range `start,end`.
    Pair(Addr, Addr),
}

/// Read-only action executed when a line matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Print,
    Quit,
}

/// One compiled statement in a `search sed` script.
#[derive(Debug)]
struct Statement {
    range: RangeSpec,
    negated: bool,
    action: Action,
}

/// Mutable per-file state for one compiled statement.
struct StatementState {
    in_range: bool,
    relative_end: Option<usize>,
}

/// Runs read-only `search sed` scripts over `paths` and returns the hashline
/// output.
///
/// # Errors
/// Returns [`Error::Usage`] when any script expression is invalid, stdin (`-`)
/// is named, or a mutating/transforming sed command is used, and [`Error::Io`]
/// when a file cannot be read.
pub fn run_sed(scripts: &[String], paths: &[PathBuf], opts: &SedOptions) -> Result<String, Error> {
    if paths.is_empty() {
        return Err(Error::Usage(
            "search sed: at least one file path is required (use bare `sed` for stdin pipelines)"
                .to_string(),
        ));
    }
    for path in paths {
        if path.as_os_str() == "-" {
            return Err(Error::Usage(
                "search sed: stdin (`-`) is not supported — pass file paths to get hashline anchors"
                    .to_string(),
            ));
        }
    }
    let statements = parse_scripts(scripts, opts.extended_regex)?;
    let mut out = String::new();
    let mut remaining = opts.limit.unwrap_or(usize::MAX);
    for path in paths {
        if remaining == 0 {
            break;
        }
        emit_file(&mut out, path, &statements, &mut remaining)?;
    }
    Ok(out)
}

fn emit_file(
    out: &mut String,
    path: &Path,
    statements: &[Statement],
    remaining: &mut usize,
) -> Result<(), Error> {
    let bytes = std::fs::read(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let text = String::from_utf8_lossy(&bytes);
    let display = display_path(path);

    let lines = split_display_lines(&text);
    let total = lines.len();

    let mut states: Vec<StatementState> = statements
        .iter()
        .map(|_| StatementState {
            in_range: false,
            relative_end: None,
        })
        .collect();

    let mut selected: Vec<usize> = Vec::new();
    'lines: for (idx, &line_clean) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let mut printed = false;
        let mut quit = false;
        for (stmt, st) in statements.iter().zip(states.iter_mut()) {
            let matched = eval_range(&stmt.range, st, line_no, total, line_clean);
            let active = if stmt.negated { !matched } else { matched };
            if !active {
                continue;
            }
            match stmt.action {
                Action::Print => {
                    if !printed {
                        selected.push(line_no);
                        printed = true;
                    }
                }
                Action::Quit => {
                    quit = true;
                    break;
                }
            }
        }
        if quit {
            break 'lines;
        }
    }

    if selected.is_empty() && total == 0 {
        let _ = writeln!(out, "{}", section_header(&display));
        let _ = writeln!(out, "(empty file)");
        return Ok(());
    }
    if selected.is_empty() {
        return Ok(());
    }

    let _ = writeln!(out, "{}", section_header(&display));
    let mut previous = 0;
    for line_no in selected {
        if *remaining == 0 {
            break;
        }
        if previous != 0 && line_no > previous + 1 {
            let _ = writeln!(out, "--");
        }
        let _ = writeln!(out, "{}", hashed_line(line_no, lines[line_no - 1]));
        previous = line_no;
        *remaining -= 1;
    }
    Ok(())
}

fn eval_range(
    range: &RangeSpec,
    state: &mut StatementState,
    line_no: usize,
    total: usize,
    line: &str,
) -> bool {
    match range {
        RangeSpec::All => true,
        RangeSpec::Single(addr) => matches_addr(addr, line_no, total, line),
        RangeSpec::Pair(start, end) => {
            if !state.in_range {
                if matches_addr(start, line_no, total, line) {
                    // For numeric end already at or before start (e.g. 5,3p or 5,5p),
                    // standard sed matches only the start line.
                    match end {
                        Addr::Line(n) if *n <= line_no => return true,
                        Addr::Relative(offset) => {
                            if *offset == 0 {
                                return true;
                            }
                            state.in_range = true;
                            state.relative_end = Some(line_no.saturating_add(*offset));
                            return true;
                        }
                        _ => {
                            state.in_range = true;
                            return true;
                        }
                    }
                }
                false
            } else {
                let ended = match end {
                    Addr::Line(n) => line_no >= *n,
                    Addr::Relative(_) => state.relative_end.is_some_and(|limit| line_no >= limit),
                    _ => matches_addr(end, line_no, total, line),
                };
                if ended {
                    state.in_range = false;
                    state.relative_end = None;
                }
                true
            }
        }
    }
}

struct LineHit(bool);

impl Sink for LineHit {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, _mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.0 = true;
        Ok(false)
    }
}

fn matches_addr(addr: &Addr, line_no: usize, total: usize, line: &str) -> bool {
    match addr {
        Addr::Line(n) => line_no == *n,
        Addr::Last => line_no == total,
        Addr::Relative(_) => false,
        Addr::Regex(matcher) => {
            let mut hit = LineHit(false);
            let _ = Searcher::new().search_slice(matcher, line.as_bytes(), &mut hit);
            hit.0
        }
    }
}

/// Parses one or more `-e` scripts into compiled read-only statements.
fn parse_scripts(scripts: &[String], extended_regex: bool) -> Result<Vec<Statement>, Error> {
    let mut out = Vec::new();
    for script in scripts {
        let mut cursor = Cursor::new(script, extended_regex);
        cursor.parse_block(&mut out, false)?;
    }
    if out.is_empty() {
        return Err(Error::Usage(
            "search sed: empty script — e.g. `search sed -n '1,120p' file`".to_string(),
        ));
    }
    Ok(out)
}

struct Cursor<'a> {
    src: &'a str,
    pos: usize,
    extended_regex: bool,
}

impl<'a> Cursor<'a> {
    const fn new(src: &'a str, extended_regex: bool) -> Self {
        Self {
            src,
            pos: 0,
            extended_regex,
        }
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn skip_ws(&mut self) {
        while let Some(ch) = self.peek() {
            if matches!(ch, ' ' | '\t' | '\r') {
                self.bump();
            } else {
                break;
            }
        }
    }

    fn skip_separators(&mut self) {
        while let Some(ch) = self.peek() {
            if matches!(ch, ' ' | '\t' | '\r' | '\n' | ';') {
                self.bump();
            } else {
                break;
            }
        }
    }

    fn parse_block(&mut self, out: &mut Vec<Statement>, in_group: bool) -> Result<(), Error> {
        loop {
            self.skip_separators();
            let Some(ch) = self.peek() else {
                if in_group {
                    return Err(Error::Usage(
                        "search sed: unterminated `{` group".to_string(),
                    ));
                }
                return Ok(());
            };
            if ch == '}' {
                if in_group {
                    self.bump();
                    return Ok(());
                }
                return Err(Error::Usage("search sed: unexpected `}`".to_string()));
            }
            if ch == '#' {
                while let Some(c) = self.bump() {
                    if c == '\n' {
                        break;
                    }
                }
                continue;
            }
            let range = self.parse_range()?;
            self.skip_ws();
            let negated = if self.peek() == Some('!') {
                self.bump();
                self.skip_ws();
                true
            } else {
                false
            };
            let Some(cmd) = self.bump() else {
                return Err(Error::Usage(
                    "search sed: missing command after address (expected `p` or `q`, e.g. `1,120p`)"
                        .to_string(),
                ));
            };
            match cmd {
                'p' => out.push(Statement {
                    range,
                    negated,
                    action: Action::Print,
                }),
                'q' | 'Q' => out.push(Statement {
                    range,
                    negated,
                    action: Action::Quit,
                }),
                '{' => {
                    return Err(Error::Usage(
                        "search sed: use `;`-separated statements like `1,50p;50q` instead of `{ … }` blocks"
                            .to_string(),
                    ));
                }
                other => {
                    return Err(Error::Usage(format!(
                        "search sed: command `{other}` is not supported — `search sed` only prints verbatim hash-anchored line ranges (`p`, `q`); use `edit` to modify files or bare `sed` for text transforms"
                    )));
                }
            }
            self.skip_ws();
            if let Some(next) = self.peek()
                && !matches!(next, ';' | '\n' | '}')
            {
                return Err(Error::Usage(format!(
                    "search sed: unexpected `{next}` after `{cmd}` (separate statements with `;`)"
                )));
            }
        }
    }

    fn parse_range(&mut self) -> Result<RangeSpec, Error> {
        let Some(first) = self.parse_addr(false)? else {
            return Ok(RangeSpec::All);
        };
        self.skip_ws();
        if self.peek() == Some(',') {
            self.bump();
            self.skip_ws();
            let second = self.parse_addr(true)?.ok_or_else(|| {
                Error::Usage("search sed: expected end address after `,`".to_string())
            })?;
            Ok(RangeSpec::Pair(first, second))
        } else {
            Ok(RangeSpec::Single(first))
        }
    }

    fn parse_addr(&mut self, allow_relative: bool) -> Result<Option<Addr>, Error> {
        self.skip_ws();
        let Some(ch) = self.peek() else {
            return Ok(None);
        };
        match ch {
            '$' => {
                self.bump();
                Ok(Some(Addr::Last))
            }
            '+' if allow_relative => {
                self.bump();
                let n = self.parse_number()?;
                Ok(Some(Addr::Relative(n)))
            }
            '0'..='9' => {
                let n = self.parse_number()?;
                if n == 0 {
                    return Err(Error::Usage(
                        "search sed: line numbers are 1-indexed (line 0 is invalid)".to_string(),
                    ));
                }
                Ok(Some(Addr::Line(n)))
            }
            '/' => {
                self.bump();
                self.parse_regex('/').map(Some)
            }
            '\\' => {
                self.bump();
                let delim = self.bump().ok_or_else(|| {
                    Error::Usage("search sed: missing regex delimiter after `\\`".to_string())
                })?;
                self.parse_regex(delim).map(Some)
            }
            _ => Ok(None),
        }
    }

    fn parse_number(&mut self) -> Result<usize, Error> {
        let start = self.pos;
        while let Some(ch) = self.peek() {
            if ch.is_ascii_digit() {
                self.bump();
            } else {
                break;
            }
        }
        let raw = &self.src[start..self.pos];
        raw.parse::<usize>()
            .map_err(|_| Error::Usage(format!("search sed: invalid line number `{raw}`")))
    }

    fn parse_regex(&mut self, delim: char) -> Result<Addr, Error> {
        let mut pattern = String::new();
        let mut escaped = false;
        let mut closed = false;
        while let Some(ch) = self.bump() {
            if escaped {
                if ch == delim {
                    pattern.push(delim);
                } else {
                    pattern.push('\\');
                    pattern.push(ch);
                }
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == delim {
                closed = true;
                break;
            }
            pattern.push(ch);
        }
        if !closed {
            return Err(Error::Usage(format!(
                "search sed: unterminated regex address (missing closing `{delim}`)"
            )));
        }
        let mut ignore_case = false;
        while let Some(flag) = self.peek() {
            if matches!(flag, 'i' | 'I') {
                ignore_case = true;
                self.bump();
            } else {
                break;
            }
        }
        let effective = if self.extended_regex {
            pattern
        } else {
            bre_to_ere(&pattern)
        };
        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(ignore_case)
            .build(&effective)
            .map_err(|error| {
                Error::Usage(format!("search sed: invalid regex `{effective}`: {error}"))
            })?;
        Ok(Addr::Regex(matcher))
    }
}

/// Converts common POSIX BRE escapes (`\(`, `\)`, `\{`, `\}`, `\+`, `\?`, `\|`)
/// into standard regex syntax, leaving literal unescaped `(`, `)`, `{`, `}`, `+`,
/// `?`, `|` outside character classes escaped so BRE patterns behave like `sed`.
fn bre_to_ere(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    let mut class_first = false;
    while let Some(ch) = chars.next() {
        if in_class {
            out.push(ch);
            if ch == '\\'
                && let Some(next) = chars.next()
            {
                out.push(next);
            } else if ch == ']' && !class_first {
                in_class = false;
            }
            class_first = false;
            continue;
        }
        match ch {
            '[' => {
                in_class = true;
                class_first = true;
                out.push(ch);
            }
            '\\' => match chars.next() {
                Some(special @ ('(' | ')' | '{' | '}' | '+' | '?' | '|')) => out.push(special),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            '(' | ')' | '{' | '}' | '+' | '?' | '|' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &tempfile::TempDir, name: &str, text: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn selects_numeric_ranges_with_header_and_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let text = "one\ntwo\nthree\nfour\nfive\nsix\n";
        let path = write_file(&dir, "a.txt", text);
        let out = run_sed(
            &["1,2p;5,6p".to_string()],
            std::slice::from_ref(&path),
            &SedOptions::default(),
        )
        .unwrap();
        assert_eq!(
            out,
            format!(
                "[{}]\n{}\n{}\n--\n{}\n{}\n",
                path.display(),
                hashed_line(1, "one"),
                hashed_line(2, "two"),
                hashed_line(5, "five"),
                hashed_line(6, "six"),
            )
        );
    }

    #[test]
    fn selects_regex_and_relative_ranges_like_sed_n() {
        let dir = tempfile::tempdir().unwrap();
        let text = "def a():\n    return 1\n\ndef b():\n    return 2\n";
        let path = write_file(&dir, "m.py", text);
        let out = run_sed(
            &["/def a()/,/return 1/p".to_string()],
            std::slice::from_ref(&path),
            &SedOptions::default(),
        )
        .unwrap();
        assert_eq!(
            out,
            format!(
                "[{}]\n{}\n{}\n",
                path.display(),
                hashed_line(1, "def a():"),
                hashed_line(2, "    return 1"),
            )
        );

        let out2 = run_sed(
            &["/def b/,+1p".to_string()],
            std::slice::from_ref(&path),
            &SedOptions::default(),
        )
        .unwrap();
        assert_eq!(
            out2,
            format!(
                "[{}]\n{}\n{}\n",
                path.display(),
                hashed_line(4, "def b():"),
                hashed_line(5, "    return 2"),
            )
        );
    }

    #[test]
    fn rejects_transforming_or_mutating_sed_commands() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "a.txt", "one\n");
        let err = run_sed(
            &["s/one/two/p".to_string()],
            &[path],
            &SedOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not supported"));
    }
}
