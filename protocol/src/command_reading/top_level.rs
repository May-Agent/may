//! Top-level simple-command extraction with exact byte spans via `brush-parser`.
//!
//! Used by static script rewriters that need to inspect top-level commands
//! (top-level lists, `&&`/`||`/`;` chains, and stage 0 of pipelines) and
//! replace exact source spans without regex heuristics.

use std::ops::Range;

use brush_parser::ast::{
    self, AndOrList, Command, CommandPrefixOrSuffixItem, CompoundList, IoFileRedirectKind,
    IoFileRedirectTarget, IoRedirect, Pipeline, SeparatorOperator, SimpleCommand,
};
use brush_parser::word::{WordPiece, WordPieceWithSource};
use brush_parser::{Parser, ParserOptions, SourceSpan};

/// A single argument word in a [`TopLevelSimpleCommand`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandWord {
    /// Raw source token text as written in the script.
    pub raw: String,
    /// Unquoted value when the word contains no runtime shell expansion
    /// (`$VAR`, `$(...)`, backticks, arithmetic, tilde, or gettext); `None`
    /// when dynamic.
    pub literal: Option<String>,
    /// Exact UTF-8 byte range of this word in the original script, when known
    /// and verified against the source slice.
    pub span: Option<Range<usize>>,
}

/// One top-level simple command eligible for static inspection or span-based
/// rewriting.
///
/// Only commands in top-level sequential lists, `&&`/`||`/`;` chains, or stage
/// 0 of a pipeline are emitted. Background (`&`), negated (`!`), timed
/// (`time`), piped-in (`stage > 0`), compound (`if`/`for`/`while`/subshell),
/// function-body, and command-substitution commands are never emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopLevelSimpleCommand {
    /// Raw program word as written in the script (e.g. `rg`, `sed`, `ast-grep`).
    pub raw_program: String,
    /// Exact UTF-8 byte range of the program word in the original script.
    pub program_span: Range<usize>,
    /// Positional and flag arguments following the program name.
    pub args: Vec<CommandWord>,
    /// Relative or absolute `cd` steps active before this command executes.
    pub cd_steps: Vec<String>,
    /// Whether the command carries any prefix/suffix variable assignment.
    pub has_assignments: bool,
    /// Whether the command carries any non-benign redirection (anything other
    /// than `2>&1` or `2>/dev/null`).
    pub has_disallowed_redirect: bool,
    /// Whether the command carries a process substitution (`<(...)` / `>(...)`).
    pub has_process_substitution: bool,
    /// Raw program names of any downstream pipeline stages after this command
    /// (`empty` when not piped out; `None` for a non-simple downstream stage).
    pub downstream_programs: Vec<Option<String>>,
}

/// Parses `script` with `brush-parser` and extracts every top-level simple
/// command in source order with verified UTF-8 byte spans.
///
/// Returns `None` if `script` is not valid shell.
#[must_use]
pub fn extract_top_level_simple_commands(script: &str) -> Option<Vec<TopLevelSimpleCommand>> {
    let program = Parser::new(script.as_bytes(), &ParserOptions::default())
        .parse_program()
        .ok()?;
    let char_offsets = char_to_byte_offsets(script);
    let mut commands = Vec::new();
    let mut cd_steps = Vec::new();
    for list in &program.complete_commands {
        walk_compound_list(list, script, &char_offsets, &mut cd_steps, &mut commands);
    }
    Some(commands)
}

/// Maps 0-based `brush-parser` character indices to UTF-8 byte offsets in
/// `script` in a single linear pass.
fn char_to_byte_offsets(script: &str) -> Vec<usize> {
    script
        .char_indices()
        .map(|(byte_idx, _)| byte_idx)
        .chain(std::iter::once(script.len()))
        .collect()
}

/// Converts a `brush-parser` character span into a verified UTF-8 byte range
/// matching `expected` verbatim in `script`.
fn span_to_bytes(
    span: &SourceSpan,
    offsets: &[usize],
    script: &str,
    expected: &str,
) -> Option<Range<usize>> {
    let start = *offsets.get(span.start.index)?;
    let end = *offsets.get(span.end.index)?;
    (start <= end && script.get(start..end) == Some(expected)).then_some(start..end)
}

