//! May's rules on what a terminal script may run, as aither script policies.
//!
//! Every script of a shell built with a [`PolicySet`] is put to them before it
//! runs: one that kills processes by name or pattern, or is not valid shell
//! and so cannot be checked for that; a foreground `sleep`; a script that
//! only repeats a read of files unchanged since; an `apply_patch` hunk that
//! only changes whitespace where it carries no meaning; and, in the managed
//! bash, the host `grep`, `awk` and
//! `find` and the `sed` forms `aither_sed::supports` does not accept. Each
//! refusal names what to do instead. None of them approves a script that
//! leaves the sandbox: each [passes](ScriptDecision::Pass) one it has no
//! objection to.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use aither_core::llm::tool::CallIdentity;
use aither_sandbox::{
    ScriptCheck, ScriptDecision, ScriptEnd, ScriptOrigin, ScriptOutcome, ScriptPolicy,
    ScriptRefusal, ShellBuilder, ShellCommand, ShellScript,
};
use askama::Template;
use syntect::parsing::{ParseState, Scope, ScopeStack, ScopeStackOp, SyntaxReference, SyntaxSet};

use crate::command_reading::{
    CommandReading, DiffLine, DiffOp, FileChange, Hunk, Intent, Link, SearchTool,
};

/// Whether the scripts of a session run in the managed bash: always inside a
/// container, and on the host everywhere but Windows, where `cmd.exe` runs
/// them with the host's own programs.
#[must_use]
pub const fn runs_managed_bash(in_container: bool) -> bool {
    in_container || cfg!(not(windows))
}

/// The calls the unchanged-read policy refused, for the agent's turn
/// boundary to tell a turn made only of repeated reads from one that
/// learned something. Every clone shares the record.
#[derive(Debug, Clone, Default)]
pub struct RefusedReads(Arc<Mutex<HashSet<CallIdentity>>>);

impl RefusedReads {
    fn record(&self, call: CallIdentity) {
        self.0
            .lock()
            .expect("the refused read record is never held across a panic")
            .insert(call);
    }

    /// Whether `call` was refused as an unchanged re-read; each call is
    /// answered once.
    ///
    /// # Panics
    /// Panics when the record's lock is poisoned.
    #[must_use]
    pub fn take(&self, call: &CallIdentity) -> bool {
        self.0
            .lock()
            .expect("the refused read record is never held across a panic")
            .remove(call)
    }
}

/// What May's policies for the shell of one agent are chosen by.
#[derive(Debug, Clone, Default)]
pub struct PolicySet {
    /// [`runs_managed_bash`] of the session.
    pub managed_bash: bool,
    /// The record the agent's turn boundary reads.
    pub refused: RefusedReads,
}

impl PolicySet {
    /// Puts every policy of the set to `builder`'s scripts.
    #[must_use]
    pub fn apply<I, L, O, W, T>(
        self,
        builder: ShellBuilder<I, L, O, W, T>,
    ) -> ShellBuilder<I, L, O, W, T> {
        let builder = builder
            .policy(KillByName)
            .policy(ForegroundSleep)
            .policy(UnchangedFileReads {
                refused: self.refused,
            })
            .policy(PythonWithUv)
            .policy(WhitespacePatch);
        if self.managed_bash {
            builder.policy(ManagedShellCommands)
        } else {
            builder
        }
    }
}

fn refuse(template: &impl Template) -> ScriptDecision {
    ScriptDecision::Refuse(ScriptRefusal::new(
        template
            .render()
            .expect("a script policy template renders typed input"),
    ))
}

/// The script parsed as shell; `None` for a script in another language or
/// one that is not valid shell.
const fn parsed<'a>(script: &ScriptCheck<'a>) -> Option<&'a ShellScript> {
    match script.shell {
        Some(Ok(shell)) => Some(shell),
        Some(Err(_)) | None => None,
    }
}

#[derive(Template)]
#[template(path = "refused_kill_by_name.txt")]
struct KillByNameRefused<'a> {
    program: &'a str,
}

#[derive(Template)]
#[template(path = "refused_unparsable_command.txt")]
struct UnparsableRefused<'a> {
    reason: &'a str,
}

/// Refuses a script that signals processes selected by name or pattern,
/// which may hit processes the session did not start, and a shell script
/// that does not parse, which cannot be checked for that.
#[derive(Debug, Clone, Copy, Default)]
pub struct KillByName;

