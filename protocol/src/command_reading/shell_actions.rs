//! AST-level extraction of file-targeting shell actions (output redirections,
//! heredoc/here-string interpreter scripts, and command invocations with active
//! `cd` scope) using `brush-parser`.

use brush_parser::ast::{
    self, AndOrList, Command, CommandPrefixOrSuffixItem, CompoundCommand, CompoundList,
    IoFileRedirectKind, IoFileRedirectTarget, IoHereDocument, IoRedirect, Pipeline, RedirectList,
    SimpleCommand,
};
use brush_parser::word::{WordPiece, WordPieceWithSource};
use brush_parser::{Parser, ParserOptions};

/// Deepest nesting of `sh -c` / `eval` / command substitutions followed.
const MAX_DEPTH: usize = 16;

/// One file-relevant action observed in a shell script's AST, paired with the
/// `cd` steps active at that point in the script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellAction {
    /// Relative or absolute `cd` arguments applied in order from the script's
    /// starting directory before this action executes.
    pub cd_steps: Vec<String>,
    /// The action performed.
    pub kind: ShellActionKind,
}

/// The kind of file-relevant shell action extracted from a script's AST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellActionKind {
    /// An output redirection (`>`, `>>`, `>|`, `&>`, `&>>`, `<>`) targeting a
    /// path (never an fd duplication like `2>&1`).
    OutputRedirect {
        /// Unquoted target word (preserving variable references like `$TMPDIR/...`).
        target: String,
    },
    /// A simple command invocation (after unwrapping wrappers like `env`/`sudo`/`time`).
    Command {
        /// Lower-cased program basename without `.exe`.
        program: String,
        /// Unquoted arguments after the program name.
        args: Vec<String>,
    },
    /// A simple command receiving a literal here-document or here-string on
    /// stdin (directly or via `cat <<'EOF' | <cmd>`).
    StdinScript {
        /// Lower-cased program basename without `.exe`.
        program: String,
        /// Unquoted arguments after the program name.
        args: Vec<String>,
        /// Text fed on standard input.
        stdin: String,
    },
}

/// Parses `script` with `brush-parser` and extracts its file-relevant actions
/// in execution order. Returns `None` if `script` is not valid shell.
#[must_use]
pub fn inspect_shell_actions(script: &str) -> Option<Vec<ShellAction>> {
    let mut actions = Vec::new();
    let mut cd_steps = Vec::new();
    walk_script(script, 0, &mut cd_steps, &mut actions)?;
    Some(actions)
}

fn walk_script(
    script: &str,
    depth: usize,
    cd_steps: &mut Vec<String>,
    out: &mut Vec<ShellAction>,
) -> Option<()> {
    if depth > MAX_DEPTH {
        return Some(());
    }
    let program = Parser::new(script.as_bytes(), &ParserOptions::default())
        .parse_program()
        .ok()?;
    for list in &program.complete_commands {
        walk_compound_list(list, depth, cd_steps, out);
    }
    Some(())
}

fn walk_compound_list(
    list: &CompoundList,
    depth: usize,
    cd_steps: &mut Vec<String>,
    out: &mut Vec<ShellAction>,
) {
    for item in &list.0 {
        if matches!(item.1, ast::SeparatorOperator::Async) {
            let mut sub_cd = cd_steps.clone();
            walk_and_or(&item.0, depth, &mut sub_cd, out);
        } else {
            walk_and_or(&item.0, depth, cd_steps, out);
        }
    }
}

fn walk_and_or(
    list: &AndOrList,
    depth: usize,
    cd_steps: &mut Vec<String>,
    out: &mut Vec<ShellAction>,
) {
    walk_pipeline(&list.first, depth, cd_steps, out);
    for next in &list.additional {
        match next {
            ast::AndOr::And(pipeline) | ast::AndOr::Or(pipeline) => {
                walk_pipeline(pipeline, depth, cd_steps, out);
            }
        }
    }
}

/// Parsed representation of one simple command before emitting actions.
struct ParsedSimple {
    argv: Vec<String>,
    stdin_docs: Vec<String>,
}

