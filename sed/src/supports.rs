//! The print-only `sed -n` forms the host `sed` may run for May's command
//! policy until the shell's `sed` is this program.

use std::ffi::OsString;

use crate::script;

const UNSUPPORTED: &str = include_str!("../templates/unsupported.txt");

/// An option or script outside the `sed -n 'N,Mp'` forms.
#[derive(Debug, thiserror::Error)]
#[error("{}", UNSUPPORTED)]
pub struct Unsupported;

/// Whether a host's `sed` prints the same lines as this program for `args`.
///
/// `args` (the program name excluded) qualify when they are GNU `sed -n` with
/// one script of `p` commands addressed by `N`, `N,M`, `N,$` or `$`, joined by
/// `;`, over files, `-` or standard input.
///
/// May's command policy uses this, and the host `sed` it lets through, until
/// the managed shell's `sed` is this program, which runs every script that
/// writes no file.
///
/// # Errors
/// Returns [`Unsupported`] for an option or script outside those forms.
pub fn supports(args: Vec<OsString>) -> Result<(), Unsupported> {
    if let [only] = args.as_slice()
        && only == "--version"
    {
        return Ok(());
    }
    let mut quiet = false;
    let mut script: Option<String> = None;
    let mut operands = Vec::new();
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        let Some(text) = argument.to_str().filter(|text| text.starts_with('-')) else {
            operands.push(argument);
            continue;
        };
        match text {
            "-" => operands.push(argument),
            "--" => {
                operands.extend(arguments);
                break;
            }
            "--quiet" | "--silent" => quiet = true,
            _ if text.starts_with("--") => return Err(Unsupported),
            _ => {
                let mut flags = text[1..].chars();
                while let Some(flag) = flags.next() {
                    match flag {
                        'n' => quiet = true,
                        'e' => {
                            let attached = flags.as_str();
                            let value = if attached.is_empty() {
                                arguments
                                    .next()
                                    .and_then(|value| value.into_string().ok())
                                    .ok_or(Unsupported)?
                            } else {
                                attached.to_owned()
                            };
                            if script.replace(value).is_some() {
                                return Err(Unsupported);
                            }
                            break;
                        }
                        _ => return Err(Unsupported),
                    }
                }
            }
        }
    }
    let script = match script {
        Some(script) => script,
        None => operands
            .into_iter()
            .next()
            .and_then(|operand| operand.into_string().ok())
            .ok_or(Unsupported)?,
    };
    if !quiet {
        return Err(Unsupported);
    }
    script::validate(&script)
}
