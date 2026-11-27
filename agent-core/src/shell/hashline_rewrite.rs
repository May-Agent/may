//! Static AST-based bare-read rewriter for hashline mode.
//!
//! Rewrites top-level read/search commands (`rg`, `sed -n`, `cat`, `head`,
//! `tail`, `nl`, `ast-grep`, `sg`) into their hashline `search <subcmd>`
//! equivalents using exact UTF-8 byte spans extracted from `brush-parser`, so
//! output automatically carries `[path]` and `N#HH|content` anchors for
//! `edit`.
//!
//! Only safe, top-level, read-only invocations are rewritten; piped-in stages,
//! redirected commands, background jobs, command substitutions, saved-output
//! reads (`outputs/...`), and invocations with unsupported flags or dynamic
//! expansions stay untouched.

use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use aither_sandbox::{RewriteScript, ScriptCheck, ScriptRewrite};
use askama::Template;

use crate::command_reading::{
    CommandWord, TopLevelSimpleCommand, extract_top_level_simple_commands,
};

/// Result of rewriting one or more bare read/search commands in a shell script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashlineRewrite {
    /// The rewritten shell script, ready to execute.
    pub script: String,
    /// Model-facing notice explaining what was rewritten and how to call
    /// `search <subcmd>` directly (or pass `raw: true` to bypass rewriting).
    pub notice: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RewrittenPair {
    from: &'static str,
    to: &'static str,
}

#[derive(Template)]
#[template(path = "notice_hashline_rewrite.txt", escape = "none")]
struct RewriteNoticeTemplate<'a> {
    rewrites: &'a [RewrittenPair],
}

/// Static AST rewriter turning safe top-level bare reads/searches into
/// `search <subcmd>` calls in hashline mode.
#[derive(Debug, Clone, Default)]
pub struct HashlineReadRewriter {
    outputs_dir: Option<PathBuf>,
}

impl HashlineReadRewriter {
    /// Creates a rewriter recognising relative `outputs/...` paths as saved
    /// outputs that are never rewritten.
    #[must_use]
    pub const fn new() -> Self {
        Self { outputs_dir: None }
    }

    /// Recognises reads under `outputs_dir` (in addition to relative
    /// `outputs/...` paths) as saved-output reads that are never rewritten.
    #[must_use]
    pub fn with_outputs_dir(outputs_dir: impl Into<PathBuf>) -> Self {
        Self {
            outputs_dir: Some(outputs_dir.into()),
        }
    }