fn walk_pipeline(
    pipeline: &Pipeline,
    depth: usize,
    cd_steps: &mut Vec<String>,
    out: &mut Vec<ShellAction>,
) {
    let in_pipeline = pipeline.seq.len() > 1;
    let mut prev_cat_stdin: Option<String> = None;

    for command in &pipeline.seq {
        // In a multi-stage pipeline each stage runs in its own subshell so a
        // `cd` inside one stage does not alter subsequent commands.
        let mut stage_cd;
        let active_cd = if in_pipeline {
            stage_cd = cd_steps.clone();
            &mut stage_cd
        } else {
            &mut *cd_steps
        };

        match command {
            Command::Simple(simple) => {
                let parsed = walk_simple(simple, depth, active_cd, prev_cat_stdin.take(), out);
                // Detect `cat <<'EOF' | <next>` so piped heredocs reach the next stage.
                if in_pipeline
                    && let Some(parsed) = parsed
                    && parsed.argv.len() == 1
                    && program_basename(&parsed.argv[0]) == "cat"
                    && let Some(doc) = parsed.stdin_docs.into_iter().next()
                {
                    prev_cat_stdin = Some(doc);
                }
            }
            Command::Compound(compound, redirects) => {
                prev_cat_stdin = None;
                walk_compound_command(compound, depth, active_cd, out);
                if let Some(redirects) = redirects {
                    walk_redirect_list(redirects, depth, active_cd, out);
                }
            }
            Command::Function(function) => {
                prev_cat_stdin = None;
                let mut fn_cd = active_cd.clone();
                walk_compound_command(&function.body.0, depth, &mut fn_cd, out);
                if let Some(redirects) = &function.body.1 {
                    walk_redirect_list(redirects, depth, &mut fn_cd, out);
                }
            }
            Command::ExtendedTest(_, redirects) => {
                prev_cat_stdin = None;
                if let Some(redirects) = redirects {
                    walk_redirect_list(redirects, depth, active_cd, out);
                }
            }
        }
    }
}

fn walk_compound_command(
    compound: &CompoundCommand,
    depth: usize,
    cd_steps: &mut Vec<String>,
    out: &mut Vec<ShellAction>,
) {
    match compound {
        CompoundCommand::Arithmetic(_) => {}
        CompoundCommand::ArithmeticForClause(clause) => {
            let mut body_cd = cd_steps.clone();
            walk_compound_list(&clause.body.list, depth, &mut body_cd, out);
        }
        CompoundCommand::BraceGroup(group) => {
            walk_compound_list(&group.list, depth, cd_steps, out);
        }
        CompoundCommand::Subshell(subshell) => {
            let mut sub_cd = cd_steps.clone();
            walk_compound_list(&subshell.list, depth, &mut sub_cd, out);
        }
        CompoundCommand::ForClause(clause) => {
            for value in clause.values.iter().flatten() {
                walk_word_substitutions(&value.value, depth, cd_steps, out);
            }
            let mut body_cd = cd_steps.clone();
            walk_compound_list(&clause.body.list, depth, &mut body_cd, out);
        }
        CompoundCommand::CaseClause(clause) => {
            walk_word_substitutions(&clause.value.value, depth, cd_steps, out);
            for item in &clause.cases {
                if let Some(body) = &item.cmd {
                    let mut arm_cd = cd_steps.clone();
                    walk_compound_list(body, depth, &mut arm_cd, out);
                }
            }
        }
        CompoundCommand::IfClause(clause) => {
            let mut cond_cd = cd_steps.clone();
            walk_compound_list(&clause.condition, depth, &mut cond_cd, out);
            let mut then_cd = cond_cd.clone();
            walk_compound_list(&clause.then, depth, &mut then_cd, out);
            for other in clause.elses.iter().flatten() {
                let mut else_cd = cd_steps.clone();
                if let Some(condition) = &other.condition {
                    walk_compound_list(condition, depth, &mut else_cd, out);
                }
                walk_compound_list(&other.body, depth, &mut else_cd, out);
            }
        }
        CompoundCommand::WhileClause(clause) | CompoundCommand::UntilClause(clause) => {
            let mut loop_cd = cd_steps.clone();
            walk_compound_list(&clause.0, depth, &mut loop_cd, out);
            walk_compound_list(&clause.1.list, depth, &mut loop_cd, out);
        }
        CompoundCommand::Coprocess(coproc) => {
            let mut sub_cd = cd_steps.clone();
            let pipeline = Pipeline {
                bang: false,
                timed: None,
                seq: vec![(*coproc.body).clone()],
            };
            walk_pipeline(&pipeline, depth, &mut sub_cd, out);
        }
    }
}