/// Programs that signal processes selected by name, user or other pattern.
const KILLERS: &[&str] = &["pkill", "killall", "killall5", "skill"];

/// The program that kills by name or pattern in `script`, if any.
fn kill_by_name(script: &ShellScript) -> Option<String> {
    for command in script.commands() {
        let arguments = command.arguments();
        let by_name = match command.program() {
            "fuser" => arguments.iter().any(|argument| {
                argument == "--kill"
                    || argument
                        .strip_prefix('-')
                        .is_some_and(|flags| !flags.starts_with('-') && flags.contains('k'))
            }),
            "taskkill" => arguments.iter().any(|argument| {
                let selector = argument.trim_start_matches(['/', '-']).to_ascii_lowercase();
                argument.starts_with(['/', '-']) && matches!(selector.as_str(), "im" | "fi")
            }),
            name => KILLERS.contains(&name),
        };
        if by_name {
            return Some(command.program().to_string());
        }
    }
    let lookups = script
        .commands()
        .iter()
        .filter(|command| matches!(command.program(), "pgrep" | "pidof"));
    for lookup in lookups {
        // `kill $(pgrep NAME)`.
        let substituted_into_kill = lookup
            .substituted_into()
            .is_some_and(|line| script.line(line).any(|command| command.program() == "kill"));
        // `pgrep NAME | xargs kill`.
        let piped_into_kill = lookup.pipeline().is_some_and(|producer| {
            script.commands().iter().any(|command| {
                command.program() == "kill"
                    && command.pipeline().is_some_and(|consumer| {
                        consumer.pipeline == producer.pipeline && consumer.stage > producer.stage
                    })
                    && script
                        .wrappers(command)
                        .any(|wrapper| wrapper.program() == "xargs")
            })
        });
        if substituted_into_kill || piped_into_kill {
            return Some("kill of pgrep/pidof output".to_string());
        }
    }
    None
}

impl ScriptPolicy for KillByName {
    type State = ();

    fn check(
        &self,
        script: &ScriptCheck<'_>,
        (): &(),
    ) -> impl Future<Output = ScriptDecision> + Send {
        let decision = match script.shell {
            None => ScriptDecision::Pass,
            Some(Err(error)) => refuse(&UnparsableRefused {
                reason: error.reason(),
            }),
            Some(Ok(shell)) => kill_by_name(shell).map_or(ScriptDecision::Pass, |program| {
                refuse(&KillByNameRefused { program: &program })
            }),
        };
        std::future::ready(decision)
    }
}

#[derive(Template)]
#[template(path = "refused_sleep.txt")]
struct SleepRefused;

/// Refuses a foreground `terminal` script that runs `sleep` where the
/// script waits on it: it only serves polling. A detached script
/// (`background: true`) may sleep, as a timer or while it waits on a
/// condition.
#[derive(Debug, Clone, Copy, Default)]
pub struct ForegroundSleep;

impl ScriptPolicy for ForegroundSleep {
    type State = ();

    fn check(
        &self,
        script: &ScriptCheck<'_>,
        (): &(),
    ) -> impl Future<Output = ScriptDecision> + Send {
        let sleeps = script.origin == ScriptOrigin::Foreground
            && parsed(script).is_some_and(|shell| {
                shell
                    .commands()
                    .iter()
                    .any(|command| !command.is_background() && command.program() == "sleep")
            });
        std::future::ready(if sleeps {
            refuse(&SleepRefused)
        } else {
            ScriptDecision::Pass
        })
    }
}

/// The files `reading` reads when the script does nothing but read: every
/// step a recognised file read, or an `rg` search of the output piped into
/// it. `None` for a script with any other step — a `cd`, a program that could
/// write, a redirect to a file, a wrapper, a compound command or a word
/// computed at run time all read as unknown steps — and for one that reads
/// no file.
fn pure_read(reading: &CommandReading) -> Option<Vec<&str>> {
    let mut files = Vec::new();
    for step in &reading.steps {
        match &step.intent {
            Intent::Read { path, .. } => files.push(path.as_str()),
            // `rg` without a path searches its standard input.
            Intent::Search {
                tool: SearchTool::Rg,
                pattern: Some(_),
                paths,
            } if step.link == Link::Pipe && paths.is_empty() => {}
            _ => return None,
        }
    }
    (!files.is_empty()).then_some(files)
}