    /// Rewrites eligible top-level bare read/search commands in `script`.
    ///
    /// Returns `None` if `script` is not valid shell or contains no eligible
    /// bare read/search command.
    ///
    /// # Panics
    /// Panics only if the static Askama notice template fails to render, which
    /// cannot happen for valid UTF-8 inputs.
    #[must_use]
    pub fn rewrite_script(&self, script: &str) -> Option<HashlineRewrite> {
        let commands = extract_top_level_simple_commands(script)?;
        let mut edits: Vec<(Range<usize>, &'static str, RewrittenPair)> = Vec::new();

        for cmd in &commands {
            if let Some(edit) = plan_command_rewrite(cmd, self.outputs_dir.as_deref()) {
                edits.push(edit);
            }
        }
        if edits.is_empty() {
            return None;
        }

        // Verify spans are strictly ordered and non-overlapping before splicing.
        let mut cursor = 0usize;
        for (span, _, _) in &edits {
            if span.start < cursor || span.start > span.end || span.end > script.len() {
                return None;
            }
            cursor = span.end;
        }

        let mut rewritten = String::with_capacity(script.len() + edits.len() * 8);
        let mut pairs: Vec<RewrittenPair> = Vec::new();
        let mut pos = 0usize;
        for (span, replacement, pair) in edits {
            rewritten.push_str(&script[pos..span.start]);
            rewritten.push_str(replacement);
            pos = span.end;
            if !pairs.contains(&pair) {
                pairs.push(pair);
            }
        }
        rewritten.push_str(&script[pos..]);

        let notice = RewriteNoticeTemplate { rewrites: &pairs }
            .render()
            .expect("the hashline rewrite notice template renders");

        Some(HashlineRewrite {
            script: rewritten,
            notice,
        })
    }
}

impl RewriteScript for HashlineReadRewriter {
    fn rewrite(&self, script: &ScriptCheck<'_>) -> Option<ScriptRewrite> {
        let rewritten = self.rewrite_script(script.script)?;
        Some(ScriptRewrite {
            script: rewritten.script,
            notice: Some(rewritten.notice),
        })
    }
}

fn plan_command_rewrite(
    cmd: &TopLevelSimpleCommand,
    outputs_dir: Option<&Path>,
) -> Option<(Range<usize>, &'static str, RewrittenPair)> {
    if cmd.has_assignments || cmd.has_disallowed_redirect || cmd.has_process_substitution {
        return None;
    }

    // When stdout feeds a downstream pipe stage (e.g. `cat a.json | jq .` or
    // `rg foo | wc -l`), adding `[path]` and `N#HH|` anchors would break the
    // consumer. Only allow rewriting search commands piped directly into a
    // single `head` preview stage (`rg ... | head -20`).
    if !cmd.downstream_programs.is_empty() {
        let is_single_head = matches!(
            cmd.downstream_programs.as_slice(),
            [Some(prog)] if prog == "head"
        );
        if !is_single_head || !matches!(cmd.raw_program.as_str(), "rg" | "ast-grep" | "sg") {
            return None;
        }
    }

    // Require every argument to be statically literal (no `$VAR`, `$(...)`,
    // backticks, arithmetic, or tilde expansion).
    let literals: Vec<&str> = cmd
        .args
        .iter()
        .map(|arg| arg.literal.as_deref())
        .collect::<Option<Vec<_>>>()?;

    match cmd.raw_program.as_str() {
        "rg" => rewrite_rg(&literals, &cmd.cd_steps, outputs_dir).then(|| {
            (
                cmd.program_span.clone(),
                "search rg",
                RewrittenPair {
                    from: "rg",
                    to: "search rg",
                },
            )
        }),
        "sed" => rewrite_sed(&literals, &cmd.cd_steps, outputs_dir).then(|| {
            (
                cmd.program_span.clone(),
                "search sed",
                RewrittenPair {
                    from: "sed",
                    to: "search sed",
                },
            )
        }),
        "cat" => rewrite_cat(&literals, &cmd.cd_steps, outputs_dir).then(|| {
            (
                cmd.program_span.clone(),
                "search cat",
                RewrittenPair {
                    from: "cat",
                    to: "search cat",
                },
            )
        }),
        "head" => rewrite_head_or_tail(&literals, false, &cmd.cd_steps, outputs_dir).then(|| {
            (
                cmd.program_span.clone(),
                "search head",
                RewrittenPair {
                    from: "head",
                    to: "search head",
                },
            )
        }),
        "tail" => rewrite_head_or_tail(&literals, true, &cmd.cd_steps, outputs_dir).then(|| {
            (
                cmd.program_span.clone(),
                "search tail",
                RewrittenPair {
                    from: "tail",
                    to: "search tail",
                },
            )
        }),
        "nl" => rewrite_nl(&literals, &cmd.cd_steps, outputs_dir).then(|| {
            (
                cmd.program_span.clone(),
                "search nl",
                RewrittenPair {
                    from: "nl",
                    to: "search nl",
                },
            )
        }),
        "ast-grep" => {
            let span = rewrite_ast_grep(cmd, &literals, outputs_dir)?;
            Some((
                span,
                "search sg",
                RewrittenPair {
                    from: "ast-grep",
                    to: "search sg",
                },
            ))
        }
        "sg" => {
            let span = rewrite_ast_grep(cmd, &literals, outputs_dir)?;
            Some((
                span,
                "search sg",
                RewrittenPair {
                    from: "sg",
                    to: "search sg",
                },
            ))
        }
        _ => None,
    }
}

/// Checks whether `path` targets a saved output under `outputs/` or
/// `outputs_dir`, accounting for any preceding `cd` steps in the script.
fn is_saved_output_path(path: &str, cd_steps: &[String], outputs_dir: Option<&Path>) -> bool {
    if is_direct_saved_output(Path::new(path), outputs_dir) {
        return true;
    }
    if !cd_steps.is_empty() && !Path::new(path).is_absolute() {
        let mut combined = PathBuf::new();
        for step in cd_steps {
            if Path::new(step).is_absolute() {
                combined = PathBuf::from(step);
            } else {
                combined.push(step);
            }
        }
        combined.push(path);
        if is_direct_saved_output(&normalize_lexical(&combined), outputs_dir) {
            return true;
        }
    }
    false
}

fn is_direct_saved_output(candidate: &Path, outputs_dir: Option<&Path>) -> bool {
    let normalized = normalize_lexical(candidate);
    if outputs_dir.is_some_and(|dir| normalized.starts_with(normalize_lexical(dir))) {
        return true;
    }
    let mut components = normalized
        .components()
        .skip_while(|component| matches!(component, Component::CurDir));
    matches!(
        components.next(),
        Some(Component::Normal(segment)) if segment == "outputs"
    ) && components.next().is_some()
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn is_non_negative_int(raw: &str) -> bool {
    !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit())
}

/// Matches the exact option grammar supported by `hashline/src/bin/search.rs`
/// `rg` subcommand so a rewritten `search rg` invocation never fails on an
/// unsupported flag.
fn rewrite_rg(args: &[&str], cd_steps: &[String], outputs_dir: Option<&Path>) -> bool {
    let mut has_explicit_pattern = false;
    let mut files_mode = false;
    let mut positionals: Vec<&str> = Vec::new();
    let mut flags = true;
    let mut i = 0usize;

    while i < args.len() {
        let arg = args[i];
        if !flags || !arg.starts_with('-') || arg == "-" {
            positionals.push(arg);
            i += 1;
            continue;
        }
        if arg == "--" {
            flags = false;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = long
                .split_once('=')
                .map_or((long, None), |(k, v)| (k, Some(v)));
            match name {
                // Boolean flags supported by `search rg`.
                "fixed-strings"
                | "literal"
                | "no-fixed-strings"
                | "ignore-case"
                | "case-sensitive"
                | "no-ignore-case"
                | "smart-case"
                | "no-smart-case"
                | "word-regexp"
                | "no-word-regexp"
                | "line-regexp"
                | "no-line-regexp"
                | "invert-match"
                | "no-invert-match"
                | "multiline"
                | "no-multiline"
                | "multiline-dotall"
                | "no-multiline-dotall"
                | "passthru"
                | "no-passthru"
                | "hidden"
                | "no-hidden"
                | "follow"
                | "no-follow"
                | "text"
                | "binary"
                | "no-text"
                | "no-binary"
                | "no-ignore"
                | "ignore"
                | "no-ignore-vcs"
                | "no-ignore-dot"
                | "no-ignore-global"
                | "no-ignore-parent"
                | "no-require-git"
                | "require-git"
                | "one-file-system"
                | "unrestricted"
                | "glob-case-insensitive"
                | "no-glob-case-insensitive"
                | "files-with-matches"
                | "files-without-match"
                | "count"
                | "sort-files"
                | "line-number"
                | "no-line-number"
                | "with-filename"
                | "no-filename"
                | "heading"
                | "no-heading"
                | "line-buffered"
                | "block-buffered"
                | "no-messages"
                | "no-ignore-messages"
                | "no-config"
                | "mmap"
                | "no-mmap"
                | "crlf"
                | "no-crlf"
                | "column"
                | "no-column"
                | "pretty"
                | "trim" => {
                    if attached.is_some() {
                        return false;
                    }
                }
                "files" => {
                    if attached.is_some() {
                        return false;
                    }
                    files_mode = true;
                }
                "regexp" => {
                    if take_long_value(attached, args, &mut i).is_none() {
                        return false;
                    }
                    has_explicit_pattern = true;
                }
                "file" => {
                    let Some(path) = take_long_value(attached, args, &mut i) else {
                        return false;
                    };
                    if path == "-" {
                        return false;
                    }
                    has_explicit_pattern = true;
                }
                "context" | "before-context" | "after-context" | "max-depth" | "max-count"
                | "limit" => {
                    let Some(val) = take_long_value(attached, args, &mut i) else {
                        return false;
                    };
                    if !is_non_negative_int(val) {
                        return false;
                    }
                }
                "glob" | "iglob" | "type" | "type-not" | "type-add" | "sort" | "sortr"
                | "color" | "colors" | "threads" | "max-columns" | "path-separator"
                | "context-separator" => {
                    if take_long_value(attached, args, &mut i).is_none() {
                        return false;
                    }
                }
                // Unsupported flags (`--json`, `--vimgrep`, `--null`, `--only-matching`,
                // `--replace`, `--stats`, `--pcre2`, `--help`, `--version`, etc.).
                _ => return false,
            }
            i += 1;
            continue;
        }

        // Short flag or bundle.
        let cluster = &arg[1..];
        if cluster.is_empty() {
            return false;
        }
        for (offset, ch) in cluster.char_indices() {
            let rest = &cluster[offset + ch.len_utf8()..];
            match ch {
                'F' | 'i' | 's' | 'S' | 'w' | 'x' | 'v' | 'U' | 'a' | '.' | 'L' | 'u' | 'l'
                | 'c' | 'n' | 'N' | 'H' | 'I' | 'p' => {}
                'e' => {
                    if take_short_value(rest, args, &mut i).is_none() {
                        return false;
                    }
                    has_explicit_pattern = true;
                    break;
                }
                'f' => {
                    let Some(path) = take_short_value(rest, args, &mut i) else {
                        return false;
                    };
                    if path == "-" {
                        return false;
                    }
                    has_explicit_pattern = true;
                    break;
                }
                'C' | 'B' | 'A' | 'd' | 'm' => {
                    let Some(val) = take_short_value(rest, args, &mut i) else {
                        return false;
                    };
                    if !is_non_negative_int(val) {
                        return false;
                    }
                    break;
                }
                'g' | 't' | 'T' | 'j' | 'M' => {
                    if take_short_value(rest, args, &mut i).is_none() {
                        return false;
                    }
                    break;
                }
                // Unsupported short flags (`-0`, `-o`, `-r`, `-P`, `-b`, `-h`, `-V`, etc.).
                _ => return false,
            }
        }
        i += 1;
    }

    if positionals.contains(&"-") {
        return false;
    }
    let paths = if files_mode || has_explicit_pattern {
        positionals.as_slice()
    } else {
        let Some((_, rest)) = positionals.split_first() else {
            return false;
        };
        rest
    };

    paths.is_empty()
        || paths
            .iter()
            .any(|path| !is_saved_output_path(path, cd_steps, outputs_dir))
}

/// Rewrites `sed` only when `-n`/`--quiet`/`--silent` is active, no mutating
/// flags appear, explicit non-`outputs/` file path(s) exist, and every
/// expression uses strictly read-only `search sed` print/quit syntax.
fn rewrite_sed(args: &[&str], cd_steps: &[String], outputs_dir: Option<&Path>) -> bool {
    let mut quiet = false;
    let mut scripts: Vec<&str> = Vec::new();
    let mut positionals: Vec<&str> = Vec::new();
    let mut flags = true;
    let mut i = 0usize;

    while i < args.len() {
        let arg = args[i];
        if !flags || !arg.starts_with('-') || arg == "-" {
            positionals.push(arg);
            i += 1;
            continue;
        }
        if arg == "--" {
            flags = false;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = long
                .split_once('=')
                .map_or((long, None), |(k, v)| (k, Some(v)));
            match name {
                "quiet" | "silent" => {
                    if attached.is_some() {
                        return false;
                    }
                    quiet = true;
                }
                "regexp-extended" => {
                    if attached.is_some() {
                        return false;
                    }
                }
                "expression" => {
                    let Some(expr) = take_long_value(attached, args, &mut i) else {
                        return false;
                    };
                    scripts.push(expr);
                }
                "limit" => {
                    let Some(val) = take_long_value(attached, args, &mut i) else {
                        return false;
                    };
                    if !is_non_negative_int(val) {
                        return false;
                    }
                }
                // Reject `--in-place`, `--file` (unverifiable script file), etc.
                _ => return false,
            }
            i += 1;
            continue;
        }

        let cluster = &arg[1..];
        if cluster.is_empty() {
            return false;
        }
        for (offset, ch) in cluster.char_indices() {
            let rest = &cluster[offset + ch.len_utf8()..];
            match ch {
                'n' => quiet = true,
                'E' | 'r' => {}
                'e' => {
                    let Some(expr) = take_short_value(rest, args, &mut i) else {
                        return false;
                    };
                    scripts.push(expr);
                    break;
                }
                // Reject `-i`, `-f`, `-h`, etc.
                _ => return false,
            }
        }
        i += 1;
    }

    if !quiet {
        return false;
    }
    let paths = if scripts.is_empty() {
        let Some((first, rest)) = positionals.split_first() else {
            return false;
        };
        scripts.push(first);
        rest
    } else {
        positionals.as_slice()
    };

    if paths.is_empty()
        || paths.iter().any(|path| {
            path.is_empty() || *path == "-" || is_saved_output_path(path, cd_steps, outputs_dir)
        })
    {
        return false;
    }

    let mut total_statements = 0usize;
    for expr in &scripts {
        let Some(count) = validate_read_only_sed_script(expr) else {
            return false;
        };
        total_statements += count;
    }
    total_statements > 0
}

/// Validates that a `sed` expression uses strictly the read-only line-slicing
/// grammar supported by `search sed` (`p` / `q` / `Q` over optional 1-based
/// numeric, `$`, `+N`, or `/regex/` addresses with optional `!` negation).
fn validate_read_only_sed_script(src: &str) -> Option<usize> {
    let mut cursor = SedCursor::new(src);
    let mut count = 0usize;
    loop {
        cursor.skip_separators();
        let Some(ch) = cursor.peek() else {
            return Some(count);
        };
        if ch == '#' {
            while let Some(c) = cursor.bump() {
                if c == '\n' {
                    break;
                }
            }
            continue;
        }
        cursor.parse_range()?;
        cursor.skip_ws();
        if cursor.peek() == Some('!') {
            cursor.bump();
            cursor.skip_ws();
        }
        let cmd = cursor.bump()?;
        if !matches!(cmd, 'p' | 'q' | 'Q') {
            return None;
        }
        count += 1;
        cursor.skip_ws();
        if let Some(next) = cursor.peek()
            && !matches!(next, ';' | '\n')
        {
            return None;
        }
    }
}

struct SedCursor<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> SedCursor<'a> {
    const fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\r')) {
            self.bump();
        }
    }

    fn skip_separators(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\r' | '\n' | ';')) {
            self.bump();
        }
    }

    fn parse_range(&mut self) -> Option<()> {
        if !self.parse_addr(false)? {
            return Some(());
        }
        self.skip_ws();
        if self.peek() == Some(',') {
            self.bump();
            self.skip_ws();
            if !self.parse_addr(true)? {
                return None;
            }
        }
        Some(())
    }

    fn parse_addr(&mut self, allow_relative: bool) -> Option<bool> {
        self.skip_ws();
        let Some(ch) = self.peek() else {
            return Some(false);
        };
        match ch {
            '$' => {
                self.bump();
                Some(true)
            }
            '+' if allow_relative => {
                self.bump();
                let _ = self.parse_number()?;
                Some(true)
            }
            '0'..='9' => {
                let n = self.parse_number()?;
                if n == 0 {
                    return None;
                }
                Some(true)
            }
            '/' => {
                self.bump();
                self.parse_regex('/')?;
                Some(true)
            }
            '\\' => {
                self.bump();
                let delim = self.bump()?;
                self.parse_regex(delim)?;
                Some(true)
            }
            _ => Some(false),
        }
    }

    fn parse_number(&mut self) -> Option<usize> {
        let start = self.pos;
        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            self.bump();
        }
        if self.pos == start {
            return None;
        }
        self.src[start..self.pos].parse::<usize>().ok()
    }

    fn parse_regex(&mut self, delim: char) -> Option<()> {
        let mut escaped = false;
        let mut closed = false;
        while let Some(ch) = self.bump() {
            if escaped {
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
        }
        if !closed {
            return None;
        }
        while matches!(self.peek(), Some('i' | 'I')) {
            self.bump();
        }
        Some(())
    }
}