fn walk_simple(
    simple: &SimpleCommand,
    depth: usize,
    cd_steps: &mut Vec<String>,
    piped_stdin: Option<String>,
    out: &mut Vec<ShellAction>,
) -> Option<ParsedSimple> {
    let mut argv = Vec::new();
    let mut stdin_docs = Vec::new();
    if let Some(doc) = piped_stdin {
        stdin_docs.push(doc);
    }

    if let Some(prefix) = &simple.prefix {
        for item in &prefix.0 {
            if let CommandPrefixOrSuffixItem::AssignmentWord(assignment, _) = item {
                walk_assignment_substitutions(assignment, depth, cd_steps, out);
            } else {
                walk_affix(item, depth, cd_steps, &mut argv, &mut stdin_docs, out);
            }
        }
    }
    if let Some(program) = &simple.word_or_name {
        walk_word_substitutions(&program.value, depth, cd_steps, out);
        argv.push(unquote_word(&program.value));
    }
    if let Some(suffix) = &simple.suffix {
        for item in &suffix.0 {
            walk_affix(item, depth, cd_steps, &mut argv, &mut stdin_docs, out);
        }
    }

    if argv.is_empty() {
        return None;
    }

    let (program, arguments) = unwrap_command(&argv);
    if program == "cd" {
        apply_cd(arguments, cd_steps);
        return Some(ParsedSimple { argv, stdin_docs });
    }

    out.push(ShellAction {
        cd_steps: cd_steps.clone(),
        kind: ShellActionKind::Command {
            program: program.clone(),
            args: arguments.to_vec(),
        },
    });
    for stdin in &stdin_docs {
        out.push(ShellAction {
            cd_steps: cd_steps.clone(),
            kind: ShellActionKind::StdinScript {
                program: program.clone(),
                args: arguments.to_vec(),
                stdin: stdin.clone(),
            },
        });
    }

    // Follow inline nested shell scripts (`sh -c '...'`, `bash <<'EOF'`, `eval '...'`).
    if program == "eval" && !arguments.is_empty() {
        let _ = walk_script(&arguments.join(" "), depth + 1, cd_steps, out);
    } else if is_shell_program(&program) {
        if let Some(inline) = extract_shell_c_script(arguments) {
            let mut sub_cd = cd_steps.clone();
            let _ = walk_script(inline, depth + 1, &mut sub_cd, out);
        } else if !has_shell_script_file(arguments) {
            for stdin in &stdin_docs {
                let mut sub_cd = cd_steps.clone();
                let _ = walk_script(stdin, depth + 1, &mut sub_cd, out);
            }
        }
    }

    Some(ParsedSimple { argv, stdin_docs })
}

fn walk_affix(
    item: &CommandPrefixOrSuffixItem,
    depth: usize,
    cd_steps: &mut Vec<String>,
    argv: &mut Vec<String>,
    stdin_docs: &mut Vec<String>,
    out: &mut Vec<ShellAction>,
) {
    match item {
        CommandPrefixOrSuffixItem::Word(word) => {
            walk_word_substitutions(&word.value, depth, cd_steps, out);
            argv.push(unquote_word(&word.value));
        }
        CommandPrefixOrSuffixItem::AssignmentWord(assignment, word) => {
            walk_assignment_substitutions(assignment, depth, cd_steps, out);
            walk_word_substitutions(&word.value, depth, cd_steps, out);
            argv.push(unquote_word(&word.value));
        }
        CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
            if let Some(doc) = stdin_from_redirect(redirect) {
                stdin_docs.push(doc);
            }
            walk_redirect(redirect, depth, cd_steps, out);
        }
        CommandPrefixOrSuffixItem::ProcessSubstitution(_, subshell) => {
            let mut sub_cd = cd_steps.clone();
            walk_compound_list(&subshell.list, depth + 1, &mut sub_cd, out);
        }
    }
}