/// What a read shows besides the files' content: the directory its relative
/// paths resolve against, how each step follows the one before, and every
/// command's words — the reader, its arguments and what its output is piped
/// into. Two reads with the same identity of files with the same stamps
/// print the same output.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReadIdentity {
    dir: PathBuf,
    links: Vec<Link>,
    commands: Vec<Vec<String>>,
}

/// A file's `(len, modified)` stamp as a read found it: two reads whose
/// stamps match return the same bytes.
type Fingerprint = (u64, Option<SystemTime>);

/// What one agent's file reads returned with.
#[derive(Debug, Default)]
pub struct FileReads {
    inner: Mutex<FileReadsInner>,
}

#[derive(Debug, Default)]
struct FileReadsInner {
    /// Read → the stamps of its files when it last succeeded.
    recorded: HashMap<ReadIdentity, Vec<(PathBuf, Fingerprint)>>,
    /// Task id → the read its script was admitted as, with its stamps.
    pending: HashMap<String, (ReadIdentity, Vec<(PathBuf, Fingerprint)>)>,
}

impl FileReads {
    fn inner(&self) -> std::sync::MutexGuard<'_, FileReadsInner> {
        self.inner
            .lock()
            .expect("the file read record is never held across a panic")
    }
}

#[derive(Template)]
#[template(path = "refused_unchanged_file_read.txt")]
struct UnchangedFileReadRefused<'a> {
    paths: &'a [&'a Path],
}

/// Refuses a `terminal` script that does nothing but repeat a read that
/// last succeeded with the same files unchanged: the same commands with the
/// same words, from the same directory. It would print what the last one
/// did, spending a turn for nothing; only a background event can say
/// something new.
#[derive(Debug, Clone)]
pub struct UnchangedFileReads {
    refused: RefusedReads,
}

/// The stamp of every file `reads` names, relative paths resolved against
/// `working_dir`, the directory the script starts in; `None` when one is not
/// an existing regular file.
async fn fingerprints(working_dir: &Path, reads: &[&str]) -> Option<Vec<(PathBuf, Fingerprint)>> {
    let stamps = futures_util::future::join_all(reads.iter().map(|read| async move {
        let path = working_dir.join(read);
        match async_fs::metadata(&path).await {
            Ok(meta) if meta.is_file() => Some((path, (meta.len(), meta.modified().ok()))),
            Ok(_) | Err(_) => None,
        }
    }))
    .await;
    stamps.into_iter().collect()
}

impl ScriptPolicy for UnchangedFileReads {
    type State = FileReads;

    async fn check(&self, script: &ScriptCheck<'_>, state: &FileReads) -> ScriptDecision {
        if script.origin == ScriptOrigin::Run {
            return ScriptDecision::Pass;
        }
        let Some(shell) = parsed(script) else {
            return ScriptDecision::Pass;
        };
        // A script on a server reads files this host cannot stamp.
        let Some(start) = script.start.host() else {
            return ScriptDecision::Pass;
        };
        let reading = CommandReading::of(script.script);
        let Some(files) = pure_read(&reading) else {
            return ScriptDecision::Pass;
        };
        let Some(stamps) = fingerprints(start, &files).await else {
            return ScriptDecision::Pass;
        };
        let identity = ReadIdentity {
            dir: start.to_path_buf(),
            links: reading.steps.iter().map(|step| step.link).collect(),
            commands: shell
                .commands()
                .iter()
                .map(|command| command.argv().to_vec())
                .collect(),
        };
        let unchanged = state.inner().recorded.get(&identity) == Some(&stamps);
        if unchanged {
            let paths: Vec<&Path> = stamps.iter().map(|(path, _)| path.as_path()).collect();
            if let Some(call) = script.call {
                self.refused.record(call.clone());
            }
            return refuse(&UnchangedFileReadRefused { paths: &paths });
        }
        state
            .inner()
            .pending
            .insert(script.task_id.to_string(), (identity, stamps));
        ScriptDecision::Pass
    }