fn rewrite_cat(args: &[&str], cd_steps: &[String], outputs_dir: Option<&Path>) -> bool {
    let mut files: Vec<&str> = Vec::new();
    let mut rest_positional = false;

    for &arg in args {
        if rest_positional || arg == "-" || !arg.starts_with('-') {
            files.push(arg);
            continue;
        }
        if arg == "--" {
            rest_positional = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            if !matches!(long, "number" | "number-nonblank") {
                return false;
            }
            continue;
        }
        let cluster = &arg[1..];
        if cluster.is_empty() || !cluster.chars().all(|flag| matches!(flag, 'n' | 'b')) {
            return false;
        }
    }

    !files.is_empty()
        && files.iter().all(|path| {
            !path.is_empty() && *path != "-" && !is_saved_output_path(path, cd_steps, outputs_dir)
        })
}

fn rewrite_head_or_tail(
    args: &[&str],
    allow_plus_offset: bool,
    cd_steps: &[String],
    outputs_dir: Option<&Path>,
) -> bool {
    let mut files: Vec<&str> = Vec::new();
    let mut rest_positional = false;
    let mut i = 0usize;

    while i < args.len() {
        let arg = args[i];
        if rest_positional || arg == "-" || !arg.starts_with('-') {
            files.push(arg);
            i += 1;
            continue;
        }
        if arg == "--" {
            rest_positional = true;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = long
                .split_once('=')
                .map_or((long, None), |(k, v)| (k, Some(v)));
            if name != "lines" {
                return false;
            }
            let Some(val) = take_long_value(attached, args, &mut i) else {
                return false;
            };
            if !is_valid_line_count(val, allow_plus_offset) {
                return false;
            }
            i += 1;
            continue;
        }
        let cluster = &arg[1..];
        if cluster.is_empty() {
            return false;
        }
        // Shorthand `-N` (e.g. `head -20 file`, `tail -50 file`).
        if is_non_negative_int(cluster) {
            i += 1;
            continue;
        }
        if let Some(rest) = cluster.strip_prefix('n') {
            let Some(val) = take_short_value(rest, args, &mut i) else {
                return false;
            };
            if !is_valid_line_count(val, allow_plus_offset) {
                return false;
            }
            i += 1;
            continue;
        }
        return false;
    }

    !files.is_empty()
        && files.iter().all(|path| {
            !path.is_empty() && *path != "-" && !is_saved_output_path(path, cd_steps, outputs_dir)
        })
}

