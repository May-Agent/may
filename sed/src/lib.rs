//! `aither-sed`: the `sed` of sandboxed agent scripts, uutils' `sed` run in
//! its sandbox mode so that no script writes a file.
//!
//! uutils' `--sandbox` refuses the `w`/`W`/`r`/`R`/`e` commands and the
//! `s///w` and `s///e` flags while the script compiles, before any file is
//! opened. This crate adds the refusal of `-i`/`--in-place` and names
//! `apply_patch` in every refusal. Everything else runs as uutils' `sed`.

mod script;
mod supports;

/// The print-only `sed -n` check May's command policy uses until the managed
/// shell's `sed` is this program.
pub use supports::{Unsupported, supports};

use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::PathBuf;

use askama::Template;
use sed::sed::command::ProcessingContext;
use sed::sed::compiler::compile;
use sed::sed::script_line_provider::ScriptValue;
use sed::sed::{uu_app, uumain};

/// Printed by `--version`; the tool-runtime initializer reads the version
/// from its second word.
pub const VERSION: &str = concat!("aither-sed ", env!("CARGO_PKG_VERSION"));

/// The program name uutils prints in its diagnostics and usage.
const PROGRAM: &str = "aither-sed";

/// Exit status of a refused write form, as uutils' own script errors.
pub const EXIT_REFUSED: u8 = 1;

/// Exit status of a command line clap rejects, as uutils' usage errors.
const EXIT_USAGE: u8 = 1;

/// Why uutils' sandbox refuses a script: its messages for the write commands
/// and flags all name the sandbox, and no other script error does.
const SANDBOX_MARK: &str = "--sandbox";

/// The refusal of in-place editing.
const IN_PLACE: &str = "`-i` and `--in-place` edit files";

/// A write form refused before anything runs.
#[derive(Template)]
#[template(path = "refused_write.txt")]
struct Refused<'a> {
    reason: &'a str,
}

fn refuse(stderr: &mut impl Write, reason: &str) -> u8 {
    let text = Refused { reason }
        .render()
        .expect("the refusal template renders");
    let _ = write!(stderr, "{text}");
    EXIT_REFUSED
}

/// Runs `aither-sed` with `args` (the program name excluded) on the process's
/// standard streams and returns its exit status.
#[must_use]
pub fn run(args: Vec<OsString>) -> u8 {
    let mut stderr = io::stderr();
    if let [only] = args.as_slice()
        && only == "--version"
    {
        return match writeln!(io::stdout(), "{VERSION}") {
            Ok(()) => 0,
            Err(_) => 4,
        };
    }
    // Before clap, which cannot parse the attached `-iSUFFIX`.
    if in_place_requested(&args) {
        return refuse(&mut stderr, IN_PLACE);
    }
    let command_line: Vec<OsString> = std::iter::once(OsString::from(PROGRAM))
        .chain(args.iter().cloned())
        .collect();
    let matches = match uu_app().try_get_matches_from(&command_line) {
        Ok(matches) => matches,
        Err(error) => {
            let _ = error.print();
            return if error.use_stderr() { EXIT_USAGE } else { 0 };
        }
    };
    if matches.contains_id("in-place") {
        return refuse(&mut stderr, IN_PLACE);
    }
    let mut context = ProcessingContext {
        sandbox: true,
        regex_extended: matches.get_flag("regexp-extended"),
        posix: matches.get_flag("posix"),
        uutil_extensions: matches.get_flag("uutil-extensions"),
        ..ProcessingContext::default()
    };
    // Any other script error is reported by uutils when it runs the script.
    if let Err(error) = compile(scripts(&matches), &mut context) {
        let message = error.to_string();
        if message.contains(SANDBOX_MARK) {
            return refuse(&mut stderr, &message);
        }
    }
    let status = uumain(
        std::iter::once(OsString::from(PROGRAM))
            .chain([OsString::from(SANDBOX_MARK)])
            .chain(args),
    );
    u8::try_from(status).unwrap_or(u8::MAX)
}

/// Whether `args` (the program name excluded) ask for in-place editing in the
/// short form: a single-dash argument with an `i` among its flags, which
/// `-iSUFFIX` and clusters such as `-ni.bak` have and clap cannot parse. The
/// scan stops at `--`; the flags `e`, `f` and `l` take the rest of their
/// argument, or the next argument when last, as a value, and a value is never
/// a flag. The long `--in-place` is found by clap.
fn in_place_requested(args: &[OsString]) -> bool {
    /// Long options whose value is the next argument unless given with `=`.
    const LONG_WITH_VALUE: [&[u8]; 3] = [b"--expression", b"--file", b"--line-length"];
    let mut arguments = args
        .iter()
        .map(|argument| OsStr::as_encoded_bytes(argument));
    while let Some(argument) = arguments.next() {
        if argument == b"--" {
            return false;
        }
        if LONG_WITH_VALUE.contains(&argument) {
            arguments.next();
            continue;
        }
        let Some(flags) = argument
            .strip_prefix(b"-")
            .filter(|flags| !flags.is_empty() && !flags.starts_with(b"-"))
        else {
            continue;
        };
        for (position, flag) in flags.iter().enumerate() {
            match flag {
                b'i' => return true,
                b'e' | b'f' | b'l' => {
                    if position + 1 == flags.len() {
                        arguments.next();
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    false
}

/// The scripts of the command line in the order given: the `-e` and `-f`
/// scripts, or else the first operand, as uutils reads them.
fn scripts(matches: &clap::ArgMatches) -> Vec<ScriptValue> {
    let mut indexed = Vec::new();
    if let Some(indices) = matches.indices_of("expression") {
        let values = matches.get_many::<String>("expression").unwrap_or_default();
        indexed.extend(
            indices
                .zip(values)
                .map(|(index, value)| (index, ScriptValue::StringVal(value.clone()))),
        );
    }
    if let Some(indices) = matches.indices_of("script-file") {
        let values = matches
            .get_many::<PathBuf>("script-file")
            .unwrap_or_default();
        indexed.extend(
            indices
                .zip(values)
                .map(|(index, value)| (index, ScriptValue::PathVal(value.clone()))),
        );
    }
    if indexed.is_empty()
        && let Some(script) = matches.get_one::<String>("script")
    {
        indexed.push((0, ScriptValue::StringVal(script.clone())));
    }
    indexed.sort_by_key(|(index, _)| *index);
    indexed.into_iter().map(|(_, value)| value).collect()
}

#[cfg(test)]
mod tests;