fn walk_compound_list(
    list: &CompoundList,
    script: &str,
    offsets: &[usize],
    cd_steps: &mut Vec<String>,
    out: &mut Vec<TopLevelSimpleCommand>,
) {
    for item in &list.0 {
        // Never rewrite backgrounded (`&`) commands; also isolate any `cd`
        // inside a background job so it does not affect later commands.
        if matches!(item.1, SeparatorOperator::Async) {
            continue;
        }
        walk_and_or(&item.0, script, offsets, cd_steps, out);
    }
}

fn walk_and_or(
    list: &AndOrList,
    script: &str,
    offsets: &[usize],
    cd_steps: &mut Vec<String>,
    out: &mut Vec<TopLevelSimpleCommand>,
) {
    walk_pipeline(&list.first, script, offsets, cd_steps, out);
    for next in &list.additional {
        match next {
            ast::AndOr::And(pipeline) | ast::AndOr::Or(pipeline) => {
                walk_pipeline(pipeline, script, offsets, cd_steps, out);
            }
        }
    }
}

fn walk_pipeline(
    pipeline: &Pipeline,
    script: &str,
    offsets: &[usize],
    cd_steps: &mut Vec<String>,
    out: &mut Vec<TopLevelSimpleCommand>,
) {
    if pipeline.bang || pipeline.timed.is_some() {
        return;
    }
    let Some(Command::Simple(simple)) = pipeline.seq.first() else {
        return;
    };
    let downstream_programs: Vec<Option<String>> = pipeline
        .seq
        .iter()
        .skip(1)
        .map(|stage| match stage {
            Command::Simple(next) => next.word_or_name.as_ref().map(|word| word.value.clone()),
            _ => None,
        })
        .collect();
    let piped_out = !downstream_programs.is_empty();
    let Some(cmd) = parse_simple(simple, script, offsets, cd_steps, downstream_programs) else {
        return;
    };
    // Track `cd` across sequential chains (never inside a multi-stage pipeline,
    // where each stage runs in its own subshell).
    if !piped_out
        && cmd.raw_program == "cd"
        && !cmd.has_assignments
        && !cmd.has_disallowed_redirect
        && !cmd.has_process_substitution
    {
        apply_cd(&cmd.args, cd_steps);
        return;
    }
    out.push(cmd);
}

fn parse_simple(
    simple: &SimpleCommand,
    script: &str,
    offsets: &[usize],
    cd_steps: &[String],
    downstream_programs: Vec<Option<String>>,
) -> Option<TopLevelSimpleCommand> {
    let program_word = simple.word_or_name.as_ref()?;
    let program_span = span_to_bytes(
        program_word.loc.as_ref()?,
        offsets,
        script,
        &program_word.value,
    )?;

    let mut args = Vec::new();
    let mut has_assignments = false;
    let mut has_disallowed_redirect = false;
    let mut has_process_substitution = false;

    if let Some(prefix) = &simple.prefix {
        for item in &prefix.0 {
            inspect_affix(
                item,
                script,
                offsets,
                &mut args,
                &mut has_assignments,
                &mut has_disallowed_redirect,
                &mut has_process_substitution,
            );
        }
    }
    if let Some(suffix) = &simple.suffix {
        for item in &suffix.0 {
            inspect_affix(
                item,
                script,
                offsets,
                &mut args,
                &mut has_assignments,
                &mut has_disallowed_redirect,
                &mut has_process_substitution,
            );
        }
    }

    Some(TopLevelSimpleCommand {
        raw_program: program_word.value.clone(),
        program_span,
        args,
        cd_steps: cd_steps.to_vec(),
        has_assignments,
        has_disallowed_redirect,
        has_process_substitution,
        downstream_programs,
    })
}

