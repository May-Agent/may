//! Event lines of the one-shot stream.
//!
//! Assistant text goes to stdout and every event line to stderr. The two
//! share a terminal, so [`OneShotOutput`] ends the text's line before it
//! writes an event line.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write};

use aither_agent::{AgentError, AgentEvent, TimedEvent};
use aither_core::llm::tool::{CommandOutcome, ToolResult};
use askama::Template;
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

/// Longest one-line summary of a call, in characters.
const SUMMARY_LIMIT: usize = 100;

/// Longest error message shown on a failure line, in characters.
const MESSAGE_LIMIT: usize = 300;

/// What a tool call is shown as, remembered from its start event.
struct StartedCall {
    at: Timestamp,
    label: String,
}

/// The arguments of `terminal` and `terminal_background` the start line reads.
#[derive(Deserialize)]
struct TerminalCall {
    #[serde(default)]
    description: String,
    #[serde(default)]
    script: String,
}

/// How long a call ran.
struct Elapsed(SignedDuration);

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let millis = self.0.as_millis();
        if millis < 1000 {
            write!(f, "{millis}ms")
        } else {
            write!(f, "{:.1}s", self.0.as_secs_f64())
        }
    }
}

enum ToolOutcome<'a> {
    Done,
    Exited { code: i32 },
    Background { task_id: &'a str },
    Killed { task_id: &'a str },
}

enum LineKind<'a> {
    RunStart,
    Thinking {
        text: &'a str,
    },
    ToolStart {
        name: &'a str,
        label: &'a str,
    },
    BuiltInTool {
        tool: &'a str,
        detail: &'a str,
    },
    ToolFinished {
        name: &'a str,
        label: &'a str,
        elapsed: Option<Elapsed>,
        outcome: ToolOutcome<'a>,
    },
    ToolFailed {
        name: &'a str,
        label: &'a str,
        elapsed: Option<Elapsed>,
        message: &'a str,
    },
    BackgroundStarted {
        task_id: &'a str,
        output_file: &'a str,
    },
    BackgroundCompleted {
        task_id: &'a str,
    },
    Paused {
        reason: String,
    },
    Resumed {
        reason: String,
    },
    SkillActivated {
        name: &'a str,
    },
    Error {
        error: &'a AgentError,
    },
}

#[derive(Template)]
#[template(path = "oneshot_event_line.txt", escape = "none")]
struct EventLine<'a> {
    at: Timestamp,
    kind: LineKind<'a>,
}

/// Collapses `text` to one line of at most `limit` characters.
fn one_line(text: &str, limit: usize) -> String {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or_default();
    let more_lines = lines.next().is_some();
    let collapsed = first.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &collapsed[..end]),
        None if more_lines => format!("{collapsed} …"),
        None => collapsed,
    }
}

/// Names a call by its tool and arguments.
fn call_label(name: &str, arguments: &str) -> String {
    if matches!(name, "terminal" | "terminal_background")
        && let Ok(call) = serde_json::from_str::<TerminalCall>(arguments)
    {
        let description = one_line(&call.description, SUMMARY_LIMIT);
        return if description.is_empty() {
            one_line(&call.script, SUMMARY_LIMIT)
        } else {
            description
        };
    }
    one_line(arguments, SUMMARY_LIMIT)
}

/// The two streams of a one-shot run.
pub struct OneShotOutput<O, E> {
    stdout: O,
    stderr: E,
    /// Whether the last assistant text left stdout inside a line.
    stdout_open: bool,
    calls: HashMap<String, StartedCall>,
}

impl<O: Write, E: Write> OneShotOutput<O, E> {
    pub fn new(stdout: O, stderr: E) -> Self {
        Self {
            stdout,
            stderr,
            stdout_open: false,
            calls: HashMap::new(),
        }
    }

    /// Writes streamed assistant text to stdout.
    pub fn text(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.stdout.write_all(text.as_bytes())?;
        self.stdout.flush()?;
        self.stdout_open = !text.ends_with('\n');
        Ok(())
    }

    /// Ends the assistant text's line, if it is open.
    pub fn end_text_line(&mut self) -> io::Result<()> {
        if self.stdout_open {
            self.stdout.write_all(b"\n")?;
            self.stdout.flush()?;
            self.stdout_open = false;
        }
        Ok(())
    }

    /// The stderr stream, for a line written outside the event stream.
    pub fn stderr(&mut self) -> io::Result<&mut E> {
        self.end_text_line()?;
        Ok(&mut self.stderr)
    }

    fn write_line(&mut self, at: Timestamp, kind: LineKind<'_>) -> anyhow::Result<()> {
        let line = EventLine { at, kind }.render()?;
        let stderr = self.stderr()?;
        // The template renders the line without its terminator.
        stderr.write_all(line.as_bytes())?;
        stderr.write_all(b"\n")?;
        stderr.flush()?;
        Ok(())
    }