    fn ended(&self, end: &ScriptEnd<'_>, state: &FileReads) -> impl Future<Output = ()> + Send {
        let read = state.inner().pending.remove(end.task_id);
        // Only a read that succeeded counts: a denied or failed read must
        // run again and show its failure.
        if let Some((identity, stamps)) = read
            && end.outcome == (ScriptOutcome::Exited { code: 0 })
        {
            state.inner().recorded.insert(identity, stamps);
        }
        std::future::ready(())
    }
}

#[derive(Template)]
#[template(path = "refused_python_setup.txt")]
struct PythonSetupRefused<'a> {
    command: &'a str,
    replacement: &'a str,
}

/// Refuses a script that installs Python packages with pip or creates a
/// virtual environment with `venv` or `virtualenv`: uv does both, and the
/// managed interpreter refuses a bare `pip install` anyway.
#[derive(Debug, Clone, Copy, Default)]
pub struct PythonWithUv;

const PIP_INSTALL_WITH_UV: &str =
    "`uv add <package>` in a project or `uv pip install <package>` in an environment";
const VENV_WITH_UV: &str = "`uv venv`";

/// Whether `version` is empty or a version suffix such as `3` or `3.13`.
fn is_version_suffix(version: &str) -> bool {
    version
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
}

/// Whether `program` is `pip`, `pip3` or a versioned `pip3.13`.
fn is_pip(program: &str) -> bool {
    program.strip_prefix("pip").is_some_and(is_version_suffix)
}

/// Whether `program` is `python`, `python3` or a versioned `python3.13`.
fn is_python(program: &str) -> bool {
    program
        .strip_prefix("python")
        .is_some_and(is_version_suffix)
}

/// The first word of `arguments` that is not an option.
fn subcommand(arguments: &[String]) -> Option<&str> {
    arguments
        .iter()
        .map(String::as_str)
        .find(|argument| !argument.starts_with('-'))
}

/// The refused form `command` is, with the uv command that replaces it.
fn python_setup(command: &ShellCommand) -> Option<(String, &'static str)> {
    let program = command.program();
    let arguments = command.arguments();
    if is_pip(program) {
        return (subcommand(arguments) == Some("install"))
            .then(|| (format!("{program} install"), PIP_INSTALL_WITH_UV));
    }
    if program == "virtualenv" {
        return Some((program.to_string(), VENV_WITH_UV));
    }
    if !is_python(program) {
        return None;
    }
    let module = arguments.iter().position(|argument| argument == "-m")? + 1;
    let module_name = arguments.get(module)?.as_str();
    match module_name {
        "pip" => (subcommand(&arguments[module + 1..]) == Some("install"))
            .then(|| (format!("{program} -m pip install"), PIP_INSTALL_WITH_UV)),
        "venv" | "virtualenv" => Some((format!("{program} -m {module_name}"), VENV_WITH_UV)),
        _ => None,
    }
}

impl ScriptPolicy for PythonWithUv {
    type State = ();

    fn check(
        &self,
        script: &ScriptCheck<'_>,
        (): &(),
    ) -> impl Future<Output = ScriptDecision> + Send {
        let decision = parsed(script)
            .and_then(|shell| shell.commands().iter().find_map(python_setup))
            .map_or(ScriptDecision::Pass, |(command, replacement)| {
                refuse(&PythonSetupRefused {
                    command: &command,
                    replacement,
                })
            });
        std::future::ready(decision)
    }
}

#[derive(Template)]
#[template(path = "refused_whitespace_patch.txt")]
struct WhitespacePatchRefused<'a> {
    path: &'a str,
}

/// Refuses a script whose `apply_patch` patch has a hunk that removes and
/// adds the same code apart from whitespace that carries no meaning in the
/// file's language: formatting by hand, which the project's formatter does.
///
/// Whitespace inside a string literal is meaning, and so are each line's
/// indentation and the line breaks in an [indentation-sensitive
/// language](INDENTATION_SENSITIVE); between two word characters a run of
/// whitespace keeps them two words. A hunk is lexed where it applies: the
/// file is lexed from its top down to the hunk, so a string opened above the
/// hunk is still open in it. A file of a language syntect does not lex, a
/// file that cannot be read and a hunk that does not place in its file are
/// never refused: there is nothing to judge, and a hunk that does not place
/// fails on its own.
#[derive(Debug, Clone, Copy, Default)]
pub struct WhitespacePatch;

/// syntect's bundled syntaxes, loaded the first time a hunk needs them.
#[derive(Debug, Clone, Default)]
pub struct Syntaxes(Arc<OnceLock<SyntaxSet>>);