fn walk_redirect_list(
    redirects: &RedirectList,
    depth: usize,
    cd_steps: &mut Vec<String>,
    out: &mut Vec<ShellAction>,
) {
    for redirect in &redirects.0 {
        walk_redirect(redirect, depth, cd_steps, out);
    }
}

fn walk_redirect(
    redirect: &IoRedirect,
    depth: usize,
    cd_steps: &mut Vec<String>,
    out: &mut Vec<ShellAction>,
) {
    match redirect {
        IoRedirect::File(_, kind, target) => match target {
            IoFileRedirectTarget::Filename(word) => {
                walk_word_substitutions(&word.value, depth, cd_steps, out);
                if matches!(
                    kind,
                    IoFileRedirectKind::Write
                        | IoFileRedirectKind::Clobber
                        | IoFileRedirectKind::Append
                        | IoFileRedirectKind::ReadAndWrite
                ) {
                    let target = unquote_word(&word.value);
                    if !target.is_empty() {
                        out.push(ShellAction {
                            cd_steps: cd_steps.clone(),
                            kind: ShellActionKind::OutputRedirect { target },
                        });
                    }
                }
            }
            IoFileRedirectTarget::Duplicate(word) => {
                walk_word_substitutions(&word.value, depth, cd_steps, out);
            }
            IoFileRedirectTarget::Fd(_) => {}
            IoFileRedirectTarget::ProcessSubstitution(_, subshell) => {
                let mut sub_cd = cd_steps.clone();
                walk_compound_list(&subshell.list, depth + 1, &mut sub_cd, out);
            }
        },
        IoRedirect::OutputAndError(word, _) => {
            walk_word_substitutions(&word.value, depth, cd_steps, out);
            let target = unquote_word(&word.value);
            if !target.is_empty() {
                out.push(ShellAction {
                    cd_steps: cd_steps.clone(),
                    kind: ShellActionKind::OutputRedirect { target },
                });
            }
        }
        IoRedirect::HereDocument(_, doc) => {
            if doc.requires_expansion {
                walk_word_substitutions(&doc.doc.value, depth, cd_steps, out);
            }
        }
        IoRedirect::HereString(_, word) => {
            walk_word_substitutions(&word.value, depth, cd_steps, out);
        }
    }
}

fn stdin_from_redirect(redirect: &IoRedirect) -> Option<String> {
    match redirect {
        IoRedirect::HereDocument(
            fd,
            IoHereDocument {
                remove_tabs, doc, ..
            },
        ) if fd.is_none() || *fd == Some(0) => {
            if *remove_tabs {
                let mut stripped = String::with_capacity(doc.value.len());
                for line in doc.value.split_inclusive('\n') {
                    stripped.push_str(line.trim_start_matches('\t'));
                }
                Some(stripped)
            } else {
                Some(doc.value.clone())
            }
        }
        IoRedirect::HereString(fd, word) if fd.is_none() || *fd == Some(0) => {
            Some(unquote_word(&word.value))
        }
        _ => None,
    }
}

fn walk_assignment_substitutions(
    assignment: &ast::Assignment,
    depth: usize,
    cd_steps: &mut Vec<String>,
    out: &mut Vec<ShellAction>,
) {
    match &assignment.value {
        ast::AssignmentValue::Scalar(word) => {
            walk_word_substitutions(&word.value, depth, cd_steps, out);
        }
        ast::AssignmentValue::Array(entries) => {
            for (key, value) in entries {
                if let Some(key) = key {
                    walk_word_substitutions(&key.value, depth, cd_steps, out);
                }
                walk_word_substitutions(&value.value, depth, cd_steps, out);
            }
        }
    }
}

fn walk_word_substitutions(
    source: &str,
    depth: usize,
    cd_steps: &mut Vec<String>,
    out: &mut Vec<ShellAction>,
) {
    let Ok(pieces) = brush_parser::word::parse(source, &ParserOptions::default()) else {
        return;
    };
    walk_pieces_substitutions(&pieces, depth, cd_steps, out);
}

