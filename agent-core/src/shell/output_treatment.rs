//! Classifies foreground shell scripts into verbatim, verbatim-foldable, or
//! command output treatment.
//!
//! - Targeted file reads (`sed -n`, `head`, `tail`, and pipelines ending in
//!   them), reads of saved output files under `outputs/`, and searches (`rg`,
//!   `grep`, `ast-grep`, `sg`, `fd`, read-only `find`) stay
//!   [`OutputTreatment::Verbatim`]: blank lines are preserved and oversized
//!   output is never syntax-folded.
//! - Un-ranged whole-file reads (`cat`, `cat -n`, `nl`) of regular files are
//!   [`OutputTreatment::VerbatimFoldable`]: blank lines are preserved inline,
//!   and oversized source code is syntax-folded instead of head+tail truncated.
//! - Every other script is [`OutputTreatment::Command`].

use std::path::{Component, Path, PathBuf};

use aither_sandbox::{OutputTreatment, ScriptCheck, ShellScript, TreatOutput};

use crate::command_reading::{CommandReading, Intent, Link, ReadRange, Step};

/// Chooses [`OutputTreatment`] for each foreground script.
///
/// Targeted reads, saved-output reads, and searches stay verbatim; un-ranged
/// whole-file reads stay verbatim inline and fold source code on overflow;
/// everything else is treated as command output.
#[derive(Debug, Clone, Default)]
pub struct FileReadsAreVerbatim {
    outputs_dir: Option<PathBuf>,
}

impl FileReadsAreVerbatim {
    /// Recognises reads under `outputs_dir` (in addition to relative
    /// `outputs/...` paths) as saved-output reads that never fold.
    #[must_use]
    pub fn with_outputs_dir(outputs_dir: impl Into<PathBuf>) -> Self {
        Self {
            outputs_dir: Some(outputs_dir.into()),
        }
    }
}