/// The syntect syntaxes of languages whose indentation and line breaks carry
/// meaning: the off-side rule (Python, Haskell, YAML, Haml and Sass), a
/// Makefile's tab-led recipes, Markdown's and reStructuredText's nested
/// blocks, a diff's line markers.
const INDENTATION_SENSITIVE: &[&str] = &[
    "Python",
    "YAML",
    "Haskell",
    "Literate Haskell",
    "Ruby Haml",
    "Makefile",
    "Markdown",
    "reStructuredText",
    "Diff",
];

/// What a side of a hunk means, once whitespace that carries none is gone.
#[derive(Debug, PartialEq, Eq)]
enum Meaning {
    /// A character of code, or any character of a string literal.
    Char(char),
    /// Whitespace that keeps two word characters apart.
    Gap,
    /// A line break followed by this indentation, in an
    /// indentation-sensitive language.
    Line(String),
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The syntax of the language `path` is written in, matched on its file
/// name and then its extension; `None` for one syntect does not lex.
fn language<'s>(syntaxes: &'s SyntaxSet, path: &str) -> Option<&'s SyntaxReference> {
    let path = Path::new(path);
    let by = |part: Option<&std::ffi::OsStr>| {
        part.and_then(std::ffi::OsStr::to_str)
            .and_then(|part| syntaxes.find_syntax_by_extension(part))
    };
    let syntax = by(path.file_name()).or_else(|| by(path.extension()))?;
    (syntax.name != syntaxes.find_syntax_plain_text().name).then_some(syntax)
}

/// syntect partway through a file: the parser's state and the scopes open
/// after the last line it lexed. A clone continues from the same point.
#[derive(Debug, Clone)]
struct Lexer<'s> {
    syntaxes: &'s SyntaxSet,
    state: ParseState,
    scopes: ScopeStack,
    /// The scope every string literal's scope starts with.
    string: Scope,
}

impl<'s> Lexer<'s> {
    /// A lexer at the top of a file written in `syntax`.
    fn new(syntax: &SyntaxReference, syntaxes: &'s SyntaxSet) -> Self {
        Self {
            syntaxes,
            state: ParseState::new(syntax),
            scopes: ScopeStack::new(),
            string: Scope::new("string").expect("`string` is a valid scope"),
        }
    }

    /// The scope operations of `text` as the next line, with the line as
    /// syntect lexes it: ending in a line break.
    fn parse(&mut self, text: &str) -> Option<(String, Vec<(usize, ScopeStackOp)>)> {
        let mut line = String::with_capacity(text.len() + 1);
        line.push_str(text);
        line.push('\n');
        let ops = self.state.parse_line(&line, self.syntaxes).ok()?;
        Some((line, ops))
    }

    /// Lexes `text` as the next line; `None` when syntect cannot.
    fn skip(&mut self, text: &str) -> Option<()> {
        let (_, ops) = self.parse(text)?;
        ops.iter()
            .try_for_each(|(_, op)| self.scopes.apply(op).ok())
    }

    /// Lexes `text` as the next line, telling `each` every character of it,
    /// its line break included, and whether it sits in a string literal;
    /// `None` when syntect cannot.
    fn line(&mut self, text: &str, mut each: impl FnMut(char, bool)) -> Option<()> {
        let (line, ops) = self.parse(text)?;
        let mut ops = ops.into_iter().peekable();
        for (offset, c) in line.char_indices() {
            while let Some((_, op)) = ops.next_if(|(at, _)| *at <= offset) {
                self.scopes.apply(&op).ok()?;
            }
            let string = self.string;
            each(
                c,
                self.scopes
                    .as_slice()
                    .iter()
                    .any(|scope| string.is_prefix_of(*scope)),
            );
        }
        // An operation at the end of the line holds for the next one.
        ops.try_for_each(|(_, op)| self.scopes.apply(&op).ok())
    }
}

