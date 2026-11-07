//! Walks the parsed script and turns each command into a step.

use brush_parser::ast::{
    AndOrList, Command, CommandPrefixOrSuffixItem, IoFileRedirectKind, IoFileRedirectTarget,
    IoHereDocument, IoRedirect, Pipeline, PipelineOperator, SeparatorOperator, SimpleCommand,
};
use brush_parser::word::{WordPiece, WordPieceWithSource};
use brush_parser::{Parser, ParserOptions, SourceInfo};

use super::{CommandReading, Intent, Link, Step, patch, recognise};

/// A command word as May can know it before the script runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Arg {
    /// Fixed text once quoting is removed.
    Literal(String),
    /// Unquoted glob or brace characters: the shell expands it to paths May
    /// cannot see.
    Pattern,
    /// A variable, substitution or anything else computed at run time.
    Dynamic,
}

impl Arg {
    pub(super) fn literal(&self) -> Option<&str> {
        match self {
            Self::Literal(text) => Some(text),
            Self::Pattern | Self::Dynamic => None,
        }
    }
}

/// A simple command with its words resolved.
struct Simple {
    name: String,
    args: Vec<Arg>,
    /// Text fed to standard input by a here-document or here-string.
    stdin: Option<String>,
}

pub(super) fn read(script: &str) -> CommandReading {
    let parsed = Parser::new(
        script.as_bytes(),
        &ParserOptions::default(),
        &SourceInfo::default(),
    )
    .parse_program();
    let Ok(program) = parsed else {
        return CommandReading {
            steps: vec![unknown(Link::Start, script.trim().to_owned())],
        };
    };
    let mut steps: Vec<Step> = Vec::new();
    for list in &program.complete_commands {
        for item in &list.0 {
            let link = if steps.is_empty() {
                Link::Start
            } else {
                Link::Then
            };
            if matches!(item.1, SeparatorOperator::Async) {
                steps.push(unknown(link, item.0.to_string()));
            } else {
                and_or(&item.0, link, &mut steps);
            }
        }
    }
    CommandReading { steps }
}

fn unknown(link: Link, source: String) -> Step {
    Step::new(link, Intent::Unknown { source })
}

fn and_or(list: &AndOrList, first: Link, steps: &mut Vec<Step>) {
    for (index, (operator, pipeline)) in list.iter().enumerate() {
        let link = match (index, operator) {
            (0, _) => first,
            (_, PipelineOperator::And) => Link::And,
            (_, PipelineOperator::Or) => Link::Or,
        };
        self::pipeline(pipeline, link, steps);
    }
}

fn pipeline(pipeline: &Pipeline, first: Link, steps: &mut Vec<Step>) {
    if pipeline.bang || pipeline.timed.is_some() {
        steps.push(unknown(first, pipeline.to_string()));
        return;
    }
    let stages: Vec<Option<Simple>> = pipeline
        .seq
        .iter()
        .map(|command| match command {
            Command::Simple(simple) => simple_command(simple),
            _ => None,
        })
        .collect();
    let mut index = 0;
    while index < pipeline.seq.len() {
        let link = if index == 0 { first } else { Link::Pipe };
        // A patch piped in: `cat <<'EOF' | apply_patch`.
        if let (Some(Some(feed)), Some(Some(sink))) = (stages.get(index), stages.get(index + 1))
            && feed.name == "cat"
            && feed.args.is_empty()
            && sink.name == "apply_patch"
            && sink.args.is_empty()
            && sink.stdin.is_none()
            && let Some(text) = &feed.stdin
        {
            let intent = patch::parse(text).map_or_else(
                || Intent::Unknown {
                    source: format!("{} | {}", pipeline.seq[index], pipeline.seq[index + 1]),
                },
                |files| Intent::Edit { files },
            );
            steps.push(Step::new(link, intent));
            index += 2;
            continue;
        }
        let intents = stages[index].as_ref().and_then(|simple| {
            recognise::recognise(&simple.name, &simple.args, simple.stdin.as_deref())
        });
        match intents {
            Some(intents) => {
                for (offset, intent) in intents.into_iter().enumerate() {
                    let link = if offset == 0 { link } else { Link::Then };
                    steps.push(Step::new(link, intent));
                }
            }
            None => steps.push(unknown(link, pipeline.seq[index].to_string())),
        }
        index += 1;
    }
}