fn is_valid_line_count(raw: &str, allow_plus_offset: bool) -> bool {
    let digits = if allow_plus_offset {
        raw.strip_prefix('+').unwrap_or(raw)
    } else {
        raw
    };
    is_non_negative_int(digits)
}

fn rewrite_nl(args: &[&str], cd_steps: &[String], outputs_dir: Option<&Path>) -> bool {
    let mut files: Vec<&str> = Vec::new();
    let mut rest_positional = false;
    let mut i = 0usize;

    while i < args.len() {
        let arg = args[i];
        if rest_positional || arg == "-" || !arg.starts_with('-') {
            files.push(arg);
            i += 1;
            continue;
        }
        if arg == "--" {
            rest_positional = true;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = long
                .split_once('=')
                .map_or((long, None), |(k, v)| (k, Some(v)));
            match name {
                "body-numbering"
                | "section-delimiter"
                | "footer-numbering"
                | "header-numbering"
                | "line-increment"
                | "join-blank-lines"
                | "number-format"
                | "number-separator"
                | "starting-line-number"
                | "number-width" => {
                    if take_long_value(attached, args, &mut i).is_none() {
                        return false;
                    }
                }
                "no-renumber" => {
                    if attached.is_some() {
                        return false;
                    }
                }
                _ => return false,
            }
            i += 1;
            continue;
        }
        let cluster = &arg[1..];
        if cluster.is_empty() {
            return false;
        }
        for (offset, ch) in cluster.char_indices() {
            let rest = &cluster[offset + ch.len_utf8()..];
            match ch {
                'b' | 'd' | 'f' | 'h' | 'i' | 'l' | 'n' | 's' | 'v' | 'w' => {
                    if take_short_value(rest, args, &mut i).is_none() {
                        return false;
                    }
                    break;
                }
                'p' => {}
                _ => return false,
            }
        }
        i += 1;
    }

    !files.is_empty()
        && files.iter().all(|path| {
            !path.is_empty() && *path != "-" && !is_saved_output_path(path, cd_steps, outputs_dir)
        })
}