    /// Writes the line for one agent event, if it has one.
    pub fn event(&mut self, result: &Result<TimedEvent, AgentError>) -> anyhow::Result<()> {
        // Every line opens with the UTC time the agent emitted the event. A
        // run error is not an event and carries no emission time, so it is
        // stamped on receipt.
        let at = result
            .as_ref()
            .map_or_else(|_| Timestamp::now(), |timed| timed.at);
        let event = match result {
            Ok(timed) => &timed.event,
            Err(error) => return self.write_line(at, LineKind::Error { error }),
        };
        match event {
            AgentEvent::RunStart { .. } => self.write_line(at, LineKind::RunStart),
            AgentEvent::Reasoning(text) if !text.trim().is_empty() => {
                self.write_line(at, LineKind::Thinking { text })
            }
            // UI progress — oneshot mode ignores deltas.
            AgentEvent::ToolCallDelta { .. }
            | AgentEvent::TurnComplete { .. }
            | AgentEvent::Checkpoint { .. }
            | AgentEvent::Reasoning(_)
            | AgentEvent::Complete { .. }
            | AgentEvent::BuiltInToolResult { .. }
            | AgentEvent::Text(_) => Ok(()),
            AgentEvent::ToolCallStart {
                id,
                name,
                arguments,
            } => {
                let label = call_label(name, arguments);
                self.write_line(
                    at,
                    LineKind::ToolStart {
                        name,
                        label: &label,
                    },
                )?;
                self.calls.insert(id.clone(), StartedCall { at, label });
                Ok(())
            }
            AgentEvent::BuiltInToolCall { tool, detail, .. } => {
                let detail = one_line(detail, SUMMARY_LIMIT);
                self.write_line(
                    at,
                    LineKind::BuiltInTool {
                        tool,
                        detail: &detail,
                    },
                )
            }
            AgentEvent::ToolCallEnd { id, name, result } => {
                let started = self.calls.remove(id);
                let label = started.as_ref().map_or("", |call| call.label.as_str());
                let elapsed = started
                    .as_ref()
                    .map(|call| Elapsed(at.duration_since(call.at)));
                match result {
                    ToolResult::Error { message } => {
                        let message = one_line(message, MESSAGE_LIMIT);
                        self.write_line(
                            at,
                            LineKind::ToolFailed {
                                name,
                                label,
                                elapsed,
                                message: &message,
                            },
                        )
                    }
                    other => {
                        let outcome = match other.command().map(|command| &command.outcome) {
                            Some(CommandOutcome::Exited { code }) => {
                                ToolOutcome::Exited { code: *code }
                            }
                            Some(CommandOutcome::Running { task_id, .. }) => {
                                ToolOutcome::Background { task_id }
                            }
                            Some(CommandOutcome::Killed { task_id, .. }) => {
                                ToolOutcome::Killed { task_id }
                            }
                            None => ToolOutcome::Done,
                        };
                        self.write_line(
                            at,
                            LineKind::ToolFinished {
                                name,
                                label,
                                elapsed,
                                outcome,
                            },
                        )
                    }
                }
            }
            AgentEvent::BackgroundTaskStarted {
                task_id,
                output_file,
                ..
            } => self.write_line(
                at,
                LineKind::BackgroundStarted {
                    task_id,
                    output_file,
                },
            ),
            AgentEvent::BackgroundTaskCompleted { task_id, .. } => {
                self.write_line(at, LineKind::BackgroundCompleted { task_id })
            }
            AgentEvent::RunPaused { reason, .. } => self.write_line(
                at,
                LineKind::Paused {
                    reason: format!("{reason:?}"),
                },
            ),
            AgentEvent::RunResumed { reason, .. } => self.write_line(
                at,
                LineKind::Resumed {
                    reason: format!("{reason:?}"),
                },
            ),
            AgentEvent::SkillActivated { name, .. } => {
                self.write_line(at, LineKind::SkillActivated { name })
            }
            // The usage line is not templated.
            AgentEvent::Usage(usage) => {
                let stderr = self.stderr()?;
                writeln!(
                    stderr,
                    "{at} Usage: prompt={:?} completion={:?} total={:?}",
                    usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
                )?;
                stderr.flush()?;
                Ok(())
            }
            AgentEvent::Error(error) => self.write_line(at, LineKind::Error { error }),
        }
    }
}

#[cfg(test)]
impl OneShotOutput<Vec<u8>, Vec<u8>> {
    pub fn stdout_bytes(&self) -> &[u8] {
        &self.stdout
    }

    pub fn stderr_bytes(&self) -> &[u8] {
        &self.stderr
    }
}