impl TreatOutput for FileReadsAreVerbatim {
    fn treatment(&self, script: &ScriptCheck<'_>) -> OutputTreatment {
        // A script in another language, or one that is not valid shell, is
        // no file read: `CommandReading` reads shell only.
        if !matches!(script.shell, Some(Ok(_))) {
            return OutputTreatment::Command;
        }
        let reading = CommandReading::of(script.script);
        if reading.steps.is_empty() {
            return OutputTreatment::Command;
        }

        let mut overall = OutputTreatment::Verbatim;
        let mut start = 0;
        while start < reading.steps.len() {
            let mut end = start + 1;
            while end < reading.steps.len() && reading.steps[end].link == Link::Pipe {
                end += 1;
            }
            match classify_pipeline(&reading.steps[start..end], self.outputs_dir.as_deref()) {
                OutputTreatment::Command => return OutputTreatment::Command,
                OutputTreatment::VerbatimFoldable => {
                    overall = OutputTreatment::VerbatimFoldable;
                }
                OutputTreatment::Verbatim => {}
            }
            start = end;
        }
        overall
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StageKind {
    /// Un-ranged whole-file read of a regular file (`cat f`, `nl -ba f`).
    WholeRead,
    /// Targeted file read (`sed -n`, `head`, `tail`) or read of a saved output.
    VerbatimRead,
    /// Read-only search (`rg`, `grep`, `ast-grep`, `sg`, `fd`, read-only `find`).
    Search,
    /// Stdin-only targeted slice (`sed -n`, `head`, `tail`) after a pipe.
    SliceFilter,
    /// Stdin-only line numberer (`nl`, `cat -n`) after a pipe.
    NumberFilter,
    /// Anything else.
    Command,
}

fn classify_pipeline(stages: &[Step], outputs_dir: Option<&Path>) -> OutputTreatment {
    let Some((first, rest)) = stages.split_first() else {
        return OutputTreatment::Command;
    };
    let mut treatment = match classify_step(first, false, outputs_dir) {
        StageKind::WholeRead => OutputTreatment::VerbatimFoldable,
        StageKind::VerbatimRead | StageKind::Search => OutputTreatment::Verbatim,
        StageKind::SliceFilter | StageKind::NumberFilter | StageKind::Command => {
            return OutputTreatment::Command;
        }
    };
    for stage in rest {
        match classify_step(stage, true, outputs_dir) {
            StageKind::SliceFilter | StageKind::Search => {
                treatment = OutputTreatment::Verbatim;
            }
            StageKind::NumberFilter => {}
            StageKind::WholeRead | StageKind::VerbatimRead | StageKind::Command => {
                return OutputTreatment::Command;
            }
        }
    }
    treatment
}

fn classify_step(step: &Step, piped_in: bool, outputs_dir: Option<&Path>) -> StageKind {
    match &step.intent {
        Intent::Read { path, range } => match range {
            ReadRange::Whole => {
                if is_saved_output_path(path, outputs_dir) {
                    StageKind::VerbatimRead
                } else {
                    StageKind::WholeRead
                }
            }
            ReadRange::Head { .. } | ReadRange::Tail { .. } | ReadRange::Lines { .. } => {
                StageKind::VerbatimRead
            }
        },
        Intent::Search { .. } => StageKind::Search,
        Intent::Unknown { source } => classify_unknown(source, piped_in, outputs_dir),
        Intent::Edit { .. }
        | Intent::List { .. }
        | Intent::Git { .. }
        | Intent::Build { .. }
        | Intent::Tasks { .. } => StageKind::Command,
    }
}

fn is_saved_output_path(path: &str, outputs_dir: Option<&Path>) -> bool {
    let candidate = Path::new(path);
    if outputs_dir.is_some_and(|dir| candidate.starts_with(dir)) {
        return true;
    }
    let mut components = candidate
        .components()
        .skip_while(|component| matches!(component, Component::CurDir));
    matches!(
        components.next(),
        Some(Component::Normal(segment)) if segment == "outputs"
    ) && components.next().is_some()
}

fn classify_unknown(source: &str, piped_in: bool, outputs_dir: Option<&Path>) -> StageKind {
    if has_unquoted_stdout_redirect(source) {
        return StageKind::Command;
    }
    let Ok(parsed) = ShellScript::parse(source) else {
        return StageKind::Command;
    };
    let [cmd] = parsed.commands() else {
        return StageKind::Command;
    };
    if cmd.is_background()
        || cmd.wrapper().is_some()
        || cmd.interpreter().is_some()
        || cmd.substituted_into().is_some()
        || cmd.pipeline().is_some()
    {
        return StageKind::Command;
    }

    let args = cmd.arguments();
    match cmd.program() {
        "sed" => classify_sed(source, args, piped_in),
        "head" | "tail" => classify_head_or_tail(source, args, piped_in),
        "nl" => classify_nl(source, args, piped_in, outputs_dir),
        "cat" => classify_cat(source, args, piped_in, outputs_dir),
        "rg" | "grep" | "fd" => StageKind::Search,
        "ast-grep" | "sg" => {
            if is_ast_grep_rewriting(args) {
                StageKind::Command
            } else {
                StageKind::Search
            }
        }
        "find" => {
            if is_find_read_only(args) {
                StageKind::Search
            } else {
                StageKind::Command
            }
        }
        _ => StageKind::Command,
    }
}

fn classify_sed(source: &str, args: &[String], piped_in: bool) -> StageKind {
    if has_shell_expansion(source) {
        return StageKind::Command;
    }
    let mut quiet = false;
    let mut has_expr = false;
    let mut positionals: Vec<&str> = Vec::new();
    let mut iter = args.iter();
    let mut rest_positional = false;
    while let Some(arg) = iter.next() {
        if rest_positional || arg == "-" || !arg.starts_with('-') {
            positionals.push(arg);
            continue;
        }
        if arg == "--" {
            rest_positional = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = long
                .split_once('=')
                .map_or((long, None), |(k, v)| (k, Some(v)));
            match name {
                "quiet" | "silent" => quiet = true,
                // "in-place" is rejected by `_`
                "expression" | "file" => {
                    if inline.is_none() && iter.next().is_none() {
                        return StageKind::Command;
                    }
                    has_expr = true;
                }
                "regexp-extended" | "posix" | "unbuffered" | "null-data" | "separate" => {}
                _ => return StageKind::Command,
            }
            continue;
        }
        let Some(cluster) = arg.strip_prefix('-') else {
            continue;
        };
        for (offset, flag) in cluster.char_indices() {
            match flag {
                'n' => quiet = true,
                // 'i' is rejected by `_`
                'e' | 'f' => {
                    let tail = &cluster[offset + flag.len_utf8()..];
                    if tail.is_empty() && iter.next().is_none() {
                        return StageKind::Command;
                    }
                    has_expr = true;
                    break;
                }
                'E' | 'r' | 'u' | 'z' | 's' => {}
                _ => return StageKind::Command,
            }
        }
    }
    if !quiet {
        return StageKind::Command;
    }
    let files = if has_expr {
        positionals.as_slice()
    } else {
        match positionals.split_first() {
            Some((expr, files)) if !expr.is_empty() => files,
            _ => return StageKind::Command,
        }
    };
    if piped_in {
        if files.is_empty() {
            StageKind::SliceFilter
        } else {
            StageKind::Command
        }
    } else if !files.is_empty() && files.iter().all(|file| !file.is_empty()) {
        StageKind::VerbatimRead
    } else {
        StageKind::Command
    }
}

fn classify_head_or_tail(source: &str, args: &[String], piped_in: bool) -> StageKind {
    if has_shell_expansion(source) {
        return StageKind::Command;
    }
    let mut files: Vec<&str> = Vec::new();
    let mut iter = args.iter();
    let mut rest_positional = false;
    while let Some(arg) = iter.next() {
        if rest_positional || arg == "-" || !arg.starts_with('-') {
            files.push(arg);
            continue;
        }
        if arg == "--" {
            rest_positional = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = long
                .split_once('=')
                .map_or((long, None), |(k, v)| (k, Some(v)));
            match name {
                "lines" | "bytes" => {
                    if inline.is_none() && iter.next().is_none() {
                        return StageKind::Command;
                    }
                }
                "quiet" | "silent" | "verbose" | "zero-terminated" => {}
                _ => return StageKind::Command,
            }
            continue;
        }
        let Some(cluster) = arg.strip_prefix('-') else {
            continue;
        };
        if cluster.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        for (offset, flag) in cluster.char_indices() {
            match flag {
                'n' | 'c' => {
                    let tail = &cluster[offset + flag.len_utf8()..];
                    if tail.is_empty() && iter.next().is_none() {
                        return StageKind::Command;
                    }
                    break;
                }
                'q' | 'v' | 'z' => {}
                _ => return StageKind::Command,
            }
        }
    }
    if piped_in {
        if files.is_empty() || files == ["-"] {
            StageKind::SliceFilter
        } else {
            StageKind::Command
        }
    } else if !files.is_empty() && files.iter().all(|file| !file.is_empty() && *file != "-") {
        StageKind::VerbatimRead
    } else {
        StageKind::Command
    }
}

fn classify_nl(
    source: &str,
    args: &[String],
    piped_in: bool,
    outputs_dir: Option<&Path>,
) -> StageKind {
    if has_shell_expansion(source) {
        return StageKind::Command;
    }
    let mut files: Vec<&str> = Vec::new();
    let mut iter = args.iter();
    let mut rest_positional = false;
    while let Some(arg) = iter.next() {
        if rest_positional || arg == "-" || !arg.starts_with('-') {
            files.push(arg);
            continue;
        }
        if arg == "--" {
            rest_positional = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = long
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
                    if inline.is_none() && iter.next().is_none() {
                        return StageKind::Command;
                    }
                }
                "no-renumber" => {}
                _ => return StageKind::Command,
            }
            continue;
        }
        let Some(cluster) = arg.strip_prefix('-') else {
            continue;
        };
        for (offset, flag) in cluster.char_indices() {
            match flag {
                'b' | 'd' | 'f' | 'h' | 'i' | 'l' | 'n' | 's' | 'v' | 'w' => {
                    let tail = &cluster[offset + flag.len_utf8()..];
                    if tail.is_empty() && iter.next().is_none() {
                        return StageKind::Command;
                    }
                    break;
                }
                'p' => {}
                _ => return StageKind::Command,
            }
        }
    }
    if piped_in {
        if files.is_empty() || files == ["-"] {
            StageKind::NumberFilter
        } else {
            StageKind::Command
        }
    } else if !files.is_empty() && files.iter().all(|file| !file.is_empty() && *file != "-") {
        if files
            .iter()
            .all(|file| is_saved_output_path(file, outputs_dir))
        {
            StageKind::VerbatimRead
        } else {
            StageKind::WholeRead
        }
    } else {
        StageKind::Command
    }
}