/// Rewrites read-only `ast-grep` / `sg` calls matching `search sg`'s supported
/// options (and requiring `--lang`/`-l`). Strips an optional leading `run`
/// subcommand by expanding the replacement span to cover `<prog> run`.
fn rewrite_ast_grep(
    cmd: &TopLevelSimpleCommand,
    args: &[&str],
    outputs_dir: Option<&Path>,
) -> Option<Range<usize>> {
    let (replace_span, effective_args) = match (cmd.args.first(), args.first().copied()) {
        (
            Some(CommandWord {
                span: Some(run_span),
                ..
            }),
            Some("run"),
        ) => (cmd.program_span.start..run_span.end, &args[1..]),
        (_, Some("scan" | "test" | "new" | "lsp" | "completions" | "docs")) => return None,
        _ => (cmd.program_span.clone(), args),
    };

    let mut has_lang = false;
    let mut has_explicit_pattern = false;
    let mut positionals: Vec<&str> = Vec::new();
    let mut flags = true;
    let mut i = 0usize;

    while i < effective_args.len() {
        let arg = effective_args[i];
        if !flags || !arg.starts_with('-') || arg == "-" {
            positionals.push(arg);
            i += 1;
            continue;
        }
        if arg == "--" {
            flags = false;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = long
                .split_once('=')
                .map_or((long, None), |(k, v)| (k, Some(v)));
            match name {
                "lang" => {
                    let val = take_long_value(attached, effective_args, &mut i)?;
                    if val.is_empty() {
                        return None;
                    }
                    has_lang = true;
                }
                "pattern" => {
                    let val = take_long_value(attached, effective_args, &mut i)?;
                    if val.is_empty() || val == "-" {
                        return None;
                    }
                    has_explicit_pattern = true;
                }
                "context" | "before-context" | "after-context" | "max-count" | "limit" => {
                    let val = take_long_value(attached, effective_args, &mut i)?;
                    if !is_non_negative_int(val) {
                        return None;
                    }
                }
                "glob" => {
                    let _ = take_long_value(attached, effective_args, &mut i)?;
                }
                "hidden" | "line-number" => {
                    if attached.is_some() {
                        return None;
                    }
                }
                // Reject `--rewrite`, `--update-all`, `--interactive`, `--json`, etc.
                _ => return None,
            }
            i += 1;
            continue;
        }

        let cluster = &arg[1..];
        if cluster.is_empty() {
            return None;
        }
        for (offset, ch) in cluster.char_indices() {
            let rest = &cluster[offset + ch.len_utf8()..];
            match ch {
                'l' => {
                    let val = take_short_value(rest, effective_args, &mut i)?;
                    if val.is_empty() {
                        return None;
                    }
                    has_lang = true;
                    break;
                }
                'p' => {
                    let val = take_short_value(rest, effective_args, &mut i)?;
                    if val.is_empty() || val == "-" {
                        return None;
                    }
                    has_explicit_pattern = true;
                    break;
                }
                'C' | 'B' | 'A' | 'm' => {
                    let val = take_short_value(rest, effective_args, &mut i)?;
                    if !is_non_negative_int(val) {
                        return None;
                    }
                    break;
                }
                'g' => {
                    let _ = take_short_value(rest, effective_args, &mut i)?;
                    break;
                }
                'n' => {}
                // Reject `-r`, `-U`, `-i`, `-h`, `-V`, etc.
                _ => return None,
            }
        }
        i += 1;
    }

    if !has_lang {
        return None;
    }
    let paths = if has_explicit_pattern {
        positionals.as_slice()
    } else {
        let (first, rest) = positionals.split_first()?;
        if first.is_empty() || *first == "-" {
            return None;
        }
        rest
    };

    if !paths.is_empty()
        && paths
            .iter()
            .all(|path| is_saved_output_path(path, &cmd.cd_steps, outputs_dir))
    {
        return None;
    }

    Some(replace_span)
}