/// The meaning of `lines` lexed by `lexer`, which stands where they start;
/// `layout` keeps each line's indentation. `None` when syntect cannot lex
/// them.
fn meaning<'a>(
    lines: impl Iterator<Item = &'a str>,
    mut lexer: Lexer<'_>,
    layout: bool,
) -> Option<Vec<Meaning>> {
    let mut meaning = Vec::new();
    // Whether whitespace came since the last character kept; a line break
    // is whitespace too.
    let mut gap = false;
    for text in lines {
        if layout && !text.trim().is_empty() {
            let indentation = text.strip_suffix(text.trim_start()).unwrap_or_default();
            meaning.push(Meaning::Line(indentation.to_owned()));
        }
        lexer.line(text, |c, in_string| {
            if in_string {
                meaning.push(Meaning::Char(c));
            } else if c.is_whitespace() {
                gap = true;
                return;
            } else {
                if gap
                    && is_word(c)
                    && matches!(meaning.last(), Some(Meaning::Char(p)) if is_word(*p))
                {
                    meaning.push(Meaning::Gap);
                }
                meaning.push(Meaning::Char(c));
            }
            gap = false;
        })?;
    }
    Some(meaning)
}

/// Whether `hunk` removes and adds lines.
fn replaces(hunk: &Hunk) -> bool {
    let has = |op| hunk.lines.iter().any(|line| line.op == op);
    has(DiffOp::Removed) && has(DiffOp::Added)
}

/// The lines of `hunk` before (`Removed`) or after (`Added`) it applies.
fn side(hunk: &Hunk, dropped: DiffOp) -> impl Iterator<Item = &str> {
    hunk.lines
        .iter()
        .filter(move |line| line.op != dropped)
        .map(|line| line.text.as_str())
}

/// Whether `hunk`'s two sides are equal once all whitespace is stripped: the
/// only hunks whose whitespace is worth lexing.
fn differs_only_in_whitespace(hunk: &Hunk) -> bool {
    let stripped = |op: DiffOp| -> Vec<char> {
        hunk.lines
            .iter()
            .filter(|line: &&DiffLine| line.op == op)
            .flat_map(|line| line.text.chars().filter(|c| !c.is_whitespace()))
            .collect()
    };
    replaces(hunk) && stripped(DiffOp::Removed) == stripped(DiffOp::Added)
}

/// The hunks of every file `reading`'s patches update, each with the file's
/// path as the patch names it.
fn updated_hunks(reading: CommandReading) -> impl Iterator<Item = (String, Hunk)> {
    reading
        .steps
        .into_iter()
        .filter_map(|step| match step.intent {
            Intent::Edit { files } => Some(files),
            _ => None,
        })
        .flatten()
        .filter_map(|file| match file.change {
            FileChange::Update { hunks, .. } => Some((file.path, hunks)),
            FileChange::Add { .. } | FileChange::Delete => None,
        })
        .flat_map(|(path, hunks)| hunks.into_iter().map(move |hunk| (path.clone(), hunk)))
}

/// Whether `hunk` of the file at `path`, whose text is `file`, only changes
/// whitespace that carries no meaning in the file's language. `hunk` carries
/// the line numbers of where it places in `file`; one that does not place
/// carries none and is not judged.
fn meaningless_whitespace(syntaxes: &SyntaxSet, path: &str, file: &str, hunk: &Hunk) -> bool {
    let judged = || {
        let syntax = language(syntaxes, path)?;
        // The hunk's first line that is in the file, numbered from 1.
        let first = hunk.lines.iter().find_map(|line| line.old)?;
        let above = usize::try_from(first).ok()?.checked_sub(1)?;
        let mut lexer = Lexer::new(syntax, syntaxes);
        for line in file.lines().take(above) {
            lexer.skip(line)?;
        }
        let layout = INDENTATION_SENSITIVE.contains(&syntax.name.as_str());
        let before = meaning(side(hunk, DiffOp::Added), lexer.clone(), layout)?;
        let after = meaning(side(hunk, DiffOp::Removed), lexer, layout)?;
        Some(before == after)
    };
    judged().unwrap_or(false)
}

impl ScriptPolicy for WhitespacePatch {
    type State = Syntaxes;