fn walk_pieces_substitutions(
    pieces: &[WordPieceWithSource],
    depth: usize,
    cd_steps: &mut Vec<String>,
    out: &mut Vec<ShellAction>,
) {
    for WordPieceWithSource { piece, .. } in pieces {
        match piece {
            WordPiece::DoubleQuotedSequence(inner)
            | WordPiece::GettextDoubleQuotedSequence(inner) => {
                walk_pieces_substitutions(inner, depth, cd_steps, out);
            }
            WordPiece::CommandSubstitution(inner)
            | WordPiece::BackquotedCommandSubstitution(inner) => {
                let mut sub_cd = cd_steps.clone();
                let _ = walk_script(inner, depth + 1, &mut sub_cd, out);
            }
            _ => {}
        }
    }
}

/// Removes shell quoting while keeping variable/tilde syntax text intact so
/// callers can recognize `$TMPDIR/...` or `$MAY_TASKS` and avoid misreading
/// dynamic paths as literal relative paths.
fn unquote_word(raw: &str) -> String {
    let Ok(pieces) = brush_parser::word::parse(raw, &ParserOptions::default()) else {
        return raw.trim_matches('"').trim_matches('\'').to_string();
    };
    let mut out = String::new();
    append_unquoted_pieces(raw, &pieces, &mut out);
    out
}

fn append_unquoted_pieces(source: &str, pieces: &[WordPieceWithSource], out: &mut String) {
    for piece_with_src in pieces {
        match &piece_with_src.piece {
            WordPiece::Text(part)
            | WordPiece::SingleQuotedText(part)
            | WordPiece::AnsiCQuotedText(part) => out.push_str(part),
            WordPiece::EscapeSequence(part) => {
                out.push_str(part.strip_prefix('\\').unwrap_or(part));
            }
            WordPiece::DoubleQuotedSequence(inner)
            | WordPiece::GettextDoubleQuotedSequence(inner) => {
                append_unquoted_pieces(source, inner, out);
            }
            _ => {
                if let Some(slice) =
                    source.get(piece_with_src.start_index..piece_with_src.end_index)
                {
                    out.push_str(slice);
                }
            }
        }
    }
}

fn apply_cd(args: &[String], cd_steps: &mut Vec<String>) {
    let mut after_double_dash = false;
    let mut target: Option<&str> = None;
    for arg in args {
        if !after_double_dash && arg == "--" {
            after_double_dash = true;
            continue;
        }
        if !after_double_dash && arg.starts_with('-') && arg != "-" {
            continue;
        }
        target = Some(arg.as_str());
        break;
    }
    if let Some(dir) = target
        && dir != "-"
        && !dir.is_empty()
        && !dir.contains('$')
    {
        if dir.starts_with('/') {
            cd_steps.clear();
        }
        cd_steps.push(dir.to_string());
    }
}

/// Strips transparent execution wrappers (`env`, `time`, `command`, `nohup`,
/// `nice`, `sudo`, `timeout`, `exec`) to obtain `(program_basename, args)`.
fn unwrap_command(argv: &[String]) -> (String, &[String]) {
    let mut current = argv;
    for _ in 0..8 {
        let Some((head, tail)) = current.split_first() else {
            break;
        };
        let name = program_basename(head);
        let next = match name.as_str() {
            "env" => unwrap_wrapper(tail, "uC", &["unset", "chdir"], 0, true),
            "sudo" => unwrap_wrapper(
                tail,
                "ughpCDRTU",
                &[
                    "user",
                    "group",
                    "host",
                    "prompt",
                    "chdir",
                    "close-from",
                    "other-user",
                ],
                0,
                true,
            ),
            "timeout" => unwrap_wrapper(tail, "ks", &["kill-after", "signal"], 1, false),
            "nice" => unwrap_wrapper(tail, "n", &["adjustment"], 0, false),
            "command" => {
                // `command -v` / `-V` only queries PATH without executing.
                if tail.iter().any(|arg| arg == "-v" || arg == "-V") {
                    return (name, tail);
                }
                unwrap_wrapper(tail, "", &[], 0, false)
            }
            "time" | "nohup" | "exec" => unwrap_wrapper(tail, "", &[], 0, false),
            _ => return (name, tail),
        };
        match next {
            Some(inner) if !inner.is_empty() => current = inner,
            _ => return (name, tail),
        }
    }
    match current.split_first() {
        Some((head, tail)) => (program_basename(head), tail),
        None => (String::new(), &[]),
    }
}