fn take_long_value<'a>(
    attached: Option<&'a str>,
    args: &[&'a str],
    i: &mut usize,
) -> Option<&'a str> {
    attached.or_else(|| {
        *i += 1;
        args.get(*i).copied()
    })
}

fn take_short_value<'a>(rest: &'a str, args: &[&'a str], i: &mut usize) -> Option<&'a str> {
    if rest.is_empty() {
        *i += 1;
        args.get(*i).copied()
    } else {
        Some(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewriter() -> HashlineReadRewriter {
        HashlineReadRewriter::with_outputs_dir("/session/outputs")
    }

    #[test]
    fn rewrites_supported_bare_read_and_search_commands() {
        let rw = rewriter();
        for (input, expected_script, expected_from, expected_to) in [
            (
                "cd repo && rg -n foo src/ | head -20",
                "cd repo && search rg -n foo src/ | head -20",
                "rg",
                "search rg",
            ),
            (
                "rg -niC2 -g '*.py' 'def foo' src/",
                "search rg -niC2 -g '*.py' 'def foo' src/",
                "rg",
                "search rg",
            ),
            (
                "rg --files src/",
                "search rg --files src/",
                "rg",
                "search rg",
            ),
            (
                "rg -l --no-heading foo src/",
                "search rg -l --no-heading foo src/",
                "rg",
                "search rg",
            ),
            (
                "rg --passthru -c foo src/",
                "search rg --passthru -c foo src/",
                "rg",
                "search rg",
            ),
            (
                "sed -n '1,50p' a.py",
                "search sed -n '1,50p' a.py",
                "sed",
                "search sed",
            ),
            (
                "sed -n '10p;40,80p' a.py",
                "search sed -n '10p;40,80p' a.py",
                "sed",
                "search sed",
            ),
            (
                "sed -n '100,$p' a.py",
                "search sed -n '100,$p' a.py",
                "sed",
                "search sed",
            ),
            (
                "sed -nE '/def foo/,+20p' a.py",
                "search sed -nE '/def foo/,+20p' a.py",
                "sed",
                "search sed",
            ),
            (
                "sed -n -e '1,5p' -e '20,30p' a.py 2>/dev/null",
                "search sed -n -e '1,5p' -e '20,30p' a.py 2>/dev/null",
                "sed",
                "search sed",
            ),
            ("cat a.py", "search cat a.py", "cat", "search cat"),
            (
                "cat -n a.py b.py",
                "search cat -n a.py b.py",
                "cat",
                "search cat",
            ),
            (
                "cat --number-nonblank a.py",
                "search cat --number-nonblank a.py",
                "cat",
                "search cat",
            ),
            (
                "head -20 a.py",
                "search head -20 a.py",
                "head",
                "search head",
            ),
            (
                "head -n 30 a.py",
                "search head -n 30 a.py",
                "head",
                "search head",
            ),
            (
                "head --lines=15 a.py",
                "search head --lines=15 a.py",
                "head",
                "search head",
            ),
            ("head a.py", "search head a.py", "head", "search head"),
            (
                "tail -n +10 a.py",
                "search tail -n +10 a.py",
                "tail",
                "search tail",
            ),
            (
                "tail -40 a.py",
                "search tail -40 a.py",
                "tail",
                "search tail",
            ),
            (
                "tail --lines=+5 a.py",
                "search tail --lines=+5 a.py",
                "tail",
                "search tail",
            ),
            ("nl -ba a.py", "search nl -ba a.py", "nl", "search nl"),
            ("nl a.py", "search nl a.py", "nl", "search nl"),
            (
                "ast-grep run --lang rust --pattern 'fn $NAME($$$ARGS)' src/",
                "search sg --lang rust --pattern 'fn $NAME($$$ARGS)' src/",
                "ast-grep",
                "search sg",
            ),
            (
                "sg -l py -p 'def $F($$$): $$$' src/",
                "search sg -l py -p 'def $F($$$): $$$' src/",
                "sg",
                "search sg",
            ),
        ] {
            let result = rw
                .rewrite_script(input)
                .unwrap_or_else(|| panic!("expected rewrite for `{input}`"));
            assert_eq!(result.script, expected_script, "input: {input}");
            assert!(
                result.notice.contains(&format!("`{expected_from}`"))
                    && result.notice.contains(&format!("`{expected_to}`"))
                    && result.notice.contains("raw: true"),
                "notice missing expected guidance for `{input}`: {}",
                result.notice
            );
        }
    }

    #[test]
    fn rewrites_multiple_chained_reads_and_preserves_unicode_byte_spans() {
        let rw = rewriter();
        let script = "cd repo && rg -n '日本語テスト' src/ && sed -n '1,20p' a.py; head -10 b.py";
        let result = rw.rewrite_script(script).expect("rewritten");
        assert_eq!(
            result.script,
            "cd repo && search rg -n '日本語テスト' src/ && search sed -n '1,20p' a.py; search head -10 b.py"
        );
        assert!(
            result.notice.contains("`rg` to `search rg`"),
            "{}",
            result.notice
        );
        assert!(
            result.notice.contains("`sed` to `search sed`"),
            "{}",
            result.notice
        );
        assert!(
            result.notice.contains("`head` to `search head`"),
            "{}",
            result.notice
        );
        assert!(result.notice.contains("raw: true"), "{}", result.notice);
    }

    #[test]
    fn never_rewrites_piped_redirected_mutating_outputs_or_unsupported_commands() {
        let rw = rewriter();
        for script in [
            // Already using `search`
            "search rg -n foo src/",
            "search sed -n '1,20p' a.py",
            // Piped-out into data processors or non-search `| head` must NEVER be rewritten
            "rg foo src/ | wc -l",
            "rg -l foo src/ | xargs grep bar",
            "rg foo src/ | head -20 | wc -l",
            "rg -q foo src/",
            "rg --quiet foo src/",
            "cat Cargo.toml | jq .",
            "cat a.py | wc -l",
            "cat a.py | head -20",
            "sed -n '1,10p' a.py | python3 -c 'pass'",
            "head -20 a.py | tail -5",
            // Piped-in stages must NEVER be rewritten
            "foo | sed -n '1,10p'",
            "foo | rg bar",
            "foo | head -20",
            "foo | tail -n +10",
            "foo | cat -n",
            "foo | nl -ba",
            // Redirections (stdout, stdin, heredoc, herestring)
            "cat a.py > b.py",
            "cat a.py >> b.py",
            "cat a.py &> b.py",
            "cat <<'EOF' > a.py\nhello\nEOF",
            "cat < a.py",
            "rg foo <<< 'hello'",
            "sed -n '1,5p' a.py > slice.py",
            // Non-read-only or non-`-n` sed
            "sed 's/a/b/' a.py",
            "sed -i 's/a/b/' a.py",
            "sed -n 's/a/b/p' a.py",
            "sed -n '/foo/d' a.py",
            "sed -n '1,5p' -f rules.sed a.py",
            "sed -n '0,5p' a.py",
            "sed -n '1,10{p;q}' a.py",
            // Saved outputs under outputs/ or outputs_dir
            "cat outputs/out.txt",
            "cat ./outputs/out.txt",
            "cat /session/outputs/out.txt",
            "cd /session/outputs && cat out.txt",
            "head -20 outputs/out.txt",
            "sed -n '1,10p' outputs/out.txt",
            "rg needle outputs/out.txt",
            // Unsupported cat/head/tail/rg/sg flags or stdin `-`
            "cat -A a.py",
            "cat -v a.py",
            "cat -s a.py",
            "cat -",
            "cat",
            "head -c 64 a.py",
            "head -z -n 5 a.py",
            "head -n -5 a.py",
            "head -20 -",
            "tail -f a.py",
            "tail -F a.py",
            "tail --follow a.py",
            "tail -c 64 a.py",
            "rg --json foo src/",
            "rg --vimgrep foo src/",
            "rg -0 foo src/",
            "rg --null foo src/",
            "rg -o foo src/",
            "rg --only-matching foo src/",
            "rg -r bar foo src/",
            "rg --replace=bar foo src/",
            "rg --stats foo src/",
            "rg -P 'foo(?=bar)' src/",
            "rg foo -",
            "rg -f - src/",
            "sg -p 'foo' src/",
            "sg -l rust -p 'foo' -r 'bar' -U src/",
            "sg -l rust -p 'foo' --json src/",
            "ast-grep scan src/",
            // Command substitutions, dynamic expansions, background, compound commands
            "X=$(cat version.txt)",
            "echo `head -1 version.txt`",
            "cat \"$FILE\"",
            "sed -n \"$RANGE\" a.py",
            "cat a.py &",
            "! rg foo src/",
            "if true; then cat a.py; fi",
            "for f in a.py; do cat a.py; done",
            "LC_ALL=C rg foo src/",
            "/bin/cat a.py",
            // Invalid shell
            "cat 'unterminated",
        ] {
            assert_eq!(
                rw.rewrite_script(script),
                None,
                "must NOT rewrite `{script}`"
            );
        }
    }
}
