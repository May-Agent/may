//! What the model reads about its shell: every refusal, notice, marker and
//! reply the shell and its tools raise, rendered from typed facts by one
//! template each under `templates/shell/`.

use aither_core::llm::CommandStream;
use aither_sandbox::{
    AnyShellWording, CallRefusal, CommandKilled, FoldedLines, JobInputSent, JobKillAsked, JobRead,
    JobStatus, JsonAsTsv, MonitorRefusal, MonitorStarted, OmittedLines, OutputSaved, RepeatedLines,
    RunFailure, ShellWording, StoredBinaryPart, TerminalError,
};
use askama::Template;

#[derive(Template)]
#[template(path = "shell/unchanged_task_read.txt", escape = "none")]
struct UnchangedTaskRead<'a> {
    task_id: &'a str,
}

#[derive(Template)]
#[template(path = "shell/job_read.txt", escape = "none")]
struct JobReadText<'a> {
    delta: &'a str,
    has_more: bool,
    cursor: usize,
    task_id: &'a str,
    status_text: &'a str,
    bytes_read: usize,
    total_bytes: usize,
}

#[derive(Template)]
#[template(path = "shell/job_input.txt", escape = "none")]
struct JobInputText<'a> {
    task_id: &'a str,
}

#[derive(Template)]
#[template(path = "shell/job_kill.txt", escape = "none")]
struct JobKillText<'a> {
    task_id: &'a str,
    killed: bool,
}

#[derive(Template)]
#[template(path = "shell/call_refused.txt", escape = "none")]
struct CallRefused<'a> {
    refusal: &'a CallRefusal,
}

#[derive(Template)]
#[template(path = "shell/run_failure.txt", escape = "none")]
struct RunFailureText<'a, 'b> {
    failure: &'a RunFailure<'b>,
}

#[derive(Template)]
#[template(path = "shell/monitor_refused.txt", escape = "none")]
struct MonitorRefused<'a> {
    refusal: &'a MonitorRefusal,
    max_timeout_secs: u64,
}

#[derive(Template)]
#[template(path = "shell/monitor_started.txt", escape = "none")]
struct MonitorStartedText<'a> {
    task_id: &'a str,
    interval_secs: u64,
}

#[derive(Template)]
#[template(path = "shell/command_timed_out.txt", escape = "none")]
struct Killed<'a> {
    killed: CommandKilled<'a>,
}

#[derive(Template)]
#[template(path = "shell/command_output_saved.txt", escape = "none")]
struct OutputSavedText<'a> {
    preview: &'a str,
    path: &'a str,
    line_count: usize,
    byte_count: usize,
}

#[derive(Template)]
#[template(path = "shell/stored_binary_part.txt", escape = "none")]
struct StoredBinaryPartText<'a> {
    mime: &'a str,
    byte_count: usize,
    url: &'a str,
    path: &'a str,
}

#[derive(Template)]
#[template(path = "shell/json_tsv_output.txt", escape = "none")]
struct JsonTsvText<'a> {
    tsv: &'a str,
    url: &'a str,
    path: &'a str,
}

#[derive(Template)]
#[template(path = "shell/folded_code_output.txt", escape = "none")]
struct FoldedCodeText<'a> {
    folded: &'a str,
}

#[derive(Template)]
#[template(path = "shell/blank_lines_marker.txt", escape = "none")]
struct BlankLinesText {
    count: usize,
}

#[derive(Template)]
#[template(path = "shell/folded_lines_marker.txt", escape = "none")]
struct FoldedLinesText<'a> {
    indent: &'a str,
    count: usize,
    first: usize,
    last: usize,
}

#[derive(Template)]
#[template(path = "shell/omitted_lines_marker.txt", escape = "none")]
struct OmittedLinesText {
    first: usize,
    last: usize,
    total: usize,
}

#[derive(Template)]
#[template(path = "shell/repeated_lines_marker.txt", escape = "none")]
struct RepeatedLinesText {
    first: usize,
    last: usize,
    count: usize,
}

fn rendered(template: &impl Template) -> String {
    template
        .render()
        .expect("a shell wording template renders typed input")
}

fn job_status_text(status: &JobStatus) -> String {
    match status {
        JobStatus::Running => "is running".to_owned(),
        JobStatus::Completed { exit_code, .. } => format!("completed with exit code {exit_code}"),
        JobStatus::Failed { error, .. } => format!("failed: {error}"),
        JobStatus::Killed { .. } => "was killed".to_owned(),
    }
}

/// May's wording of the shell.
#[derive(Debug, Clone, Copy, Default)]
pub struct MayShellWording;

impl MayShellWording {
    /// This wording, shared.
    #[must_use]
    pub fn any() -> AnyShellWording {
        AnyShellWording::new(Self)
    }
}

impl ShellWording for MayShellWording {
    fn job_read(&self, read: &JobRead) -> String {
        match read {
            JobRead::Unchanged { task_id } => rendered(&UnchangedTaskRead { task_id }),
            JobRead::Delta(delta) => {
                let output = CommandStream::Inline {
                    text: String::from_utf8_lossy(&delta.bytes).into_owned(),
                }
                .render_for_model();
                rendered(&JobReadText {
                    delta: &output,
                    has_more: delta.cursor < delta.total_bytes,
                    cursor: delta.cursor,
                    task_id: &delta.task_id,
                    status_text: &job_status_text(&delta.status),
                    bytes_read: delta.bytes.len(),
                    total_bytes: delta.total_bytes,
                })
            }
        }
    }