fn inspect_affix(
    item: &CommandPrefixOrSuffixItem,
    script: &str,
    offsets: &[usize],
    args: &mut Vec<CommandWord>,
    has_assignments: &mut bool,
    has_disallowed_redirect: &mut bool,
    has_process_substitution: &mut bool,
) {
    match item {
        CommandPrefixOrSuffixItem::Word(word) => {
            let span = word
                .loc
                .as_ref()
                .and_then(|loc| span_to_bytes(loc, offsets, script, &word.value));
            args.push(CommandWord {
                literal: static_literal(&word.value),
                raw: word.value.clone(),
                span,
            });
        }
        CommandPrefixOrSuffixItem::AssignmentWord(..) => {
            *has_assignments = true;
        }
        CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
            if !is_benign_stderr_redirect(redirect) {
                *has_disallowed_redirect = true;
            }
        }
        CommandPrefixOrSuffixItem::ProcessSubstitution(..) => {
            *has_process_substitution = true;
        }
    }
}

/// Only `2>&1` and `2>/dev/null` (or `2>>/dev/null`, `2>|/dev/null`) are benign
/// stderr redirections that do not divert stdout or feed stdin.
fn is_benign_stderr_redirect(redirect: &IoRedirect) -> bool {
    match redirect {
        IoRedirect::File(Some(2), IoFileRedirectKind::DuplicateOutput, target) => match target {
            IoFileRedirectTarget::Fd(1) => true,
            IoFileRedirectTarget::Duplicate(word) => {
                static_literal(&word.value).as_deref() == Some("1")
            }
            IoFileRedirectTarget::Filename(_)
            | IoFileRedirectTarget::Fd(_)
            | IoFileRedirectTarget::ProcessSubstitution(..) => false,
        },
        IoRedirect::File(
            Some(2),
            IoFileRedirectKind::Write | IoFileRedirectKind::Clobber | IoFileRedirectKind::Append,
            IoFileRedirectTarget::Filename(word),
        ) => static_literal(&word.value).as_deref() == Some("/dev/null"),
        _ => false,
    }
}

/// Unquotes `raw` when every word piece is statically known without shell
/// variable/command/arithmetic/tilde expansion.
fn static_literal(raw: &str) -> Option<String> {
    let pieces = brush_parser::word::parse(raw, &ParserOptions::default()).ok()?;
    let mut out = String::new();
    append_static_pieces(&pieces, &mut out)?;
    Some(out)
}

fn append_static_pieces(pieces: &[WordPieceWithSource], out: &mut String) -> Option<()> {
    for WordPieceWithSource { piece, .. } in pieces {
        match piece {
            WordPiece::Text(part) | WordPiece::SingleQuotedText(part) => out.push_str(part),
            WordPiece::EscapeSequence(part) => out.push_str(part.strip_prefix('\\')?),
            WordPiece::DoubleQuotedSequence(inner) => append_static_pieces(inner, out)?,
            WordPiece::AnsiCQuotedText(_)
            | WordPiece::GettextDoubleQuotedSequence(_)
            | WordPiece::TildeExpansion(_)
            | WordPiece::ParameterExpansion(_)
            | WordPiece::CommandSubstitution(_)
            | WordPiece::BackquotedCommandSubstitution(_)
            | WordPiece::ArithmeticExpression(_) => return None,
        }
    }
    Some(())
}

fn apply_cd(args: &[CommandWord], cd_steps: &mut Vec<String>) {
    let mut after_double_dash = false;
    let mut target: Option<&str> = None;
    for arg in args {
        let Some(text) = arg.literal.as_deref() else {
            // Dynamic `cd $DIR`: invalidate tracked steps conservatively.
            cd_steps.clear();
            return;
        };
        if !after_double_dash && text == "--" {
            after_double_dash = true;
            continue;
        }
        if !after_double_dash && text.starts_with('-') && text != "-" {
            continue;
        }
        target = Some(text);
        break;
    }
    match target {
        Some(dir) if !dir.is_empty() && dir != "-" => {
            if dir.starts_with('/') {
                cd_steps.clear();
            }
            cd_steps.push(dir.to_string());
        }
        _ => {
            cd_steps.clear();
        }
    }
}