/// Resolves a simple command, or `None` when something about it (an
/// assignment, a redirect to a file, a process substitution) is beyond
/// what a recognised command may carry.
fn simple_command(command: &SimpleCommand) -> Option<Simple> {
    let name = match resolve(&command.word_or_name.as_ref()?.value) {
        Arg::Literal(name) => name,
        Arg::Pattern | Arg::Dynamic => return None,
    };
    let mut args = Vec::new();
    let mut stdin = None;
    let items = command
        .prefix
        .iter()
        .flat_map(|prefix| prefix.0.iter())
        .chain(command.suffix.iter().flat_map(|suffix| suffix.0.iter()));
    for item in items {
        match item {
            CommandPrefixOrSuffixItem::Word(word) => args.push(resolve(&word.value)),
            CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                if let Redirect::Stdin(text) = redirect_effect(redirect)?
                    && stdin.replace(text).is_some()
                {
                    return None;
                }
            }
            CommandPrefixOrSuffixItem::AssignmentWord(..)
            | CommandPrefixOrSuffixItem::ProcessSubstitution(..) => return None,
        }
    }
    Some(Simple { name, args, stdin })
}

/// What a redirect does that May reads.
enum Redirect {
    /// It feeds this text to standard input.
    Stdin(String),
    /// It only routes output away from the terminal.
    Ignored,
}

/// The effect of a redirect, or `None` for one May cannot read.
fn redirect_effect(redirect: &IoRedirect) -> Option<Redirect> {
    match redirect {
        IoRedirect::HereDocument(
            _,
            IoHereDocument {
                requires_expansion,
                doc,
                ..
            },
        ) => {
            if *requires_expansion && doc.value.contains(['$', '`', '\\']) {
                return None;
            }
            Some(Redirect::Stdin(doc.value.clone()))
        }
        IoRedirect::HereString(_, word) => match resolve(&word.value) {
            Arg::Literal(text) => Some(Redirect::Stdin(text)),
            Arg::Pattern | Arg::Dynamic => None,
        },
        // `2>&1` and `>/dev/null` only route output away from the terminal.
        IoRedirect::File(
            _,
            IoFileRedirectKind::DuplicateOutput,
            IoFileRedirectTarget::Duplicate(_) | IoFileRedirectTarget::Fd(_),
        ) => Some(Redirect::Ignored),
        IoRedirect::File(
            _,
            IoFileRedirectKind::Write | IoFileRedirectKind::Append,
            IoFileRedirectTarget::Filename(word),
        ) if word.value == "/dev/null" => Some(Redirect::Ignored),
        IoRedirect::File(..) | IoRedirect::OutputAndError(..) => None,
    }
}

/// Removes the quoting from a word; anything the shell computes or expands
/// is not a literal.
fn resolve(word: &str) -> Arg {
    let Ok(pieces) = brush_parser::word::parse(word, &ParserOptions::default()) else {
        return Arg::Dynamic;
    };
    let mut text = String::new();
    let mut pattern = false;
    match pieces_text(&pieces, &mut text, &mut pattern) {
        Some(()) if pattern => Arg::Pattern,
        Some(()) => Arg::Literal(text),
        None => Arg::Dynamic,
    }
}

fn pieces_text(
    pieces: &[WordPieceWithSource],
    text: &mut String,
    pattern: &mut bool,
) -> Option<()> {
    for WordPieceWithSource { piece, .. } in pieces {
        match piece {
            WordPiece::Text(part) => {
                *pattern |= part.contains(['*', '?', '[', '{']);
                text.push_str(part);
            }
            WordPiece::SingleQuotedText(part) => text.push_str(part),
            WordPiece::EscapeSequence(part) => text.push_str(part.strip_prefix('\\')?),
            WordPiece::DoubleQuotedSequence(inner) => {
                // Inside double quotes a glob stays text.
                let mut quoted_pattern = false;
                pieces_text(inner, text, &mut quoted_pattern)?;
            }
            WordPiece::GettextDoubleQuotedSequence(_)
            | WordPiece::AnsiCQuotedText(_)
            | WordPiece::TildePrefix(_)
            | WordPiece::ParameterExpansion(_)
            | WordPiece::CommandSubstitution(_)
            | WordPiece::BackquotedCommandSubstitution(_)
            | WordPiece::ArithmeticExpression(_) => return None,
        }
    }
    Some(())
}