fn unwrap_wrapper<'a>(
    args: &'a [String],
    short_values: &str,
    long_values: &[&str],
    mut positionals_to_skip: usize,
    skip_assignments: bool,
) -> Option<&'a [String]> {
    let mut i = 0;
    let mut end_opts = false;
    while i < args.len() {
        let arg = &args[i];
        if !end_opts && arg == "--" {
            end_opts = true;
            i += 1;
            continue;
        }
        if !end_opts && arg.starts_with("--") {
            let name = arg.trim_start_matches('-');
            if let Some((flag, _)) = name.split_once('=') {
                let _ = flag;
                i += 1;
            } else if long_values.contains(&name) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if !end_opts && arg.starts_with('-') && arg.len() > 1 {
            let last = arg.chars().last().unwrap_or_default();
            if short_values.contains(last) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if skip_assignments && arg.contains('=') && !arg.starts_with('=') {
            i += 1;
            continue;
        }
        if positionals_to_skip > 0 {
            positionals_to_skip -= 1;
            i += 1;
            continue;
        }
        return Some(&args[i..]);
    }
    None
}

fn program_basename(program: &str) -> String {
    let base = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let lower = base.to_ascii_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

fn is_shell_program(name: &str) -> bool {
    matches!(name, "sh" | "bash" | "dash" | "ash" | "ksh" | "zsh")
}

fn extract_shell_c_script(args: &[String]) -> Option<&str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--command" {
            return iter.next().map(String::as_str);
        }
        if let Some(flags) = arg
            .strip_prefix('-')
            .filter(|rest| !rest.is_empty() && !rest.starts_with('-'))
        {
            if flags.contains('c') {
                return iter.next().map(String::as_str);
            }
            if flags.ends_with(['o', 'O']) {
                let _ = iter.next();
            }
        } else {
            break;
        }
    }
    None
}

fn has_shell_script_file(args: &[String]) -> bool {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            return iter.next().is_some();
        }
        if let Some(flags) = arg
            .strip_prefix(['-', '+'])
            .filter(|rest| !rest.is_empty() && !rest.starts_with('-'))
        {
            if flags.contains('s') {
                return false;
            }
            if flags.ends_with(['o', 'O']) {
                let _ = iter.next();
            }
        } else if arg != "-" {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heredoc_body_of_edit_never_produces_output_redirects() {
        let script = "edit <<'PATCH'\n[src/main.rs]\nPUT >5#PM:\n+fn f() -> i32 { a > b }\nPATCH\n";
        let actions = inspect_shell_actions(script).expect("parses");
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action.kind, ShellActionKind::OutputRedirect { .. })),
            "heredoc body must not produce output redirects: {actions:?}"
        );
    }

    #[test]
    fn tracks_cd_for_python_heredoc_and_redirections() {
        let script = "cd repo && python3 - <<'EOF'\nfrom pathlib import Path\nPath('src/lib.rs').write_text('hi')\nEOF\n";
        let actions = inspect_shell_actions(script).expect("parses");
        let stdin_action = actions
            .iter()
            .find(|action| matches!(action.kind, ShellActionKind::StdinScript { .. }))
            .expect("stdin script recorded");
        assert_eq!(stdin_action.cd_steps, vec!["repo"]);
        match &stdin_action.kind {
            ShellActionKind::StdinScript {
                program,
                args,
                stdin,
            } => {
                assert_eq!(program, "python3");
                assert_eq!(args, &vec!["-".to_string()]);
                assert!(stdin.contains("write_text"), "{stdin}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn isolates_cd_inside_subshell_and_skips_fd_duplication() {
        let script = "(cd sub && echo hi > existing.py) && echo hi > root.py 2>&1";
        let actions = inspect_shell_actions(script).expect("parses");
        let redirects: Vec<(&[String], &str)> = actions
            .iter()
            .filter_map(|action| match &action.kind {
                ShellActionKind::OutputRedirect { target } => {
                    Some((action.cd_steps.as_slice(), target.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            redirects,
            vec![
                (&["sub".to_string()][..], "existing.py"),
                (&[][..], "root.py"),
            ]
        );
    }
}