fn classify_cat(
    source: &str,
    args: &[String],
    piped_in: bool,
    outputs_dir: Option<&Path>,
) -> StageKind {
    if has_shell_expansion(source) {
        return StageKind::Command;
    }
    let mut files: Vec<&str> = Vec::new();
    let mut rest_positional = false;
    for arg in args {
        if rest_positional || arg == "-" || !arg.starts_with('-') {
            files.push(arg);
            continue;
        }
        if arg == "--" {
            rest_positional = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "number" | "number-nonblank" | "squeeze-blank" | "show-all" | "show-ends"
                | "show-tabs" | "show-nonprinting" => {}
                _ => return StageKind::Command,
            }
            continue;
        }
        if !arg.strip_prefix('-').is_some_and(|cluster| {
            cluster.chars().all(|flag| {
                matches!(
                    flag,
                    'A' | 'b' | 'e' | 'E' | 'n' | 's' | 't' | 'T' | 'u' | 'v'
                )
            })
        }) {
            return StageKind::Command;
        }
    }
    if piped_in {
        if files.is_empty() || files == ["-"] {
            StageKind::NumberFilter
        } else {
            StageKind::Command
        }
    } else if !files.is_empty() && files.iter().all(|file| !file.is_empty() && *file != "-") {
        if files
            .iter()
            .all(|file| is_saved_output_path(file, outputs_dir))
        {
            StageKind::VerbatimRead
        } else {
            StageKind::WholeRead
        }
    } else {
        StageKind::Command
    }
}