    fn job_input(&self, sent: &JobInputSent<'_>) -> String {
        rendered(&JobInputText {
            task_id: sent.task_id,
        })
    }

    fn job_kill(&self, kill: &JobKillAsked<'_>) -> String {
        rendered(&JobKillText {
            task_id: kill.task_id,
            killed: kill.killed,
        })
    }

    fn call_refused(&self, refusal: &CallRefusal) -> String {
        assert!(
            matches!(
                refusal,
                CallRefusal::EmptyDescription
                    | CallRefusal::InterpreterNotConfigured { .. }
                    | CallRefusal::TimeoutOutOfRange { .. }
                    | CallRefusal::EmptyTaskId
                    | CallRefusal::UnknownTask { .. }
                    | CallRefusal::ZeroMaxBytes
            ),
            "the shell refused a call May has no wording for: {refusal:?}"
        );
        rendered(&CallRefused { refusal })
    }

    fn run_failure(&self, failure: &RunFailure<'_>) -> String {
        assert!(
            matches!(
                failure,
                RunFailure::NoWorkspaceMount
                    | RunFailure::OutsideWorkspace(_)
                    | RunFailure::IdsExhausted
                    | RunFailure::Terminal(_)
                    | RunFailure::Failed { .. }
            ),
            "the shell failed in a way May has no wording for: {failure:?}"
        );
        rendered(&RunFailureText { failure })
    }

    fn monitor_refusal(&self, refusal: &MonitorRefusal) -> String {
        let max_timeout_secs = match refusal {
            MonitorRefusal::TimeoutOutOfRange { max } => max.as_secs(),
            _ => 0,
        };
        rendered(&MonitorRefused {
            refusal,
            max_timeout_secs,
        })
    }

    fn monitor_started(&self, started: &MonitorStarted<'_>) -> String {
        rendered(&MonitorStartedText {
            task_id: started.task_id,
            interval_secs: started.interval_secs,
        })
    }

    fn command_killed(&self, killed: &CommandKilled<'_>) -> String {
        rendered(&Killed { killed: *killed })
    }

    fn output_saved(&self, saved: &OutputSaved<'_>) -> String {
        rendered(&OutputSavedText {
            preview: saved.preview,
            path: saved.path,
            line_count: saved.line_count,
            byte_count: saved.byte_count,
        })
    }

    fn stored_binary_part(&self, part: &StoredBinaryPart<'_>) -> String {
        rendered(&StoredBinaryPartText {
            mime: part.mime,
            byte_count: part.byte_count,
            url: part.url,
            path: part.path,
        })
    }

    fn json_as_tsv(&self, json: &JsonAsTsv<'_>) -> String {
        rendered(&JsonTsvText {
            tsv: json.tsv,
            url: json.url,
            path: json.path,
        })
    }

    fn folded_code(&self, folded: &str) -> String {
        rendered(&FoldedCodeText { folded })
    }

    fn blank_lines(&self, count: usize) -> String {
        rendered(&BlankLinesText { count })
    }

    fn folded_lines(&self, folded: &FoldedLines<'_>) -> String {
        rendered(&FoldedLinesText {
            indent: folded.indent,
            count: folded.count,
            first: folded.range.first,
            last: folded.range.last,
        })
    }

    fn omitted_lines(&self, omitted: &OmittedLines) -> String {
        rendered(&OmittedLinesText {
            first: omitted.range.first,
            last: omitted.range.last,
            total: omitted.total,
        })
    }

    fn repeated_lines(&self, repeated: &RepeatedLines) -> String {
        rendered(&RepeatedLinesText {
            first: repeated.range.first,
            last: repeated.range.last,
            count: repeated.count,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_monitor_refusal_names_the_cap() {
        let text = MayShellWording.monitor_refusal(&MonitorRefusal::TimeoutOutOfRange {
            max: Duration::from_secs(7200),
        });
        assert_eq!(
            text,
            "The monitor call was refused: timeout_secs must be between 1 and 7200."
        );
    }

    #[test]
    fn a_kill_before_the_bound_says_so() {
        assert_eq!(
            MayShellWording.command_killed(&CommandKilled::BeforeTimeout),
            "Killed before its timeout bound passed."
        );
    }

    #[test]
    fn a_repeated_read_is_refused_with_the_way_to_follow_a_job() {
        let text = MayShellWording.job_read(&JobRead::Unchanged {
            task_id: "amber-forest-thunder-pearl".into(),
        });
        assert!(text.contains("'amber-forest-thunder-pearl' produced no new output"));
        assert!(text.contains("`monitor`"));
    }

    #[test]
    fn a_failure_of_the_terminal_is_worded_without_its_frame() {
        let text = MayShellWording.run_failure(&RunFailure::Terminal(&TerminalError::Denied(
            "not today".into(),
        )));
        assert_eq!(text, "not today");
    }
}
