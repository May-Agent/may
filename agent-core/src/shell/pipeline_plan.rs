//! Pipeline tapping policy and tool-scoped ANSI color rules for May shells.

use aither_sandbox::{ColorInjection, PlanPipelines, ScriptCheck, StageDirective, StageSyntax};

use crate::command_reading::{CommandReading, Intent};

/// Selects multi-stage shell pipelines to tap and enables non-TTY ANSI color
/// only on known color-capable stages that did not specify color themselves.
#[derive(Debug, Clone, Copy, Default)]
pub struct MayPipelinePlanner;

impl PlanPipelines for MayPipelinePlanner {
    fn plan_pipeline(
        &self,
        _script: &ScriptCheck<'_>,
        stages: &[StageSyntax],
    ) -> Option<Vec<StageDirective>> {
        if stages.len() < 2 || excluded_pipeline(stages) {
            return None;
        }
        Some(
            stages
                .iter()
                .map(|stage| StageDirective {
                    command: stage.source.trim().to_owned(),
                    injected_color: stage_color_injection(stage),
                    line_slicer: is_pure_line_slicer(stage),
                })
                .collect(),
        )
    }
}

fn program_basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

fn excluded_pipeline(stages: &[StageSyntax]) -> bool {
    // Patch pipelines (`cat <<'EOF' | apply_patch`, `cat <<'EOF' | edit`) edit files rather than filter output.
    if stages.iter().any(|stage| {
        stage
            .program
            .as_deref()
            .is_some_and(|name| matches!(program_basename(name), "apply_patch" | "edit"))
    }) {
        return true;
    }
    let first = &stages[0];
    // `pipe <pid> 0 | rg ...` reads an already-retained stage buffer.
    if first
        .program
        .as_deref()
        .is_some_and(|name| program_basename(name) == "pipe")
    {
        return true;
    }
    // Static file reads on stage 0 (`cat foo.rs | rg bar`, `sed -n '1,50p' a | ...`)
    // read directly from disk in under a millisecond and need no stage buffer.
    let reading = CommandReading::of(&first.source);
    !reading.steps.is_empty()
        && reading
            .steps
            .iter()
            .all(|step| matches!(step.intent, Intent::Read { .. }))
}

/// Whether `stage` is a pure line-mode `tail` or `head` reading piped `stdin`,
/// so upstream harness-injected SGR sequences can pass through to its stdout.
fn is_pure_line_slicer(stage: &StageSyntax) -> bool {
    if stage.stdin_literal.is_some() || stage.has_heredoc {
        return false;
    }
    let Some(program) = stage.program.as_deref().map(program_basename) else {
        return false;
    };
    if !matches!(program, "tail" | "head") {
        return false;
    }
    let mut iter = stage.literal_args.iter();
    while let Some(slot) = iter.next() {
        let Some(arg) = slot.as_deref() else {
            return false;
        };
        if arg == "-" || matches!(arg, "-q" | "--quiet" | "--silent") {
            continue;
        }
        if arg == "-n" || arg == "--lines" {
            let Some(Some(val)) = iter.next() else {
                return false;
            };
            if !is_line_count_spec(val) {
                return false;
            }
            continue;
        }
        if let Some(val) = arg
            .strip_prefix("--lines=")
            .or_else(|| arg.strip_prefix("-n"))
        {
            if !is_line_count_spec(val) {
                return false;
            }
            continue;
        }
        if let Some(digits) = arg.strip_prefix('-')
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            continue;
        }
        return false;
    }
    true
}