fn is_ast_grep_rewriting(args: &[String]) -> bool {
    let mut rest_positional = false;
    for arg in args {
        if rest_positional {
            break;
        }
        if arg == "--" {
            rest_positional = true;
            continue;
        }
        if matches!(arg.as_str(), "--update-all" | "--interactive") {
            return true;
        }
        if let Some(cluster) = arg.strip_prefix('-').filter(|c| !c.starts_with('-'))
            && cluster.chars().any(|flag| matches!(flag, 'U' | 'i'))
        {
            return true;
        }
    }
    false
}

fn is_find_read_only(args: &[String]) -> bool {
    !args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-exec"
                | "-execdir"
                | "-ok"
                | "-okdir"
                | "-delete"
                | "-fprint"
                | "-fprint0"
                | "-fprintf"
                | "-fls"
        )
    })
}

/// Whether `source` contains a shell variable or command expansion outside
/// single quotes (`$`, `` ` ``).
fn has_shell_expansion(source: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for ch in source.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if !in_single && ch == '\\' {
            escaped = true;
            continue;
        }
        if !in_double && ch == '\'' {
            in_single = !in_single;
            continue;
        }
        if !in_single && ch == '"' {
            in_double = !in_double;
            continue;
        }
        if !in_single && matches!(ch, '$' | '`') {
            return true;
        }
    }
    false
}

/// Whether `source` contains an unquoted stdout redirection (`>`, `>>`, `1>`,
/// `&>`). Benign stderr redirections (`2>&1`, `2>/dev/null`) are ignored.
fn has_unquoted_stdout_redirect(source: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let bytes = source.as_bytes();
    for (idx, ch) in source.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if !in_single && ch == '\\' {
            escaped = true;
            continue;
        }
        if !in_double && ch == '\'' {
            in_single = !in_single;
            continue;
        }
        if !in_single && ch == '"' {
            in_double = !in_double;
            continue;
        }
        if !in_single && !in_double && ch == '>' {
            let after = &source[idx + 1..];
            let is_stderr = idx > 0
                && bytes[idx - 1] == b'2'
                && (idx == 1 || bytes[idx - 2].is_ascii_whitespace());
            let benign_stderr = is_stderr
                && (after.starts_with("&1") || after.trim_start().starts_with("/dev/null"));
            if !benign_stderr {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests;