    async fn check(&self, script: &ScriptCheck<'_>, syntaxes: &Syntaxes) -> ScriptDecision {
        if parsed(script).is_none() {
            return ScriptDecision::Pass;
        }
        // A patch on a server edits files this host cannot read.
        let Some(start) = script.start.host() else {
            return ScriptDecision::Pass;
        };
        let paths: HashSet<String> = updated_hunks(CommandReading::of(script.script))
            .filter(|(_, hunk)| differs_only_in_whitespace(hunk))
            .map(|(path, _)| path)
            .collect();
        if paths.is_empty() {
            return ScriptDecision::Pass;
        }
        // Each file as the patch tool resolves it, against the directory the
        // script starts in; one that cannot be read is not judged.
        let files: HashMap<String, String> =
            futures_util::future::join_all(paths.into_iter().map(|path| async move {
                let text = async_fs::read_to_string(start.join(&path)).await.ok()?;
                Some((path, text))
            }))
            .await
            .into_iter()
            .flatten()
            .collect();
        if files.is_empty() {
            return ScriptDecision::Pass;
        }
        let script = script.script.to_owned();
        let start = start.to_path_buf();
        let syntaxes = syntaxes.0.clone();
        // Placing the hunks, loading the syntaxes and lexing are blocking
        // work. Reading the script in its directory places each hunk with
        // the patch tool's own matching.
        let refused = blocking::unblock(move || {
            let syntaxes = syntaxes.get_or_init(SyntaxSet::load_defaults_newlines);
            updated_hunks(CommandReading::of_in(&script, &start))
                .filter(|(_, hunk)| differs_only_in_whitespace(hunk))
                .find_map(|(path, hunk)| {
                    let file = files.get(&path)?;
                    meaningless_whitespace(syntaxes, &path, file, &hunk).then_some(path)
                })
        })
        .await;
        refused.map_or(ScriptDecision::Pass, |path| {
            refuse(&WhitespacePatchRefused { path: &path })
        })
    }
}

#[derive(Template)]
#[template(path = "refused_host_command.txt")]
struct HostCommandRefused<'a> {
    name: &'a str,
    replacement: &'a str,
}

#[derive(Template)]
#[template(path = "refused_sed.txt")]
struct SedRefused<'a> {
    unsupported: &'a aither_sed::Unsupported,
}

/// Host programs a script's own shell does not run, each with what to use
/// instead.
const HOST_COMMANDS: [(&str, &str); 3] = [
    ("grep", "`rg`"),
    ("awk", "`python3` or `jq`"),
    ("find", "`fd`"),
];

/// Refuses a script whose own shell runs `grep`, `awk` or `find`, or `sed`
/// in a form `aither_sed::supports` does not accept (the line ranges); the
/// allowed forms run the host `sed`.
///
/// The rule binds the script's own shell, the way shell functions would: a
/// command it runs by name, through `time` or `eval` included. A program
/// named by path, run through a wrapper such as `env`, `xargs` or
/// `command`, or by another shell (`sh -c`, a script file) is the host's.
#[derive(Debug, Clone, Copy, Default)]
pub struct ManagedShellCommands;

/// Whether `script`'s own shell runs `command` by its name.
fn run_by_own_shell(script: &ShellScript, command: &ShellCommand) -> bool {
    let mut current = command;
    loop {
        let runner = match (current.wrapper(), current.interpreter()) {
            (Some(wrapper), _) => (script.get(wrapper), "time"),
            (None, Some(interpreter)) => (script.get(interpreter), "eval"),
            (None, None) => return true,
        };
        if runner.0.program() != runner.1 {
            return false;
        }
        current = runner.0;
    }
}

/// The refusal of the first command `script`'s own shell runs that the
/// managed bash does not provide.
fn host_command(script: &ShellScript) -> Option<ScriptDecision> {
    script.commands().iter().find_map(|command| {
        let name = command.argv().first()?.as_str();
        if !run_by_own_shell(script, command) {
            return None;
        }
        if name == "sed" {
            let arguments = command.arguments().iter().map(OsString::from).collect();
            return aither_sed::supports(arguments).err().map(|unsupported| {
                refuse(&SedRefused {
                    unsupported: &unsupported,
                })
            });
        }
        HOST_COMMANDS
            .iter()
            .find(|(blocked, _)| *blocked == name)
            .map(|&(name, replacement)| refuse(&HostCommandRefused { name, replacement }))
    })
}

impl ScriptPolicy for ManagedShellCommands {
    type State = ();

    fn check(
        &self,
        script: &ScriptCheck<'_>,
        (): &(),
    ) -> impl Future<Output = ScriptDecision> + Send {
        std::future::ready(
            parsed(script)
                .and_then(host_command)
                .unwrap_or(ScriptDecision::Pass),
        )
    }
}

#[cfg(test)]
mod tests;