fn is_line_count_spec(text: &str) -> bool {
    let digits = text
        .strip_prefix('+')
        .or_else(|| text.strip_prefix('-'))
        .unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn has_global_color_override(assignments: &[String]) -> bool {
    assignments.iter().any(|name| {
        matches!(
            name.as_str(),
            "NO_COLOR" | "FORCE_COLOR" | "CLICOLOR" | "CLICOLOR_FORCE" | "TERM"
        )
    })
}

fn has_color_flag(args: &[Option<String>]) -> bool {
    args.iter().flatten().any(|arg| {
        arg == "--color"
            || arg == "--no-color"
            || arg.starts_with("--color=")
            || arg.starts_with("--colour")
    })
}

fn stage_color_injection(stage: &StageSyntax) -> Option<ColorInjection> {
    if has_global_color_override(&stage.assignments) || has_color_flag(&stage.literal_args) {
        return None;
    }
    let program = program_basename(stage.program.as_deref()?);
    match program {
        "cargo" => {
            if stage
                .assignments
                .iter()
                .any(|name| name == "CARGO_TERM_COLOR")
            {
                return None;
            }
            // Machine-readable cargo JSON output must stay pure JSON Lines.
            let machine_json = stage.literal_args.iter().flatten().any(|arg| {
                arg.starts_with("--message-format") && arg.contains("json")
                    || arg == "--format-version"
            });
            if machine_json {
                return None;
            }
            Some(ColorInjection {
                env: vec![("CARGO_TERM_COLOR".to_owned(), "always".to_owned())],
                flags: Vec::new(),
            })
        }
        "rg" => {
            let structured = stage.literal_args.iter().flatten().any(|arg| {
                matches!(
                    arg.as_str(),
                    "--json"
                        | "--vimgrep"
                        | "--files"
                        | "-l"
                        | "--files-with-matches"
                        | "--files-without-match"
                        | "-c"
                        | "--count"
                        | "--count-matches"
                        | "-0"
                        | "--null"
                )
            });
            if structured {
                return None;
            }
            Some(ColorInjection {
                env: Vec::new(),
                flags: vec!["--color=always".to_owned()],
            })
        }
        "pytest" => {
            if stage
                .assignments
                .iter()
                .any(|name| name == "PYTEST_ADDOPTS")
            {
                return None;
            }
            Some(ColorInjection {
                env: Vec::new(),
                flags: vec!["--color=yes".to_owned()],
            })
        }
        "git" => git_color_injection(stage),
        _ => None,
    }
}

fn git_color_injection(stage: &StageSyntax) -> Option<ColorInjection> {
    if stage
        .assignments
        .iter()
        .any(|name| name.starts_with("GIT_CONFIG"))
    {
        return None;
    }
    // Check that no `-c color...` override is present and locate the subcommand.
    let mut iter = stage.literal_args.iter();
    let mut subcommand: Option<&str> = None;
    let mut porcelain = false;
    while let Some(slot) = iter.next() {
        let arg = slot.as_deref()?;
        if subcommand.is_none() {
            if matches!(
                arg,
                "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace"
            ) {
                let val = iter.next()?.as_deref()?;
                if arg == "-c" && val.starts_with("color.") {
                    return None;
                }
                continue;
            }
            if arg.starts_with('-') {
                if arg.starts_with("-c") && arg.contains("color.") {
                    return None;
                }
                continue;
            }
            subcommand = Some(arg);
        } else if matches!(
            arg,
            "--porcelain" | "-s" | "--short" | "--name-only" | "--name-status" | "-z" | "--raw"
        ) || arg.starts_with("--porcelain=")
            || arg.starts_with("--format=")
            || arg.starts_with("--pretty=")
        {
            porcelain = true;
        }
    }
    if porcelain {
        return None;
    }
    matches!(
        subcommand?,
        "diff" | "log" | "show" | "status" | "grep" | "branch" | "shortlog" | "range-diff"
    )
    .then(|| ColorInjection {
        env: Vec::new(),
        flags: vec!["-c".to_owned(), "color.ui=always".to_owned()],
    })
}

#[cfg(test)]
mod tests {

    use aither_sandbox::{
        Interpreter, PlanPipelines, SandboxMode, ScriptCheck, ScriptDir, ScriptOrigin, ShellScript,
        StageSyntax,
    };

    use super::*;

    fn syntax(
        step: usize,
        source: &str,
        program: Option<&str>,
        args: &[&str],
        assignments: &[&str],
    ) -> StageSyntax {
        StageSyntax {
            step,
            source: source.to_owned(),
            program: program.map(str::to_owned),
            literal_args: args.iter().map(|s| Some((*s).to_owned())).collect(),
            assignments: assignments.iter().map(|s| (*s).to_owned()).collect(),
            stdin_literal: None,
            has_heredoc: false,
        }
    }

    fn plan(script_src: &str, stages: &[StageSyntax]) -> Option<Vec<StageDirective>> {
        let parsed = ShellScript::parse(script_src);
        let start = ScriptDir::Host(std::path::PathBuf::from("/tmp"));
        let check = ScriptCheck {
            launch: aither_sandbox::LaunchId::from_raw(1),
            script: script_src,
            interpreter: Interpreter::Shell,
            shell: Some(parsed.as_ref()),
            origin: ScriptOrigin::Foreground,
            sandbox: SandboxMode::Sandboxed,
            location: None,
            start: &start,
            call: None,
        };
        MayPipelinePlanner.plan_pipeline(&check, stages)
    }

    #[test]
    fn excludes_apply_patch_static_file_reads_and_pipe_self_calls() {
        let patch_stages = [
            StageSyntax {
                stdin_literal: Some("*** Begin Patch\n*** End Patch\n".into()),
                has_heredoc: true,
                ..syntax(0, "cat <<'EOF'", Some("cat"), &[], &[])
            },
            syntax(1, "apply_patch", Some("apply_patch"), &[], &[]),
        ];
        assert!(plan("cat <<'EOF' | apply_patch", &patch_stages).is_none());

        let edit_stages = [
            StageSyntax {
                stdin_literal: Some("[src/main.rs]\nPUT 1#PM:\n+fn main() {}\n".into()),
                has_heredoc: true,
                ..syntax(0, "cat <<'EOF'", Some("cat"), &[], &[])
            },
            syntax(1, "edit", Some("edit"), &[], &[]),
        ];
        assert!(plan("cat <<'EOF' | edit", &edit_stages).is_none());

        let file_read = [
            syntax(0, "cat Cargo.toml", Some("cat"), &["Cargo.toml"], &[]),
            syntax(1, "rg version", Some("rg"), &["version"], &[]),
        ];
        assert!(plan("cat Cargo.toml | rg version", &file_read).is_none());

        let pipe_self = [
            syntax(0, "pipe 4242 0", Some("pipe"), &["4242", "0"], &[]),
            syntax(1, "rg error", Some("rg"), &["error"], &[]),
        ];
        assert!(plan("pipe 4242 0 | rg error", &pipe_self).is_none());
    }

    #[test]
    fn injects_tool_scoped_color_only_when_unspecified_and_detects_line_slicers() {
        let stages = [
            syntax(
                0,
                "cargo clippy --workspace",
                Some("cargo"),
                &["clippy", "--workspace"],
                &[],
            ),
            syntax(1, "tail -n 50", Some("tail"), &["-n", "50"], &[]),
        ];
        let planned = plan("cargo clippy --workspace | tail -n 50", &stages).expect("tapped");
        assert_eq!(
            planned[0].injected_color,
            Some(ColorInjection {
                env: vec![("CARGO_TERM_COLOR".into(), "always".into())],
                flags: vec![],
            })
        );
        assert!(!planned[0].line_slicer);
        assert_eq!(planned[1].injected_color, None);
        assert!(planned[1].line_slicer);

        // Explicit `--color=always` or `CARGO_TERM_COLOR` must leave `injected_color` `None`.
        let explicit = [
            syntax(
                0,
                "cargo clippy --color=always",
                Some("cargo"),
                &["clippy", "--color=always"],
                &[],
            ),
            syntax(1, "head -c 64", Some("head"), &["-c", "64"], &[]),
        ];
        let planned = plan("cargo clippy --color=always | head -c 64", &explicit).expect("tapped");
        assert_eq!(planned[0].injected_color, None);
        assert!(
            !planned[1].line_slicer,
            "byte-mode head is not a line slicer"
        );

        // Git diff gets `-c color.ui=always`, while `git status --porcelain` does not.
        let git_diff = [
            syntax(0, "git diff", Some("git"), &["diff"], &[]),
            syntax(1, "head -20", Some("head"), &["-20"], &[]),
        ];
        let planned = plan("git diff | head -20", &git_diff).expect("tapped");
        assert_eq!(
            planned[0].injected_color,
            Some(ColorInjection {
                env: vec![],
                flags: vec!["-c".into(), "color.ui=always".into()],
            })
        );
        assert!(planned[1].line_slicer);

        let git_porcelain = [
            syntax(
                0,
                "git status --porcelain",
                Some("git"),
                &["status", "--porcelain"],
                &[],
            ),
            syntax(1, "rg '^M'", Some("rg"), &["^M"], &[]),
        ];
        let planned = plan("git status --porcelain | rg '^M'", &git_porcelain).expect("tapped");
        assert_eq!(planned[0].injected_color, None);
        assert_eq!(
            planned[1].injected_color,
            Some(ColorInjection {
                env: vec![],
                flags: vec!["--color=always".into()],
            })
        );
    }
}
