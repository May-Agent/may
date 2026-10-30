//! Chat stream display with inline tools and todos.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use aither_agent::ContextWindowPhase;
use aither_sandbox::TerminalExecutionMode;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::super::queue::Attachment;
use super::markdown;
use super::streaming_markdown::IMAGE_HEIGHT;
use crate::agent::ContextReport;
use crate::todo_parser::TodoChecklistItem;

/// Status of a tool call.
#[derive(Debug, Clone)]
pub enum ToolStatus {
    /// Tool call in progress.
    Running,
    /// Tool call completed.
    Done,
    /// Tool call failed with an error message.
    Failed(String),
}

/// An image produced by a tool call, rendered as an overlay when the
/// terminal supports inline graphics.
#[derive(Debug, Clone)]
pub struct ToolCallImage {
    /// Key the image state is cached under.
    pub key: String,
    /// Label used when no source path is known.
    pub label: String,
    /// Source path or URL the image came from, when known.
    pub path: String,
}

impl ToolCallImage {
    /// Caption shown under the zoomed image: the source path, falling back
    /// to the label.
    #[must_use]
    pub fn caption(&self) -> &str {
        if self.path.is_empty() {
            &self.label
        } else {
            &self.path
        }
    }
}

/// Information about a terminal command execution.
#[derive(Debug, Clone)]
pub struct TerminalCallInfo {
    /// The command that was executed.
    pub command: String,
    /// The output (may be truncated, matching what agent sees).
    pub output: String,
    /// Path to full output if truncated/offloaded.
    pub output_file: Option<PathBuf>,
    /// Image produced by this call, if any.
    pub image: Option<ToolCallImage>,
}

impl TerminalCallInfo {
    /// Create a new terminal call info.
    #[must_use]
    pub const fn new(command: String) -> Self {
        Self {
            command,
            output: String::new(),
            output_file: None,
            image: None,
        }
    }

    /// Set the output.
    #[must_use]
    pub fn with_output(mut self, output: String) -> Self {
        self.output = output;
        self
    }

    /// Set the output file.
    #[must_use]
    pub fn with_output_file(mut self, path: PathBuf) -> Self {
        self.output_file = Some(path);
        self
    }

    /// Set the image produced by this call.
    #[must_use]
    pub fn with_image(mut self, image: ToolCallImage) -> Self {
        self.image = Some(image);
        self
    }

    /// Get a truncated preview of the command for inline display (unicode-safe).
    #[must_use]
    pub fn command_preview(&self, max_chars: usize) -> &str {
        match self.command.char_indices().nth(max_chars) {
            Some((pos, _)) => &self.command[..pos],
            None => &self.command,
        }
    }

    /// Get a truncated preview of the output for inline display.
    #[must_use]
    pub fn output_preview(&self, max_lines: usize) -> String {
        let mut lines = Vec::with_capacity(max_lines);
        let mut line_iter = self.output.lines();

        for line in line_iter.by_ref().take(max_lines) {
            lines.push(line);
        }

        // Check if there are more lines
        if line_iter.next().is_some() {
            format!("{}...", lines.join("\n"))
        } else {
            lines.join("\n")
        }
    }
}

/// Display representation of a tool call.
#[derive(Debug, Clone)]
pub enum ToolDisplay {
    /// A `terminal` script execution, foreground or detached.
    Terminal {
        /// Model-provided summary shown as the headline; empty for `!`
        /// bash calls, which headline their command instead.
        description: String,
        /// Command text.
        command: String,
        /// Requested execution backend.
        mode: TerminalExecutionMode,
        /// Call status.
        status: ToolStatus,
        /// The output from this command.
        output: Option<TerminalCallInfo>,
        /// File changes attributed to this call (one per tracked root).
        diffs: Vec<crate::file_tracker::RootDiff>,
        /// Expansion pinned by a click; `None` follows the automatic rule.
        pinned: Option<bool>,
        /// When the call finished; drives automatic collapse.
        finished_at: Option<Instant>,
    },
    /// A labelled action: a terminal control call (`terminal_input`,
    /// `terminal_read`, `terminal_kill`) or any other tool invocation.
    Action {
        /// Chip label shown in brackets — `stdin`, `read`, `kill`, or the
        /// tool name.
        chip: String,
        /// Background task id the call acts on, kept only to scrub it from
        /// the result text — the id itself is never rendered.
        task_id: Option<String>,
        /// One-line user-facing summary.
        summary: String,
        /// Extra body lines rendered in the expanded body, above the output.
        detail: Vec<String>,
        /// Call status.
        status: ToolStatus,
        /// The output from this call.
        output: Option<TerminalCallInfo>,
        /// Expansion pinned by a click; `None` follows the automatic rule.
        pinned: Option<bool>,
        /// When the call finished; drives automatic collapse.
        finished_at: Option<Instant>,
    },
    /// Provider-hosted (server-side) tool call — the provider executes it,
    /// the harness only observes start and result events.
    Builtin {
        /// Hosted tool name (e.g. `web_search`).
        tool: String,
        /// Invocation detail — query, URL or code — as announced at start.
        detail: String,
        /// Call status.
        status: ToolStatus,
        /// Result summary once the provider reports it.
        output: Option<String>,
        /// Expansion pinned by a click; `None` follows the automatic rule.
        pinned: Option<bool>,
        /// When the call finished; drives automatic collapse.
        finished_at: Option<Instant>,
    },
}

/// A finished call keeps its body open this long before it auto-collapses
/// to its headline.
const RECENT_EXPAND: std::time::Duration = std::time::Duration::from_secs(15);
/// Output lines previewed inside an expanded body.
const OUTPUT_PREVIEW_LINES: usize = 3;
/// Diff lines previewed per root inside an expanded body.
const DIFF_PREVIEW_LINES: usize = 8;

/// A screen-row span (in `rendered_plain_lines` space) owned by one
/// collapsible element — a click inside toggles its expansion.
#[derive(Debug, Clone)]
pub struct ClickRegion {
    /// Collapsible element ID (tool call or thinking block).
    pub element_id: String,
    /// First covered row, inclusive.
    pub start: usize,
    /// One past the last covered row.
    pub end: usize,
}

impl ToolDisplay {
    /// Whether the call has a body worth expanding — something beyond the
    /// headline. A call without one renders its headline bare, with no
    /// expand marker and no click region.
    #[must_use]
    pub fn expandable(&self) -> bool {
        match self {
            Self::Terminal { .. } => true,
            Self::Action { detail, output, .. } => {
                !detail.is_empty() || output.as_ref().is_some_and(|info| !info.output.is_empty())
            }
            Self::Builtin { output, .. } => output.as_ref().is_some_and(|text| !text.is_empty()),
        }
    }

    /// The user-set expansion pin shared by every variant.
    #[must_use]
    const fn pinned(&self) -> Option<bool> {
        match self {
            Self::Terminal { pinned, .. }
            | Self::Action { pinned, .. }
            | Self::Builtin { pinned, .. } => *pinned,
        }
    }

    /// When the call finished, shared by every variant.
    #[must_use]
    const fn finished_at(&self) -> Option<Instant> {
        match self {
            Self::Terminal { finished_at, .. }
            | Self::Action { finished_at, .. }
            | Self::Builtin { finished_at, .. } => *finished_at,
        }
    }

    /// Whether the body renders open at `now`: a click pin wins; otherwise
    /// the body is open while the call runs and for [`RECENT_EXPAND`]
    /// after it finishes.
    #[must_use]
    pub fn body_open(&self, now: Instant) -> bool {
        self.pinned().unwrap_or_else(|| {
            matches!(self.status(), ToolStatus::Running)
                || self
                    .finished_at()
                    .is_some_and(|at| now.duration_since(at) < RECENT_EXPAND)
        })
    }

    /// Get all terminal calls for this tool.
    #[must_use]
    pub fn all_terminal_calls(&self) -> Vec<&TerminalCallInfo> {
        match self {
            Self::Terminal { output, .. } | Self::Action { output, .. } => output.iter().collect(),
            Self::Builtin { .. } => Vec::new(),
        }
    }

    /// Get status reference.
    #[must_use]
    pub const fn status(&self) -> &ToolStatus {
        match self {
            Self::Terminal { status, .. }
            | Self::Action { status, .. }
            | Self::Builtin { status, .. } => status,
        }
    }

    /// Check if this tool call is complete (not running).
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        !matches!(self.status(), ToolStatus::Running)
    }
}

/// An element in the chat stream.
#[derive(Debug, Clone)]
pub enum ChatElement {
    /// User message with optional attachments
    UserMessage {
        /// Message text.
        content: String,
        /// Attachments carried by the message.
        attachments: Vec<Attachment>,
        /// Position of the persisted user message in
        /// [`crate::session::ChatHistory::messages`]; anchors prompt-level
        /// rewind.
        history_index: usize,
    },
    /// Assistant text (streaming or complete)
    AssistantText {
        /// Message text.
        content: String,
        /// Whether tokens are still arriving.
        streaming: bool,
    },
    /// Tool call
    ToolCall {
        /// Tool call id.
        id: String,
        /// Display details for the tool.
        display: ToolDisplay,
    },
    /// Todo block
    TodoBlock {
        /// Todo items.
        items: Vec<TodoChecklistItem>,
    },
    /// Error message
    Error {
        /// Error text shown in the stream.
        message: String,
    },
    /// Neutral system notice (e.g. rewind result)
    Notice {
        /// Notice text shown in the stream.
        message: String,
    },
    /// Thinking block (for thinking models): the streamed reasoning shown
    /// live while the burst runs, then auto-collapsed behind its header.
    Thinking {
        /// Stable ID for click hit-testing.
        id: String,
        /// Reasoning text received so far.
        content: String,
        /// Seconds spent thinking.
        seconds: u64,
        /// Open while reasoning streams; click the collapsed header to reopen.
        expanded: bool,
    },
    /// `/context` report — estimated context-window usage per category.
    ContextReport(ContextReport),
}

impl ChatElement {}

/// The chat stream containing all elements.
#[derive(Debug, Default)]
pub struct ChatStream {
    elements: Vec<ChatElement>,
    /// Counter generating thinking-block IDs.
    next_thinking_id: usize,
}

impl ChatStream {
    /// Create a new empty chat stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a user message. `history_index` is the position of the
    /// persisted user row in session history.
    pub fn push_user(&mut self, content: &str, attachments: Vec<Attachment>, history_index: usize) {
        self.elements.push(ChatElement::UserMessage {
            content: content.to_string(),
            attachments,
            history_index,
        });
    }

    /// Add an error message.
    pub fn push_error(&mut self, message: &str) {
        self.elements.push(ChatElement::Error {
            message: message.to_string(),
        });
    }

    /// Add a neutral system notice.
    pub fn push_notice(&mut self, message: &str) {
        self.end_text();
        self.elements.push(ChatElement::Notice {
            message: message.to_string(),
        });
    }

    /// Add a `/context` usage report.
    pub fn push_context_report(&mut self, report: ContextReport) {
        self.end_text();
        self.elements.push(ChatElement::ContextReport(report));
    }

    /// User prompts that can be rewound to, oldest first, each carrying
    /// the file changes attributed to the turn it started (tool calls
    /// between this prompt and the next).
    #[must_use]
    pub fn rewind_targets(&self) -> Vec<crate::tui::state::RewindPrompt> {
        let mut targets: Vec<crate::tui::state::RewindPrompt> = Vec::new();
        for elem in &self.elements {
            match elem {
                ChatElement::UserMessage {
                    content,
                    attachments,
                    history_index,
                } => targets.push(crate::tui::state::RewindPrompt {
                    history_index: *history_index,
                    content: content.clone(),
                    attachments: attachments.clone(),
                    files: 0,
                    additions: 0,
                    deletions: 0,
                }),
                ChatElement::ToolCall {
                    display: ToolDisplay::Terminal { diffs, .. },
                    ..
                } => {
                    if let Some(target) = targets.last_mut() {
                        target.files += diffs.iter().map(|d| d.files).sum::<usize>();
                        target.additions += diffs.iter().map(|d| d.additions).sum::<usize>();
                        target.deletions += diffs.iter().map(|d| d.deletions).sum::<usize>();
                    }
                }
                _ => {}
            }
        }
        targets
    }

    /// Everything a prompt-level rewind needs from the stream: the
    /// prompt's own content and attachments (restored into the input for
    /// editing) plus the ids of tool calls at or after it (file-restore
    /// fallback when no prompt checkpoint exists). `None` when no user
    /// message carries `history_index`.
    #[must_use]
    pub fn prompt_rewind_payload(
        &self,
        history_index: usize,
    ) -> Option<(String, Vec<Attachment>, Vec<String>)> {
        let pos = self.elements.iter().position(|elem| {
            matches!(elem, ChatElement::UserMessage { history_index: idx, .. } if *idx == history_index)
        })?;
        let ChatElement::UserMessage {
            content,
            attachments,
            ..
        } = &self.elements[pos]
        else {
            return None;
        };
        let call_ids = self.elements[pos..]
            .iter()
            .filter_map(|elem| match elem {
                ChatElement::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        Some((content.clone(), attachments.clone(), call_ids))
    }

    /// Drop the user message at `history_index` and every element after
    /// it — the stream's half of a conversation rewind. `false` when no
    /// user message carries `history_index`.
    pub fn truncate_prompt_at(&mut self, history_index: usize) -> bool {
        let Some(pos) = self.elements.iter().position(|elem| {
            matches!(elem, ChatElement::UserMessage { history_index: idx, .. } if *idx == history_index)
        }) else {
            return false;
        };
        self.elements.truncate(pos);
        true
    }

    /// Start thinking block.
    pub fn start_thinking(&mut self) {
        let id = self.next_thinking_id;
        self.next_thinking_id += 1;
        self.elements.push(ChatElement::Thinking {
            id: format!("thinking-{id}"),
            content: String::new(),
            seconds: 0,
            expanded: true,
        });
    }

    /// Append streamed reasoning text to the open thinking block, starting a
    /// new block when the previous burst already collapsed.
    pub fn push_thinking(&mut self, chunk: &str) {
        if !matches!(
            self.elements.last(),
            Some(ChatElement::Thinking { expanded: true, .. })
        ) {
            self.start_thinking();
        }
        if let Some(ChatElement::Thinking { content, .. }) = self.elements.last_mut() {
            content.push_str(chunk);
        }
    }

    /// Update thinking time.
    pub fn update_thinking(&mut self, seconds: u64) {
        if let Some(ChatElement::Thinking { seconds: s, .. }) = self.elements.last_mut() {
            *s = seconds;
        }
    }

    /// Collapse the open thinking block (called when the reasoning burst
    /// ends or first text arrives). The block stays as a collapsed header.
    pub fn collapse_thinking(&mut self) {
        if let Some(ChatElement::Thinking { expanded, .. }) = self.elements.last_mut() {
            *expanded = false;
        }
    }

    /// Start a new assistant text block.
    pub fn start_assistant(&mut self) {
        self.elements.push(ChatElement::AssistantText {
            content: String::new(),
            streaming: true,
        });
    }

    /// Append text to the current streaming element.
    pub fn push_text(&mut self, chunk: &str) {
        if let Some(ChatElement::AssistantText {
            content,
            streaming: true,
        }) = self.elements.last_mut()
        {
            content.push_str(chunk);
        } else {
            // Create new streaming element
            self.elements.push(ChatElement::AssistantText {
                content: chunk.to_string(),
                streaming: true,
            });
        }
    }

    /// Mark the current text element as done streaming.
    pub fn end_text(&mut self) {
        if let Some(ChatElement::AssistantText { streaming, .. }) = self.elements.last_mut() {
            *streaming = false;
        }
    }

    /// Add a tool call.
    pub fn push_tool(&mut self, id: String, display: ToolDisplay) {
        // End any current text block first
        self.end_text();
        self.elements.push(ChatElement::ToolCall { id, display });
    }

    /// Record a provider-hosted tool call the model just invoked.
    pub fn push_builtin(&mut self, id: String, tool: String, detail: String) {
        self.end_text();
        self.elements.push(ChatElement::ToolCall {
            id,
            display: ToolDisplay::Builtin {
                tool,
                detail,
                status: ToolStatus::Running,
                output: None,
                pinned: None,
                finished_at: None,
            },
        });
    }

    /// Mark a provider-hosted call complete. Providers that emit a result
    /// without a matching call event get a completed entry appended instead.
    pub fn finish_builtin(&mut self, id: &str, tool: &str, result: String) {
        for elem in &mut self.elements {
            if let ChatElement::ToolCall {
                id: tool_id,
                display:
                    ToolDisplay::Builtin {
                        status,
                        output,
                        finished_at,
                        ..
                    },
            } = elem
                && tool_id == id
            {
                *status = ToolStatus::Done;
                *output = Some(result);
                *finished_at = Some(Instant::now());
                return;
            }
        }
        self.end_text();
        self.elements.push(ChatElement::ToolCall {
            id: id.to_string(),
            display: ToolDisplay::Builtin {
                tool: tool.to_string(),
                detail: String::new(),
                status: ToolStatus::Done,
                output: Some(result),
                pinned: None,
                finished_at: Some(Instant::now()),
            },
        });
    }

    /// Update a tool's status and output by ID. A non-`Running` status stamps
    /// the call's finish time, which the automatic collapse counts from.
    pub fn update_tool_with_output(&mut self, id: &str, status: ToolStatus, output: Option<&str>) {
        for elem in &mut self.elements {
            if let ChatElement::ToolCall {
                id: tool_id,
                display,
            } = elem
                && tool_id == id
            {
                let finished = !matches!(status, ToolStatus::Running);
                match display {
                    ToolDisplay::Terminal {
                        status: s,
                        command,
                        output: out,
                        finished_at,
                        ..
                    } => {
                        *s = status;
                        if finished {
                            *finished_at = Some(Instant::now());
                        }
                        if let Some(output_text) = output {
                            *out = Some(TerminalCallInfo {
                                command: command.clone(),
                                output: output_text.to_string(),
                                output_file: None,
                                image: None,
                            });
                        }
                    }
                    ToolDisplay::Action {
                        status: s,
                        chip,
                        task_id,
                        output: out,
                        finished_at,
                        ..
                    } => {
                        *s = status;
                        if finished {
                            *finished_at = Some(Instant::now());
                        }
                        if let Some(output_text) = output {
                            *out = Some(TerminalCallInfo {
                                command: chip.clone(),
                                output: task_id.as_ref().map_or_else(
                                    || output_text.to_string(),
                                    |id| scrub_task_id(output_text, id),
                                ),
                                output_file: None,
                                image: None,
                            });
                        }
                    }
                    ToolDisplay::Builtin {
                        status: s,
                        output: out,
                        finished_at,
                        ..
                    } => {
                        *s = status;
                        if finished {
                            *finished_at = Some(Instant::now());
                        }
                        if let Some(output_text) = output {
                            *out = Some(output_text.to_string());
                        }
                    }
                }
                break;
            }
        }
    }

    /// Flip the expansion of a collapsible element (tool call or thinking
    /// block). For tool calls this sets the click pin: `Some(true)` pins the
    /// body open, `Some(false)` pins it shut, overriding the automatic rule.
    pub fn toggle_expanded(&mut self, id: &str) {
        for elem in &mut self.elements {
            match elem {
                ChatElement::ToolCall {
                    id: tool_id,
                    display,
                } if tool_id == id && display.expandable() => {
                    let open = display.body_open(Instant::now());
                    match display {
                        ToolDisplay::Terminal { pinned, .. }
                        | ToolDisplay::Action { pinned, .. }
                        | ToolDisplay::Builtin { pinned, .. } => *pinned = Some(!open),
                    }
                    break;
                }
                ChatElement::Thinking {
                    id: think_id,
                    expanded,
                    ..
                } if think_id == id => {
                    *expanded = !*expanded;
                    break;
                }
                _ => {}
            }
        }
    }

    /// Attach file diffs to a terminal tool call.
    pub fn update_tool_diff(&mut self, id: &str, diffs: Vec<crate::file_tracker::RootDiff>) {
        for elem in &mut self.elements {
            if let ChatElement::ToolCall {
                id: tool_id,
                display: ToolDisplay::Terminal { diffs: slot, .. },
            } = elem
                && tool_id == id
            {
                *slot = diffs;
                break;
            }
        }
    }

    /// Whether the given tool call is still running.
    #[must_use]
    pub fn is_tool_running(&self, id: &str) -> bool {
        self.elements.iter().any(|elem| {
            matches!(elem, ChatElement::ToolCall { id: tool_id, display }
                if tool_id == id && matches!(display.status(), ToolStatus::Running))
        })
    }

    /// Running `terminal`-displayed calls as `(call id, command)` pairs —
    /// the callers that can be bound to a job-registry entry for live output.
    #[must_use]
    pub fn running_terminal_calls(&self) -> Vec<(String, String)> {
        self.elements
            .iter()
            .filter_map(|elem| {
                let ChatElement::ToolCall {
                    id,
                    display:
                        ToolDisplay::Terminal {
                            command,
                            status: ToolStatus::Running,
                            ..
                        },
                } = elem
                else {
                    return None;
                };
                Some((id.clone(), command.clone()))
            })
            .collect()
    }

    /// Attach an image to a tool call's terminal info (the latest call for
    /// subagent tasks).
    pub fn set_tool_image(&mut self, id: &str, image: ToolCallImage) {
        for elem in &mut self.elements {
            if let ChatElement::ToolCall {
                id: tool_id,
                display,
            } = elem
                && tool_id == id
            {
                match display {
                    ToolDisplay::Terminal { output, .. } | ToolDisplay::Action { output, .. } => {
                        if let Some(info) = output {
                            info.image = Some(image);
                        }
                    }
                    // Provider-hosted calls carry no terminal info to attach.
                    ToolDisplay::Builtin { .. } => {}
                }
                break;
            }
        }
    }

    /// Append live output to a running terminal call's buffer. Creates the
    /// `TerminalCallInfo` on first chunk; the running renderer already shows
    /// `output` mid-run.
    pub fn append_tool_output(&mut self, id: &str, chunk: &str) {
        for elem in &mut self.elements {
            if let ChatElement::ToolCall {
                id: tool_id,
                display:
                    ToolDisplay::Terminal {
                        command, output, ..
                    },
            } = elem
                && tool_id == id
            {
                match output {
                    Some(info) => info.output.push_str(chunk),
                    None => {
                        *output = Some(
                            TerminalCallInfo::new(command.clone()).with_output(chunk.to_string()),
                        );
                    }
                }
                break;
            }
        }
    }

    /// Set the todo block (replaces previous).
    pub fn set_todos(&mut self, items: Vec<TodoChecklistItem>) {
        // Remove existing todo block
        self.elements
            .retain(|e| !matches!(e, ChatElement::TodoBlock { .. }));
        // Add new one at end
        if !items.is_empty() {
            self.elements.push(ChatElement::TodoBlock { items });
        }
    }

    /// Get all elements.
    #[must_use]
    pub fn elements(&self) -> &[ChatElement] {
        &self.elements
    }

    /// Get all tool calls with their indices.
    #[must_use]
    pub fn tool_calls(&self) -> Vec<(usize, &str, &ToolDisplay)> {
        self.elements
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                if let ChatElement::ToolCall { id, display } = e {
                    Some((i, id.as_str(), display))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Get a tool call by ID.
    #[must_use]
    pub fn get_tool(&self, id: &str) -> Option<&ToolDisplay> {
        self.elements.iter().find_map(|e| {
            if let ChatElement::ToolCall {
                id: tool_id,
                display,
            } = e
            {
                if tool_id == id { Some(display) } else { None }
            } else {
                None
            }
        })
    }

    /// Check if currently streaming.
    #[must_use]
    pub fn is_streaming(&self) -> bool {
        matches!(
            self.elements.last(),
            Some(ChatElement::AssistantText {
                streaming: true,
                ..
            })
        )
    }

    /// Get all active elements.
    #[must_use]
    pub fn active_elements(&self) -> &[ChatElement] {
        &self.elements
    }
}

/// Input rendering context for combined view.
#[derive(Debug)]
pub struct InputContext<'a> {
    /// Current input text.
    pub input: &'a str,
    /// Cursor position (character index).
    pub cursor: usize,
    /// Pending attachments.
    pub attachments: &'a [Attachment],
}

/// One running background entry shown compactly in the status bar.
#[derive(Debug)]
pub struct StatusBackgroundItem {
    /// Short label — subagent name or first command line.
    pub label: String,
    /// `true` for a `subagent` run, `false` for a terminal job.
    pub subagent: bool,
    /// Seconds since it started.
    pub elapsed_secs: u64,
}

/// What the run is doing right now, for the activity line above the
/// status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityPhase {
    /// The model is reasoning or the run is waiting on its next output.
    Thinking,
    /// The model is producing output or a tool is executing.
    Working,
}

impl ActivityPhase {
    /// Lowercase label shown before the spinner.
    const fn label(self) -> &'static str {
        match self {
            Self::Thinking => "thinking",
            Self::Working => "working",
        }
    }
}

/// Live activity indicator rendered on the line above the status bar.
#[derive(Debug, Clone, Copy)]
pub struct ActivityContext {
    /// `thinking` while reasoning/waiting, `working` while producing.
    pub phase: ActivityPhase,
    /// Output tokens produced so far this turn.
    pub tokens: u64,
}

/// Status bar rendering context for combined view.
#[derive(Debug)]
pub struct StatusContext<'a> {
    /// Model display string.
    pub model: &'a str,
    /// Token usage info (`prompt_tokens`, `context_window`).
    pub usage: Option<(u64, Option<u64>)>,
    /// Queued message count.
    pub queue_count: usize,
    /// Questions the agent asked that wait for an answer.
    pub open_questions: usize,
    /// Running background entries (subagents and jobs).
    pub background: &'a [StatusBackgroundItem],
    /// Live decode rate in tokens per second, when measurable.
    pub decode_rate: Option<f64>,
    /// Optional status message.
    pub status_message: Option<&'a str>,
    /// Current working directory shown in the status bar (`~` for home).
    pub cwd: &'a str,
    /// Live activity indicator for the line above the status bar; `None`
    /// when no run is in flight.
    pub activity: Option<ActivityContext>,
    /// Animation frame counter for the activity spinner.
    pub spinner_frame: usize,
}

/// Queued message context for rendering queue indicator.
#[derive(Debug)]
pub struct QueueContext<'a> {
    /// Queued message previews.
    pub messages: &'a [String],
}

/// Render the chat stream.
pub fn render(frame: &mut Frame, area: Rect, chat: &ChatStream, scroll: usize) {
    render_impl(frame, area, chat, scroll, RenderContext::default());
}

/// Full-screen unified render context.
#[derive(Debug)]
pub struct UnifiedRenderContext<'a> {
    /// Tool id currently selected for copy/scroll.
    pub selected_tool: Option<&'a str>,
    /// Receives the rendered content height for scroll bookkeeping.
    pub content_height_out: Option<&'a AtomicUsize>,
    /// Receives the rendered view height for scroll bookkeeping.
    pub view_height_out: Option<&'a AtomicUsize>,
    /// Input line rendered below chat content, when present.
    pub input_ctx: Option<InputContext<'a>>,
    /// Whether the input caret owns the terminal cursor (IME anchor).
    /// Only set when the composer actually receives keystrokes.
    pub input_caret_active: bool,
    /// Queue indicator context, when messages are queued.
    pub queue_ctx: Option<QueueContext<'a>>,
    /// Status bar context, when the bar is shown.
    pub status_ctx: Option<StatusContext<'a>>,
    /// Whether terminal supports image rendering
    pub images_supported: bool,
    /// Output: accumulated image positions (url, `line_index`, caption) for overlay rendering
    pub image_positions_out: Option<&'a mut Vec<(String, usize, String)>>,
}

impl<'a> From<UnifiedRenderContext<'a>> for RenderContext<'a> {
    fn from(ctx: UnifiedRenderContext<'a>) -> Self {
        Self {
            selected_tool: ctx.selected_tool,
            content_height_out: ctx.content_height_out,
            view_height_out: ctx.view_height_out,
            input_ctx: ctx.input_ctx,
            input_caret_active: ctx.input_caret_active,
            queue_ctx: ctx.queue_ctx,
            status_ctx: ctx.status_ctx,
            images_supported: ctx.images_supported,
            image_positions_out: ctx.image_positions_out,
        }
    }
}

/// Render the chat stream with full-screen scrolling (chat + queue + input + status).
pub fn render_unified(
    frame: &mut Frame,
    area: Rect,
    chat: &ChatStream,
    scroll: usize,
    ctx: UnifiedRenderContext<'_>,
) -> crate::tui::perf::ChatRenderStats {
    render_impl(frame, area, chat, scroll, ctx.into())
}

/// Build plain wrapped lines for selection/copy from unified viewport
/// content. When `regions_out` is given, it also reports the row span of
/// every collapsible tool call for click hit-testing.
#[must_use]
pub fn build_plain_wrapped_lines_unified(
    chat: &ChatStream,
    width: u16,
    queue_ctx: Option<&QueueContext<'_>>,
    input_ctx: Option<InputContext<'_>>,
    status_ctx: Option<&StatusContext<'_>>,
    regions_out: Option<&mut Vec<ClickRegion>>,
    images_supported: bool,
) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let max_width = width as usize;
    let mut out: Vec<String> = Vec::new();
    let mut regions = Vec::new();

    for elem in chat.active_elements() {
        let start = out.len();
        for line in plain_lines_for_element(elem, images_supported) {
            out.append(&mut wrap_plain_line(&plain_text(&line), max_width));
        }
        let collapsible_id = match elem {
            ChatElement::ToolCall { id, display } if display.expandable() => Some(id.clone()),
            ChatElement::Thinking {
                id,
                expanded: false,
                ..
            } => Some(id.clone()),
            _ => None,
        };
        if let Some(element_id) = collapsible_id {
            regions.push(ClickRegion {
                element_id,
                start,
                end: out.len(),
            });
        }
    }

    let mut tail: Vec<Line<'static>> = Vec::new();
    if let Some(ctx) = queue_ctx
        && !ctx.messages.is_empty()
    {
        render_queue_lines(&mut tail, ctx, width);
    }
    if let Some(ctx) = input_ctx {
        render_input_line(&mut tail, &ctx, images_supported, None);
    }
    if let Some(ctx) = status_ctx {
        render_status_line(&mut tail, ctx, max_width);
    }
    for line in tail {
        out.append(&mut wrap_plain_line(&plain_text(&line), max_width));
    }

    if let Some(slot) = regions_out {
        *slot = regions;
    }
    out
}

/// Plain text of a styled line (span contents concatenated).
fn plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>()
}

/// Push `IMAGE_HEIGHT` blank rows reserved for the graphics overlay.
/// Returns the first reserved row index, which the overlay uses as its anchor.
fn push_image_rows(lines: &mut Vec<Line<'_>>) -> usize {
    let index = lines.len();
    for _ in 0..IMAGE_HEIGHT {
        lines.push(Line::from(""));
    }
    index
}

/// One-line text marker for an attachment, matching the historical markers.
fn attachment_label(attachment: &Attachment) -> String {
    match attachment {
        Attachment::Image { .. } => "[image]".to_string(),
        Attachment::FileReference { path } => format!("@{}", path.display()),
        Attachment::PastedText {
            content,
            saved_path,
        } => {
            let char_count = content.chars().count();
            saved_path.as_ref().map_or_else(
                || format!("[pasted text: {char_count} chars]"),
                |path| format!("[pasted text: {char_count} chars -> {}]", path.display()),
            )
        }
    }
}

/// Push the display rows for one attachment. An image attachment becomes a
/// placeholder block (registered in `positions` for the overlay) when the
/// terminal supports graphics, otherwise its one-line marker.
fn push_attachment_line(
    lines: &mut Vec<Line<'_>>,
    attachment: &Attachment,
    images_supported: bool,
    positions: Option<&mut Vec<(String, usize, String)>>,
) {
    if images_supported && let Attachment::Image { path, .. } = attachment {
        let index = push_image_rows(lines);
        if let Some(key) = attachment.image_cache_key()
            && let Some(positions) = positions
        {
            positions.push((key, index, path.display().to_string()));
        }
        return;
    }
    lines.push(Line::from(Span::styled(
        attachment_label(attachment),
        Style::default().fg(Color::DarkGray),
    )));
}

/// Push the display rows for an image produced by a tool call. Emits the
/// placeholder block only when the terminal supports graphics — the text
/// preview already carries the `[image]` marker otherwise.
fn push_tool_call_image(
    lines: &mut Vec<Line<'_>>,
    info: Option<&TerminalCallInfo>,
    images_supported: bool,
    positions: &mut Vec<(String, usize, String)>,
) {
    if !images_supported {
        return;
    }
    if let Some(image) = info.and_then(|info| info.image.as_ref()) {
        let index = push_image_rows(lines);
        positions.push((image.key.clone(), index, image.caption().to_string()));
    }
}

/// Styled lines for a `/context` report block, shared by the rendered
/// chat and its plain-text mirror so row counts stay aligned.
fn context_report_lines(report: &ContextReport) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let value_style = Style::default().fg(Color::Cyan);

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            "◆ Context usage",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  ·  {} ({})", report.model_id, report.provider_id),
            dim,
        ),
    ]));
    let percent = if report.window_tokens > 0 {
        report.used_tokens as f64 / report.window_tokens as f64 * 100.0
    } else {
        0.0
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!(
                "  {}/{}",
                format_number(report.used_tokens),
                format_number(report.window_tokens)
            ),
            value_style.add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" tokens ({percent:.0}%, estimated)"), dim),
    ]));
    lines.push(Line::from(""));

    let label_width = report
        .categories
        .iter()
        .map(|row| row.kind.label().len())
        .max()
        .unwrap_or_default()
        .max("Autocompact".len());
    for row in &report.categories {
        let mut spans = vec![
            Span::raw(format!("  {:<label_width$}", row.kind.label())),
            Span::styled(format!("{:>7}", format_number(row.tokens)), value_style),
        ];
        if let Some(detail) = &row.detail {
            spans.push(Span::styled(format!("   {detail}"), dim));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw(format!("  {:<label_width$}", "Free space")),
        Span::styled(
            format!("{:>7}", format_number(report.free_tokens)),
            value_style,
        ),
    ]));
    lines.push(Line::from(vec![
        Span::raw(format!("  {:<label_width$}", "Autocompact")),
        Span::styled(
            format!("{:>7}", format_number(report.autocompact_buffer_tokens)),
            value_style,
        ),
        Span::styled(
            format!(
                "   reserved above {:.0}%",
                report.autocompact_trigger * 100.0
            ),
            dim,
        ),
    ]));
    if report.phase != ContextWindowPhase::Stable {
        lines.push(Line::from(vec![
            Span::styled("  phase: ", dim),
            Span::styled(
                format!("{:?}", report.phase),
                Style::default().fg(Color::Yellow),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines
}

/// Plain-text mirror of one chat element's styled rendering. Row count
/// must track the styled output so selection rows and click regions
/// align with what is on screen.
fn plain_lines_for_element(elem: &ChatElement, images_supported: bool) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    match elem {
        ChatElement::UserMessage {
            content,
            attachments,
            ..
        } => {
            for (i, line) in content.lines().enumerate() {
                if i == 0 {
                    lines.push(Line::from(format!("> {line}")));
                } else {
                    lines.push(Line::from(format!("  {line}")));
                }
            }
            for att in attachments {
                push_attachment_line(&mut lines, att, images_supported, None);
            }
            lines.push(Line::from(""));
        }
        ChatElement::AssistantText { content, .. } => {
            let image_mode = if images_supported {
                markdown::ImageSupport::Enabled
            } else {
                markdown::ImageSupport::Disabled
            };
            let mut md_lines = markdown::render_markdown_with_config(content, image_mode);
            while md_lines.last().is_some_and(|l| l.spans.is_empty()) {
                md_lines.pop();
            }
            lines.extend(md_lines);
            lines.push(Line::from(""));
        }
        ChatElement::Error { message } => {
            lines.push(Line::from(format!("✗ {message}")));
            lines.push(Line::from(""));
        }
        ChatElement::ToolCall { display, .. } => {
            plain_tool_lines(display, &mut lines, images_supported);
        }
        ChatElement::TodoBlock { items } => {
            lines.push(Line::from("[todo] ────────────────────────────"));
            for item in items {
                let marker = if item.checked { "[x]" } else { "[ ]" };
                lines.push(Line::from(format!("  {marker} {}", item.text)));
            }
            lines.push(Line::from("─────────────────────────────────────"));
            lines.push(Line::from(""));
        }
        ChatElement::Notice { message } => {
            lines.push(Line::from(message.clone()));
            lines.push(Line::from(""));
        }
        ChatElement::ContextReport(report) => {
            lines.extend(context_report_lines(report));
        }
        ChatElement::Thinking {
            content,
            seconds,
            expanded,
            ..
        } => {
            if *expanded {
                let text = if *seconds == 0 {
                    "◐ Thinking...".to_string()
                } else {
                    format!("◐ Thinking... ({seconds} sec)")
                };
                lines.push(Line::from(text));
                for l in content.lines() {
                    lines.push(Line::from(l.to_string()));
                }
            } else {
                let text = if *seconds == 0 {
                    "◐ Thought ▸".to_string()
                } else {
                    format!("◐ Thought for {seconds} sec ▸")
                };
                lines.push(Line::from(text));
            }
            lines.push(Line::from(""));
        }
    }
    lines
}

/// The one-line headline a call renders: `▸`/`▾`-suffixed description,
/// chip + summary, or the command itself when no description exists.
fn tool_headline(display: &ToolDisplay) -> String {
    match display {
        ToolDisplay::Terminal {
            description,
            command,
            mode,
            ..
        } => {
            if description.is_empty() {
                format!(
                    "{}$ {}",
                    terminal_mode_marker(*mode),
                    command.lines().next().unwrap_or_default()
                )
            } else {
                description.clone()
            }
        }
        ToolDisplay::Action { chip, summary, .. } => {
            let (label, rest) = action_headline(chip, summary);
            format!("{label} {rest}").trim_end().to_string()
        }
        ToolDisplay::Builtin { tool, detail, .. } => {
            format!("[{tool}] {detail}").trim_end().to_string()
        }
    }
}

/// Remove a background task id from a tool's result text, replacing its
/// quoted and bare occurrences with a readable referent.
fn scrub_task_id(text: &str, task_id: &str) -> String {
    text.replace(&format!("task '{task_id}'"), "task")
        .replace(&format!("Task '{task_id}'"), "Task")
        .replace(&format!("task_id={task_id}"), "task")
        .replace(task_id, "the task")
}

/// Status shown inside an expanded body (`↳ running`), or `None` when the
/// call finished cleanly.
fn body_status_text(status: &ToolStatus) -> Option<String> {
    match status {
        ToolStatus::Running => Some("running".to_string()),
        ToolStatus::Done => None,
        ToolStatus::Failed(e) => Some(format!("failed: {e}")),
    }
}

/// Status suffix appended to a collapsed headline — only when the state is
/// worth surfacing while folded.
fn collapsed_status_suffix(display: &ToolDisplay, open: bool) -> String {
    if open {
        return String::new();
    }
    match display.status() {
        ToolStatus::Running => " — running".to_string(),
        ToolStatus::Done => String::new(),
        ToolStatus::Failed(e) => format!(" — failed: {e}"),
    }
}

/// Plain-text mirror of `render_tool_inline` for one tool call.
fn plain_tool_lines(display: &ToolDisplay, lines: &mut Vec<Line<'static>>, images_supported: bool) {
    let expandable = display.expandable();
    let open = expandable && display.body_open(Instant::now());
    let marker = if !expandable {
        "  "
    } else if open {
        "▾ "
    } else {
        "▸ "
    };
    lines.push(Line::from(format!(
        "{marker}{}{}",
        tool_headline(display),
        collapsed_status_suffix(display, open)
    )));
    if !open {
        return;
    }
    match display {
        ToolDisplay::Terminal {
            command,
            mode,
            status,
            output,
            diffs,
            ..
        } => {
            let mode_prefix = terminal_mode_marker(*mode);
            let mut command_lines = command.lines();
            if let Some(first) = command_lines.next() {
                lines.push(Line::from(format!("{mode_prefix}$ {first}")));
                for line in command_lines {
                    lines.push(Line::from(format!("│ {line}")));
                }
            } else {
                lines.push(Line::from(format!("{mode_prefix}$")));
            }
            if let Some(status_text) = body_status_text(status) {
                lines.push(Line::from(format!("↳ {status_text}")));
            }
            if let Some(info) = output
                && !info.output.is_empty()
            {
                for line in info.output_preview(OUTPUT_PREVIEW_LINES).lines() {
                    lines.push(Line::from(format!("│ {line}")));
                }
            }
            if images_supported
                && let Some(_) = output.as_ref().and_then(|info| info.image.as_ref())
            {
                push_image_rows(lines);
            }
            plain_diff_lines(diffs, lines);
        }
        ToolDisplay::Action {
            detail,
            status,
            output,
            ..
        } => {
            for line in detail.iter().take(DIFF_PREVIEW_LINES) {
                lines.push(Line::from(format!("│ {line}")));
            }
            if detail.len() > DIFF_PREVIEW_LINES {
                lines.push(Line::from(format!(
                    "│ … {} more lines",
                    detail.len() - DIFF_PREVIEW_LINES
                )));
            }
            if let Some(status_text) = body_status_text(status) {
                lines.push(Line::from(format!("↳ {status_text}")));
            }
            if let Some(info) = output
                && !info.output.is_empty()
            {
                for line in info.output_preview(OUTPUT_PREVIEW_LINES).lines() {
                    lines.push(Line::from(format!("│ {line}")));
                }
            }
            if images_supported
                && output
                    .as_ref()
                    .and_then(|info| info.image.as_ref())
                    .is_some()
            {
                push_image_rows(lines);
            }
        }
        ToolDisplay::Builtin { status, output, .. } => {
            if let Some(status_text) = body_status_text(status) {
                lines.push(Line::from(format!("↳ {status_text}")));
            }
            if let Some(text) = output {
                for line in text.lines().take(OUTPUT_PREVIEW_LINES) {
                    lines.push(Line::from(format!("│ {line}")));
                }
            }
        }
    }
}

/// Plain-text mirror of `render_diff_preview`.
fn plain_diff_lines(diffs: &[crate::file_tracker::RootDiff], lines: &mut Vec<Line<'static>>) {
    for (index, diff) in diffs.iter().enumerate() {
        let summary = format!(
            "± {} file{} (+{} −{})",
            diff.files,
            if diff.files == 1 { "" } else { "s" },
            diff.additions,
            diff.deletions,
        );
        if diffs.len() > 1 {
            lines.push(Line::from(format!("{summary} in {}", diff.root.display())));
        } else {
            lines.push(Line::from(summary));
        }
        let count = diff.text.lines().count();
        for line in diff.text.lines().take(DIFF_PREVIEW_LINES) {
            lines.push(Line::from(format!("│ {line}")));
        }
        if count > DIFF_PREVIEW_LINES {
            lines.push(Line::from(format!(
                "│ … {} more diff lines",
                count - DIFF_PREVIEW_LINES
            )));
        }
        if index + 1 < diffs.len() {
            lines.push(Line::from(""));
        }
    }
}

fn wrap_plain_line(line: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![String::new()];
    }
    if line.is_empty() {
        return vec![String::new()];
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;

    for ch in line.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
        if current_w + w > max_width && !current.is_empty() {
            out.push(current);
            current = String::new();
            current_w = 0;
        }
        current.push(ch);
        current_w += w;
    }

    out.push(current);
    out
}

/// Clamp a visual column in `line` to `target_col`, counting wide characters.
#[must_use]
pub fn clamp_column_by_width(line: &str, target_col: usize) -> usize {
    let mut col = 0usize;
    let mut out = 0usize;
    for ch in line.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(1);
        if col + w > target_col {
            break;
        }
        col += w;
        out += 1;
    }
    out
}

/// Safely clamp scroll value for u16 and content bounds.
fn safe_scroll(scroll: usize, content_height: usize, area_height: u16) -> u16 {
    // Don't allow scroll beyond content
    let max_scroll = content_height.saturating_sub(area_height as usize);
    let clamped = scroll.min(max_scroll);
    // Clamp to u16::MAX to prevent overflow
    u16::try_from(clamped).unwrap_or(u16::MAX)
}

/// Calculate actual visual row count using ratatui's own `Paragraph::line_count`.
/// This is the only way to get an exact match with how Paragraph renders wrapped text,
/// especially for CJK wide characters and word-boundary wrapping.
fn total_wrapped_lines(lines: &[Line], width: usize) -> usize {
    if width == 0 || lines.is_empty() {
        return 0;
    }
    let w = u16::try_from(width).unwrap_or(u16::MAX);
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(w)
}

/// Internal render context with all optional parameters.
#[derive(Default)]
struct RenderContext<'a> {
    selected_tool: Option<&'a str>,
    content_height_out: Option<&'a AtomicUsize>,
    view_height_out: Option<&'a AtomicUsize>,
    input_ctx: Option<InputContext<'a>>,
    input_caret_active: bool,
    queue_ctx: Option<QueueContext<'a>>,
    status_ctx: Option<StatusContext<'a>>,
    images_supported: bool,
    image_positions_out: Option<&'a mut Vec<(String, usize, String)>>,
}

/// Internal implementation for rendering chat stream.
fn render_impl(
    frame: &mut Frame,
    area: Rect,
    chat: &ChatStream,
    scroll: usize,
    ctx: RenderContext<'_>,
) -> crate::tui::perf::ChatRenderStats {
    let mut stats = crate::tui::perf::ChatRenderStats::default();
    // Guard against zero-dimension areas to prevent overflow
    if area.width < 3 || area.height < 3 {
        return stats;
    }

    let RenderContext {
        selected_tool,
        content_height_out,
        view_height_out,
        input_ctx,
        input_caret_active,
        queue_ctx,
        status_ctx,
        images_supported,
        image_positions_out,
    } = ctx;

    let mut lines: Vec<Line> = Vec::new();
    let mut collected_image_positions: Vec<(String, usize, String)> = Vec::new();

    let build_start = std::time::Instant::now();
    for elem in chat.active_elements() {
        stats.elements += 1;
        match elem {
            ChatElement::UserMessage {
                content,
                attachments,
                ..
            } => {
                // User message with > prefix
                for (i, line) in content.lines().enumerate() {
                    if i == 0 {
                        lines.push(Line::from(vec![
                            Span::styled("> ", Style::default().fg(Color::Cyan)),
                            Span::raw(line.to_string()),
                        ]));
                    } else {
                        lines.push(Line::from(format!("  {line}")));
                    }
                }

                // Attachments — images get a placeholder block the graphics
                // overlay paints into; everything else is a one-line marker.
                for att in attachments {
                    push_attachment_line(
                        &mut lines,
                        att,
                        images_supported,
                        Some(&mut collected_image_positions),
                    );
                }

                lines.push(Line::from(""));
            }

            ChatElement::AssistantText { content, streaming } => {
                // Track current line offset before adding markdown
                let line_offset = lines.len();

                // Render markdown with position tracking
                let image_mode = if images_supported {
                    markdown::ImageSupport::Enabled
                } else {
                    markdown::ImageSupport::Disabled
                };
                let md_result = markdown::render_markdown_with_positions(content, image_mode);
                let mut md_lines = md_result.lines;
                // Strip accidental trailing empties from markdown renderer so we don't inflate
                // content height and trigger premature scrollback insertion.
                while md_lines.last().is_some_and(|l| l.spans.is_empty()) {
                    md_lines.pop();
                }

                // Collect image positions with offset adjustment
                for (url, pos, caption) in md_result.image_positions {
                    collected_image_positions.push((url, line_offset + pos, caption));
                }

                if *streaming {
                    // Append cursor to the last line of text (inline)
                    if let Some(last_line) = md_lines.last_mut() {
                        last_line
                            .spans
                            .push(Span::styled("_", Style::default().fg(Color::DarkGray)));
                    } else {
                        // No content yet, show cursor on its own line
                        md_lines.push(Line::from(Span::styled(
                            "_",
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }

                lines.extend(md_lines);
                lines.push(Line::from(""));
            }

            ChatElement::ToolCall { id, display } => {
                let is_selected = selected_tool == Some(id.as_str());
                render_tool_inline(
                    &mut lines,
                    display,
                    is_selected,
                    images_supported,
                    &mut collected_image_positions,
                );
            }

            ChatElement::TodoBlock { items } => {
                lines.push(Line::from(Span::styled(
                    "[todo] ────────────────────────────",
                    Style::default().fg(Color::Green),
                )));

                for item in items {
                    let (marker, style) = if item.checked {
                        ("[x]", Style::default().fg(Color::DarkGray))
                    } else {
                        ("[ ]", Style::default())
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {marker} "), style),
                        Span::styled(&item.text, style),
                    ]));
                }

                lines.push(Line::from(Span::styled(
                    "─────────────────────────────────────",
                    Style::default().fg(Color::Green),
                )));
                lines.push(Line::from(""));
            }

            ChatElement::Error { message } => {
                // Error message with icon and red color
                lines.push(Line::from(vec![
                    Span::styled("✗ ", Style::default().fg(Color::Red)),
                    Span::styled(message, Style::default().fg(Color::Red)),
                ]));
                lines.push(Line::from(""));
            }

            ChatElement::Notice { message } => {
                lines.push(Line::from(vec![
                    Span::styled("◈ ", Style::default().fg(Color::Cyan)),
                    Span::styled(message, Style::default().fg(Color::DarkGray)),
                ]));
                lines.push(Line::from(""));
            }

            ChatElement::ContextReport(report) => {
                lines.extend(context_report_lines(report));
            }

            ChatElement::Thinking {
                content,
                seconds,
                expanded,
                ..
            } => {
                let header = if *expanded {
                    if *seconds == 0 {
                        "◐ Thinking...".to_string()
                    } else {
                        format!("◐ Thinking... ({seconds} sec)")
                    }
                } else if *seconds == 0 {
                    "◐ Thought ▸".to_string()
                } else {
                    format!("◐ Thought for {seconds} sec ▸")
                };
                lines.push(Line::from(Span::styled(
                    header,
                    Style::default().fg(Color::Magenta),
                )));
                if *expanded {
                    for l in content.lines() {
                        lines.push(Line::from(Span::styled(
                            l.to_string(),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        )));
                    }
                }
                lines.push(Line::from(""));
            }
        }
    }

    // Render queue messages if provided
    if let Some(ref ctx) = queue_ctx
        && !ctx.messages.is_empty()
    {
        render_queue_lines(&mut lines, ctx, area.width);
    }

    // Render input line if provided (for full-screen scroll mode)
    let mut input_line_idx = None;
    let mut caret_line = None;
    if let Some(ctx) = input_ctx {
        if input_caret_active {
            caret_line = Some(input_caret_line(&ctx));
        }
        input_line_idx = Some(render_input_line(
            &mut lines,
            &ctx,
            images_supported,
            Some(&mut collected_image_positions),
        ));
    }

    // Render status bar if provided
    if let Some(ref ctx) = status_ctx {
        render_status_line(&mut lines, ctx, area.width as usize);
    }

    stats.lines_build = build_start.elapsed();

    // Calculate dimensions using VISUAL line count (accounting for wrap)
    let view_height = area.height as usize;
    let render_width = area.width as usize;
    let wrap_start = std::time::Instant::now();
    let visual_content_height = total_wrapped_lines(&lines, render_width);
    stats.wrap_count = wrap_start.elapsed();
    stats.content_height = visual_content_height;

    // Export collected image positions for overlay rendering, converting each
    // logical line index to the visual row the paragraph actually draws it
    // on: a wrapped line above pushes the image's reserved rows further down.
    if let Some(out) = image_positions_out {
        out.clear();
        out.extend(
            collected_image_positions
                .into_iter()
                .map(|(key, index, caption)| {
                    let row = lines[..index.min(lines.len())]
                        .iter()
                        .map(|line| wrapped_line_rows(line, render_width))
                        .sum();
                    (key, row, caption)
                }),
        );
    }

    // Export dimensions for scroll normalization
    // Use visual height so scrolling works correctly with wrapped lines
    if let Some(cell) = content_height_out {
        cell.store(visual_content_height, Ordering::Relaxed);
    }
    if let Some(cell) = view_height_out {
        cell.store(view_height, Ordering::Relaxed);
    }

    // Keep input/composer anchored to the bottom when content underflows.
    // Use explicit top padding lines to keep wrap + scroll behavior consistent.
    let top_padding = view_height.saturating_sub(visual_content_height);
    // Visual row where the input line starts (before padding is applied).
    let input_top = input_line_idx.map(|idx| {
        visual_content_height.saturating_sub(total_wrapped_lines(&lines[idx..], render_width))
    });
    let (scroll_y, final_lines) = if visual_content_height < view_height {
        let mut padded = Vec::with_capacity(top_padding + lines.len());
        for _ in 0..top_padding {
            padded.push(Line::from(""));
        }
        padded.extend(lines);
        (0u16, padded)
    } else {
        let max_scroll = visual_content_height - view_height;
        let actual_scroll = if scroll == usize::MAX {
            max_scroll
        } else {
            scroll.min(max_scroll)
        };
        (u16::try_from(actual_scroll).unwrap_or(u16::MAX), lines)
    };

    let widget_start = std::time::Instant::now();
    frame.render_widget(Clear, area);
    let paragraph = Paragraph::new(final_lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0));
    frame.render_widget(paragraph, area);

    // Anchor the terminal cursor on the caret cell so IME composition and
    // its candidate window track the real insertion point.
    if let (Some(caret_line), Some(input_top)) = (caret_line, input_top)
        && let Some((cx, cy)) = wrapped_tail_cell(caret_line, area.width)
    {
        let caret_row = top_padding + input_top + usize::from(cy);
        if caret_row >= usize::from(scroll_y)
            && caret_row - usize::from(scroll_y) < usize::from(area.height)
        {
            frame.set_cursor_position((
                area.x + cx,
                area.y + u16::try_from(caret_row - usize::from(scroll_y)).unwrap_or(u16::MAX),
            ));
        }
    }
    stats.widget = widget_start.elapsed();
    stats
}

/// Convert a character index to a byte index in a string.
fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(i, _)| i)
}

/// Wrapped row count of a single rendered line, matching the main
/// paragraph's word wrap exactly — `Paragraph::line_count` is the only
/// way to get an exact match for the wrap behavior.
fn wrapped_line_rows(line: &Line<'_>, width: usize) -> usize {
    if width == 0 {
        return 0;
    }
    let w = u16::try_from(width).unwrap_or(u16::MAX);
    Paragraph::new(vec![line.clone()])
        .wrap(Wrap { trim: false })
        .line_count(w)
}

/// The input `Line` up to and including the caret marker: prompt,
/// attachment chips and the text before the caret. The caret marker's
/// cell is where the terminal cursor — and therefore IME composition —
/// anchors.
fn input_caret_line(ctx: &InputContext<'_>) -> Line<'static> {
    let mut spans = Vec::new();

    // Prompt
    spans.push(Span::styled("> ", Style::default().fg(Color::Cyan)));

    // Render image attachments
    for att in ctx.attachments {
        if matches!(att, Attachment::Image { .. }) {
            spans.push(Span::styled(
                "[image] ",
                Style::default().bg(Color::DarkGray),
            ));
        }
    }

    // File references
    for att in ctx.attachments {
        if let Attachment::FileReference { path } = att {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
            spans.push(Span::styled(
                format!("@{name} "),
                Style::default().fg(Color::Cyan),
            ));
        }
    }

    // Pasted text attachments
    for att in ctx.attachments {
        if let Attachment::PastedText { content, .. } = att {
            let char_count = content.chars().count();
            spans.push(Span::styled(
                format!("[pasted: {char_count} chars] "),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }

    let byte_idx = char_to_byte_index(ctx.input, ctx.cursor);
    spans.push(Span::raw(ctx.input[..byte_idx].to_string()));
    spans.push(Span::styled("│", Style::default().fg(Color::Yellow)));
    Line::from(spans)
}

/// Cell of `line`'s last grapheme once wrapped to `width`: the caret cell
/// of the input caret line. Rendering into a scratch buffer keeps this
/// exactly in step with `Paragraph` wrapping (wide graphemes included).
fn wrapped_tail_cell(line: Line<'static>, width: u16) -> Option<(u16, u16)> {
    if width == 0 {
        return None;
    }
    let rows = u16::try_from(wrapped_line_rows(&line, width as usize)).unwrap_or(u16::MAX);
    if rows == 0 {
        return None;
    }
    let buf_area = Rect::new(0, 0, width, rows);
    let mut buf = Buffer::empty(buf_area);
    Paragraph::new(vec![line])
        .wrap(Wrap { trim: false })
        .render(buf_area, &mut buf);
    // Trailing cells of wide graphemes are reset to blank, so the last
    // non-blank cell is the caret marker itself.
    for y in (buf_area.top()..buf_area.bottom()).rev() {
        for x in (buf_area.left()..buf_area.right()).rev() {
            if buf[(x, y)].symbol() != " " {
                return Some((x, y));
            }
        }
    }
    None
}

/// Render the input line into the lines vector and return its index.
/// Attached images get a placeholder block above the input row — the
/// overlay paints the preview into it — when the terminal supports
/// graphics.
fn render_input_line(
    lines: &mut Vec<Line<'_>>,
    ctx: &InputContext<'_>,
    images_supported: bool,
    mut image_positions: Option<&mut Vec<(String, usize, String)>>,
) -> usize {
    if images_supported {
        for att in ctx.attachments {
            if let Attachment::Image { path, .. } = att {
                let index = push_image_rows(lines);
                if let Some(key) = att.image_cache_key()
                    && let Some(positions) = image_positions.as_mut()
                {
                    positions.push((key, index, path.display().to_string()));
                }
            }
        }
    }

    // Input text with caret (convert char index to byte index). The real
    // terminal cursor marks the caret — the `│` stays only in the
    // scratch anchor used for cursor placement.
    let mut line = input_caret_line(ctx);
    line.spans.pop();
    let byte_idx = char_to_byte_index(ctx.input, ctx.cursor);
    line.spans
        .push(Span::raw(ctx.input[byte_idx..].to_string()));

    let index = lines.len();
    lines.push(line);
    index
}

/// Render queued messages into the lines vector.
fn render_queue_lines(lines: &mut Vec<Line<'_>>, ctx: &QueueContext<'_>, width: u16) {
    let queue_len = ctx.messages.len();

    // Header line
    let header = format!(
        "── Queued ({}) {} ──",
        queue_len,
        if queue_len == 1 {
            "message"
        } else {
            "messages"
        }
    );
    lines.push(Line::from(Span::styled(
        header,
        Style::default().fg(Color::Yellow),
    )));

    // Show up to 3 queued messages
    let max_width = width.saturating_sub(6) as usize;
    for msg in ctx.messages.iter().take(3) {
        let preview: String = msg
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(max_width)
            .collect();
        let truncated = if msg.chars().count() > max_width || msg.lines().count() > 1 {
            format!("{preview}...")
        } else {
            preview
        };

        lines.push(Line::from(vec![
            Span::styled("  ⏳ ", Style::default().fg(Color::DarkGray)),
            Span::styled(truncated, Style::default().fg(Color::DarkGray)),
        ]));
    }

    // If more than 3 messages, show indicator
    if queue_len > 3 {
        lines.push(Line::from(Span::styled(
            format!("  ... and {} more", queue_len - 3),
            Style::default().fg(Color::DarkGray),
        )));
    }
}

/// Spinner glyphs cycled by `spinner_frame` while a run is in flight.
const SPINNER_FRAMES: [char; 4] = ['⠋', '⠙', '⠹', '⠸'];

/// Format a turn's output-token count for the activity line
/// (`340 tokens`, `1.3k tokens`).
fn format_turn_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M tokens", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k tokens", tokens as f64 / 1_000.0)
    } else if tokens == 1 {
        "1 token".to_string()
    } else {
        format!("{tokens} tokens")
    }
}

/// Render the status bar into the lines vector. `width` is the render width;
/// context usage is pinned to the right edge.
fn render_status_line(lines: &mut Vec<Line<'_>>, ctx: &StatusContext<'_>, width: usize) {
    // Live activity sits on its own line above the bar: what the model is
    // doing plus how much it has produced this turn.
    if let Some(activity) = ctx.activity {
        let spinner = SPINNER_FRAMES[ctx.spinner_frame % SPINNER_FRAMES.len()];
        let text = if activity.tokens == 0 {
            format!("{} {spinner}", activity.phase.label())
        } else {
            format!(
                "{} {spinner} ({})",
                activity.phase.label(),
                format_turn_tokens(activity.tokens)
            )
        };
        lines.push(Line::from(Span::styled(
            text,
            Style::default().fg(Color::DarkGray),
        )));
    }

    // If there's a status message (like Ctrl+C hint), show it prominently
    if let Some(status) = ctx.status_message {
        lines.push(Line::from(Span::styled(
            status.to_string(),
            Style::default().fg(Color::Yellow),
        )));
        return;
    }

    let mut parts = vec![ctx.cwd.to_string(), ctx.model.to_string()];

    if let Some(rate) = ctx.decode_rate {
        if rate >= 100.0 {
            parts.push(format!("{rate:.0} tok/s"));
        } else {
            parts.push(format!("{rate:.1} tok/s"));
        }
    }

    if ctx.queue_count > 0 {
        parts.push(format!("Q:{}", ctx.queue_count));
    }

    if ctx.open_questions > 0 {
        parts.push(format!("? {} open · /questions", ctx.open_questions));
    }

    for item in ctx.background.iter().take(3) {
        let icon = if item.subagent { "◉" } else { "●" };
        parts.push(format!(
            "{icon} {} {}",
            item.label,
            format_elapsed(item.elapsed_secs)
        ));
    }
    let hidden = ctx.background.len().saturating_sub(3);
    if hidden > 0 {
        parts.push(format!("+{hidden}"));
    }
    if !ctx.background.is_empty() {
        parts.push("↓ jobs".to_string());
    }

    // Context usage pinned to the right edge: `4.8K/1M`.
    let usage_text = ctx.usage.map(|(prompt_tokens, context_window)| {
        let used = format_number(usize::try_from(prompt_tokens).unwrap_or(usize::MAX));
        match context_window {
            Some(window) => format!(
                "{}/{}",
                used,
                format_number(usize::try_from(window).unwrap_or(usize::MAX))
            ),
            None => used,
        }
    });

    let status_text = parts.join(" | ");
    let status_style = Style::default().fg(Color::Cyan);

    if let Some(usage_text) = usage_text {
        let pad = width
            .saturating_sub(status_text.width() + usage_text.width())
            .max(1);
        lines.push(Line::from(vec![
            Span::styled(status_text, status_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(usage_text, status_style),
        ]));
    } else {
        lines.push(Line::from(Span::styled(status_text, status_style)));
    }
}

/// Format an elapsed duration compactly (`45s`, `2m05s`, `1h03m`).
#[must_use]
pub(crate) fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Format large numbers with K/M suffix for readability.
pub(crate) fn format_number(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Render a tool call inline with optional selection highlight.
/// Marker prefix shown before a terminal command for its execution backend.
const fn terminal_mode_marker(mode: TerminalExecutionMode) -> &'static str {
    match mode {
        TerminalExecutionMode::Default => "",
        TerminalExecutionMode::Sandboxed => "[sandbox] ",
        TerminalExecutionMode::Unsafe => "[unsafe] ",
        TerminalExecutionMode::Ssh => "[ssh] ",
    }
}

/// A tool call's styled headline: expand marker + chip/description +
/// collapsed status suffix. Mirrors `plain_tool_lines`'s first line.
fn tool_header_line(display: &ToolDisplay, selection_style: Style, open: bool) -> Line<'_> {
    let marker = if !display.expandable() {
        "  "
    } else if open {
        "▾ "
    } else {
        "▸ "
    };
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = vec![Span::styled(marker, dim)];
    match display {
        ToolDisplay::Terminal { .. } => {
            spans.push(Span::styled(tool_headline(display), selection_style));
        }
        ToolDisplay::Action { chip, summary, .. } => {
            let (label, rest) = action_headline(chip, summary);
            spans.push(Span::styled(label, chip_style(chip)));
            if !rest.is_empty() {
                spans.push(Span::styled(format!(" {rest}"), selection_style));
            }
        }
        ToolDisplay::Builtin { tool, detail, .. } => {
            spans.push(Span::styled(
                format!("[{tool}]"),
                Style::default().fg(Color::Magenta),
            ));
            if !detail.is_empty() {
                spans.push(Span::styled(format!(" {detail}"), selection_style));
            }
        }
    }
    let suffix = collapsed_status_suffix(display, open);
    if !suffix.is_empty() {
        let style = if matches!(display.status(), ToolStatus::Failed(_)) {
            Style::default().fg(Color::Red)
        } else {
            dim
        };
        spans.push(Span::styled(suffix, style));
    }
    Line::from(spans)
}

/// An `Action` headline as `(label, rest)`: known IPC command names get a
/// semantic verb (`websearch` → `Searched "query"`), everything else keeps
/// its `[chip]` label. `tool_headline` renders the same text plainly.
fn action_headline(chip: &str, summary: &str) -> (String, String) {
    match chip {
        "websearch" => ("Searched".to_string(), format!("\"{summary}\"")),
        "webfetch" => ("Fetched".to_string(), summary.to_string()),
        _ => (format!("[{chip}]"), summary.to_string()),
    }
}

/// Color for an `Action` chip label.
fn chip_style(chip: &str) -> Style {
    match chip {
        "kill" => Style::default().fg(Color::Red),
        "stdin" => Style::default().fg(Color::Yellow),
        "websearch" | "webfetch" => Style::default().fg(Color::Blue),
        _ => Style::default().fg(Color::Cyan),
    }
}

fn render_tool_inline<'a>(
    lines: &mut Vec<Line<'a>>,
    display: &'a ToolDisplay,
    is_selected: bool,
    images_supported: bool,
    image_positions: &mut Vec<(String, usize, String)>,
) {
    let open = display.expandable() && display.body_open(Instant::now());
    let selection_style = if is_selected {
        Style::default().bg(Color::DarkGray)
    } else {
        Style::default()
    };
    let dim = Style::default().fg(Color::DarkGray);

    lines.push(tool_header_line(display, selection_style, open));
    if !open {
        return;
    }

    match display {
        ToolDisplay::Terminal {
            command,
            mode,
            status,
            output,
            diffs,
            ..
        } => {
            let mode_prefix = terminal_mode_marker(*mode);
            let mut command_lines = command.lines();
            if let Some(first) = command_lines.next() {
                lines.push(Line::from(vec![
                    Span::styled(mode_prefix, Style::default().fg(Color::Red).bold()),
                    Span::styled("$ ", dim),
                    Span::styled(first.to_string(), selection_style),
                ]));
                for line in command_lines {
                    lines.push(Line::from(vec![
                        Span::styled("│ ", dim),
                        Span::styled(line.to_string(), selection_style),
                    ]));
                }
            } else {
                lines.push(Line::from(vec![
                    Span::styled(mode_prefix, Style::default().fg(Color::Red).bold()),
                    Span::styled("$", dim),
                ]));
            }
            if let Some(status_text) = body_status_text(status) {
                lines.push(Line::from(vec![
                    Span::styled("↳ ", dim),
                    Span::styled(status_text, dim),
                ]));
            }
            if let Some(info) = output
                && !info.output.is_empty()
            {
                for line in info.output_preview(OUTPUT_PREVIEW_LINES).lines() {
                    lines.push(Line::from(Span::styled(format!("│ {line}"), dim)));
                }
            }
            push_tool_call_image(lines, output.as_ref(), images_supported, image_positions);
            render_diff_preview(lines, diffs);
        }
        ToolDisplay::Action {
            detail,
            status,
            output,
            ..
        } => {
            for line in detail.iter().take(DIFF_PREVIEW_LINES) {
                lines.push(Line::from(Span::styled(format!("│ {line}"), dim)));
            }
            if detail.len() > DIFF_PREVIEW_LINES {
                lines.push(Line::from(Span::styled(
                    format!("│ … {} more lines", detail.len() - DIFF_PREVIEW_LINES),
                    dim.italic(),
                )));
            }
            if let Some(status_text) = body_status_text(status) {
                lines.push(Line::from(vec![
                    Span::styled("↳ ", dim),
                    Span::styled(status_text, dim),
                ]));
            }
            if let Some(info) = output
                && !info.output.is_empty()
            {
                for line in info.output_preview(OUTPUT_PREVIEW_LINES).lines() {
                    lines.push(Line::from(Span::styled(format!("│ {line}"), dim)));
                }
            }
            push_tool_call_image(lines, output.as_ref(), images_supported, image_positions);
        }
        ToolDisplay::Builtin { status, output, .. } => {
            if let Some(status_text) = body_status_text(status) {
                lines.push(Line::from(vec![
                    Span::styled("↳ ", dim),
                    Span::styled(status_text, dim),
                ]));
            }
            if let Some(text) = output {
                for line in text.lines().take(OUTPUT_PREVIEW_LINES) {
                    lines.push(Line::from(Span::styled(format!("│ {line}"), dim)));
                }
            }
        }
    }
}

/// Style one unified-diff line by its leading marker.
fn diff_line_style(line: &str) -> Style {
    if line.starts_with("+++") || line.starts_with("---") {
        Style::default().fg(Color::DarkGray).bold()
    } else if line.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::default().fg(Color::Red)
    } else if line.starts_with("@@") {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Render a compact per-root diff summary plus a capped tinted preview
/// inside an expanded terminal tool body.
fn render_diff_preview<'a>(lines: &mut Vec<Line<'a>>, diffs: &'a [crate::file_tracker::RootDiff]) {
    for (index, diff) in diffs.iter().enumerate() {
        let summary = format!(
            "± {} file{} (+{} −{})",
            diff.files,
            if diff.files == 1 { "" } else { "s" },
            diff.additions,
            diff.deletions,
        );
        lines.push(Line::from(Span::styled(
            if diffs.len() > 1 {
                format!("{summary} in {}", diff.root.display())
            } else {
                summary
            },
            Style::default().fg(Color::Yellow),
        )));

        let total = diff.text.lines().count();
        for line in diff.text.lines().take(DIFF_PREVIEW_LINES) {
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(line.to_string(), diff_line_style(line)),
            ]));
        }
        if total > DIFF_PREVIEW_LINES {
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("… {} more diff lines", total - DIFF_PREVIEW_LINES),
                    Style::default().fg(Color::DarkGray).italic(),
                ),
            ]));
        }
        if index + 1 < diffs.len() {
            lines.push(Line::from(""));
        }
    }
}

/// Render full-screen tool detail view.
pub fn render_tool_fullscreen(frame: &mut Frame, area: Rect, display: &ToolDisplay, scroll: usize) {
    // Guard against zero-dimension areas to prevent overflow
    if area.width < 3 || area.height < 3 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    // Header
    let title = match display {
        ToolDisplay::Terminal {
            description,
            command,
            mode,
            ..
        } => {
            if description.is_empty() {
                format!("{}$ {command}", terminal_mode_marker(*mode))
            } else {
                format!("{description} — {}$ {command}", terminal_mode_marker(*mode))
            }
        }
        ToolDisplay::Action { chip, summary, .. } => {
            let (label, rest) = action_headline(chip, summary);
            format!("{label} {rest}").trim_end().to_string()
        }
        ToolDisplay::Builtin { tool, detail, .. } => {
            format!("{tool}: {detail}")
        }
    };

    lines.push(Line::from(Span::styled(
        title,
        Style::default().bold().fg(Color::Cyan),
    )));
    lines.push(Line::from(""));

    // Status
    let status_line = match display.status() {
        ToolStatus::Running => {
            Span::styled("Status: Running...", Style::default().fg(Color::Yellow))
        }
        ToolStatus::Done => Span::styled("Status: Done", Style::default().fg(Color::Green)),
        ToolStatus::Failed(e) => Span::styled(
            format!("Status: Failed - {e}"),
            Style::default().fg(Color::Red),
        ),
    };
    lines.push(Line::from(status_line));
    lines.push(Line::from(""));

    // Invocation detail for Action calls (the raw arguments payload).
    if let ToolDisplay::Action { detail, .. } = display
        && !detail.is_empty()
    {
        lines.push(Line::from(Span::styled(
            "─── Detail ───",
            Style::default().fg(Color::Blue),
        )));
        for line in detail {
            lines.push(Line::from(line.clone()));
        }
        lines.push(Line::from(""));
    }

    // Provider-hosted result text.
    if let ToolDisplay::Builtin {
        output: Some(text), ..
    } = display
    {
        for line in text.lines() {
            lines.push(Line::from(line.to_string()));
        }
        lines.push(Line::from(""));
    }

    // Terminal calls
    let terminal_calls = display.all_terminal_calls();
    if terminal_calls.is_empty() {
        lines.push(Line::from(Span::styled(
            "No terminal calls yet.",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (i, call) in terminal_calls.iter().enumerate() {
            lines.push(Line::from(Span::styled(
                format!("─── Command {} ───", i + 1),
                Style::default().fg(Color::Blue),
            )));
            lines.push(Line::from(""));

            // Full command (no truncation)
            lines.push(Line::from(vec![
                Span::styled("$ ", Style::default().fg(Color::Yellow)),
                Span::raw(&call.command),
            ]));
            lines.push(Line::from(""));

            // Full output (no truncation - shows what agent sees)
            lines.push(Line::from(Span::styled(
                "Output:",
                Style::default().fg(Color::Green),
            )));

            if call.output.is_empty() {
                lines.push(Line::from(Span::styled(
                    "(no output)",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                for line in call.output.lines() {
                    lines.push(Line::from(line.to_string()));
                }
            }

            // Show if output was offloaded
            if let Some(ref path) = call.output_file {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("Full output saved to: {}", path.display()),
                    Style::default().fg(Color::DarkGray).italic(),
                )));
            }

            lines.push(Line::from(""));
        }
    }

    // File changes attributed to this call
    if let ToolDisplay::Terminal { diffs, .. } = display
        && !diffs.is_empty()
    {
        for (index, diff) in diffs.iter().enumerate() {
            lines.push(Line::from(Span::styled(
                format!(
                    "─── Changes in {} ({} file{}, +{} −{}) ───",
                    diff.root.display(),
                    diff.files,
                    if diff.files == 1 { "" } else { "s" },
                    diff.additions,
                    diff.deletions,
                ),
                Style::default().fg(Color::Blue),
            )));
            for line in diff.text.lines() {
                lines.push(Line::from(Span::styled(
                    line.to_string(),
                    diff_line_style(line),
                )));
            }
            if index + 1 < diffs.len() {
                lines.push(Line::from(""));
            }
        }
    }

    // Help text at bottom
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press Esc to close, j/k to scroll",
        Style::default().fg(Color::DarkGray),
    )));

    // Calculate safe scroll value (account for borders: 2 lines)
    let inner_height = area.height.saturating_sub(2);
    let scroll_y = safe_scroll(scroll, lines.len(), inner_height);

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Tool Details")
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0));

    frame.render_widget(paragraph, area);
}
#[cfg(test)]
mod tests {
    use super::{
        ChatElement, ChatStream, InputContext, RECENT_EXPAND, ToolCallImage, ToolDisplay,
        ToolStatus, build_plain_wrapped_lines_unified, render_input_line, render_tool_inline,
        terminal_mode_marker,
    };
    use crate::tui::queue::Attachment;
    use crate::tui::ui::streaming_markdown::IMAGE_HEIGHT;
    use aither_sandbox::TerminalExecutionMode;
    use ratatui::text::Line;
    use std::path::PathBuf;
    use std::time::Instant;

    #[test]
    fn live_output_appends_to_running_terminal_call() {
        let mut stream = ChatStream::new();
        stream.push_tool(
            "call-1".to_string(),
            ToolDisplay::Terminal {
                description: String::new(),
                command: "echo hi".to_string(),
                mode: TerminalExecutionMode::Default,
                status: ToolStatus::Running,
                output: None,
                diffs: Vec::new(),
                pinned: None,
                finished_at: None,
            },
        );

        assert_eq!(
            stream.running_terminal_calls(),
            vec![("call-1".to_string(), "echo hi".to_string())]
        );
        assert!(stream.is_tool_running("call-1"));

        stream.append_tool_output("call-1", "he");
        stream.append_tool_output("call-1", "llo\nwor");
        stream.append_tool_output("call-1", "ld\n");

        let Some(ChatElement::ToolCall { display, .. }) = stream.elements.first() else {
            panic!("tool call missing");
        };
        let ToolDisplay::Terminal {
            output: Some(info), ..
        } = display
        else {
            panic!("streamed output missing");
        };
        assert_eq!(info.command, "echo hi");
        assert_eq!(info.output, "hello\nworld\n");

        // A finished call leaves the running list and stops accepting
        // late deltas via the running-only path.
        stream.update_tool_with_output("call-1", ToolStatus::Done, Some("hello\nworld\n"));
        assert_eq!(stream.running_terminal_calls(), Vec::new());
        assert!(!stream.is_tool_running("call-1"));
    }

    #[test]
    fn terminal_mode_marker_covers_every_backend() {
        assert_eq!(terminal_mode_marker(TerminalExecutionMode::Default), "");
        assert_eq!(
            terminal_mode_marker(TerminalExecutionMode::Sandboxed),
            "[sandbox] "
        );
        assert_eq!(
            terminal_mode_marker(TerminalExecutionMode::Unsafe),
            "[unsafe] "
        );
        assert_eq!(terminal_mode_marker(TerminalExecutionMode::Ssh), "[ssh] ");
    }

    #[test]
    fn inline_terminal_renders_mode_marker() {
        for (mode, marker) in [
            (TerminalExecutionMode::Default, ""),
            (TerminalExecutionMode::Sandboxed, "[sandbox] "),
            (TerminalExecutionMode::Unsafe, "[unsafe] "),
            (TerminalExecutionMode::Ssh, "[ssh] "),
        ] {
            let display = ToolDisplay::Terminal {
                description: String::new(),
                command: "echo hi".to_string(),
                mode,
                status: ToolStatus::Done,
                output: None,
                diffs: Vec::new(),
                pinned: None,
                finished_at: None,
            };
            let mut lines: Vec<Line> = Vec::new();
            let mut positions = Vec::new();
            render_tool_inline(&mut lines, &display, false, true, &mut positions);
            let rendered: String = lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert_eq!(rendered, format!("▸ {marker}$ echo hi"));
        }
    }

    /// A call that finished long ago — outside [`RECENT_EXPAND`], so the
    /// automatic rule renders it collapsed.
    fn terminal_display(command: &str) -> ToolDisplay {
        ToolDisplay::Terminal {
            description: String::new(),
            command: command.to_string(),
            mode: TerminalExecutionMode::Default,
            status: ToolStatus::Done,
            output: None,
            diffs: Vec::new(),
            pinned: None,
            finished_at: Some(
                Instant::now()
                    .checked_sub(RECENT_EXPAND * 2)
                    .expect("test clock precedes twice the expand window"),
            ),
        }
    }

    /// A scroll step redraws the whole frame, so the diff written to the
    /// backend must reproduce it exactly; a skipped cell stays visible as
    /// a stray block. Mirrors what a real terminal shows after diffing.
    #[test]
    fn scroll_diff_leaves_no_stale_cells() {
        use super::render;
        use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Rect};

        let mut chat = ChatStream::new();
        chat.push_text("终端通道工作正常。要做更具体的测试吗？比如：\n\n- 并发/时序测试 — 比如先跑 `sleep 5` 再跑 `sleep 10`，验证顺序与完成回调\n- 后台任务测试 — `terminal` 加 `background` + `terminal_read` 增量读取\n- 文件写入测试 — 在 `artifacts/` 或 `data/` 里写文件并校验持久性\n- 特定命令 — 你直接给出要跑的命令，我执行后按你指定的格式回传（比如只回 `finished`）\n\n直接说要测什么就行。\n");
        chat.push_user("你可以跑后台命令吗", Vec::new(), 0);
        chat.push_text("可以，用 `terminal` 加 `background` 就行。我跑一个演示：\n");
        chat.push_tool(
            "call-1".to_string(),
            ToolDisplay::Terminal {
                description: String::new(),
                command: "terminal {…}".to_string(),
                mode: TerminalExecutionMode::Default,
                status: ToolStatus::Done,
                output: Some(
                    super::TerminalCallInfo::new("terminal".to_string()).with_output(
                        "tick 1 at 21:20:24\ntick 2 at 21:20:25\ntick 3 at 21:20:26".to_string(),
                    ),
                ),
                diffs: Vec::new(),
                pinned: None,
                finished_at: None,
            },
        );
        chat.push_text("后台任务运行正常，已输出到第 7 行（约每秒一行）。再等它跑完：\n");
        chat.push_text("可用的后台操作还有：\n\n- `terminal_read` - 按字节偏移增量读取（不读已读部分）\n- `terminal_input` - 向后台任务的 stdin 写数据（适合交互式进程）\n- `terminal_kill` - 中途终止任务\n- `timeout` 参数控制后台任务最长存活时间（最多 7200 秒）\n\n要不要继续测某个具体场景？比如交互式 stdin、或者并发跑多个后台任务？\n");

        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        let area = Rect::new(0, 0, 80, 30);

        let mut truth = Buffer::empty(area);
        for scroll in [usize::MAX, 9, 6, 3, 0, usize::MAX, 12] {
            terminal
                .draw(|frame| {
                    render(frame, area, &chat, scroll);
                    truth.clone_from(frame.buffer_mut());
                })
                .unwrap();
            let shown = terminal.backend().buffer().clone();
            assert_eq!(
                &shown, &truth,
                "scroll={scroll}: backend drifted from the rendered buffer"
            );
        }
    }

    fn rendered_text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn expandable_rules() {
        // Terminal calls always have a command body to expand into.
        assert!(terminal_display("echo hi").expandable());
        assert!(terminal_display("a\nb\nc").expandable());

        // Action/Builtin calls expand only when they carry detail or output.
        let bare = ToolDisplay::Action {
            chip: "kill".to_string(),
            task_id: None,
            summary: "task".to_string(),
            detail: Vec::new(),
            status: ToolStatus::Done,
            output: None,
            pinned: None,
            finished_at: None,
        };
        assert!(!bare.expandable());
        let builtin = ToolDisplay::Builtin {
            tool: "web_search".to_string(),
            detail: "q".to_string(),
            status: ToolStatus::Done,
            output: None,
            pinned: None,
            finished_at: None,
        };
        assert!(!builtin.expandable());
    }

    #[test]
    fn expansion_states() {
        // Finished long ago: collapsed to the headline only.
        let mut display = terminal_display("one\ntwo\nthree\nfour\nfive");
        let text = {
            let mut lines: Vec<Line> = Vec::new();
            let mut positions = Vec::new();
            render_tool_inline(&mut lines, &display, false, true, &mut positions);
            rendered_text(&lines)
        };
        assert_eq!(text, vec!["▸ $ one".to_string()]);

        // Just finished: still open, showing the whole command.
        let ToolDisplay::Terminal { finished_at, .. } = &mut display else {
            unreachable!()
        };
        *finished_at = Some(Instant::now());
        let text = {
            let mut lines: Vec<Line> = Vec::new();
            let mut positions = Vec::new();
            render_tool_inline(&mut lines, &display, false, true, &mut positions);
            rendered_text(&lines)
        };
        assert_eq!(text.len(), 6); // headline + 5 command lines
        assert_eq!(text[5], "│ five");

        // A click pins it shut regardless of the recent-expansion window.
        let ToolDisplay::Terminal { pinned, .. } = &mut display else {
            unreachable!()
        };
        *pinned = Some(false);
        let text = {
            let mut lines: Vec<Line> = Vec::new();
            let mut positions = Vec::new();
            render_tool_inline(&mut lines, &display, false, true, &mut positions);
            rendered_text(&lines)
        };
        assert_eq!(text, vec!["▸ $ one".to_string()]);
    }

    #[test]
    fn expanded_shows_diff_body() {
        let body = "+line\n".repeat(12);
        let mut display = terminal_display("echo hi");
        {
            let ToolDisplay::Terminal { diffs, .. } = &mut display else {
                unreachable!()
            };
            *diffs = vec![crate::file_tracker::RootDiff {
                root: std::path::PathBuf::from("/tmp/root"),
                text: body,
                files: 1,
                additions: 12,
                deletions: 0,
            }];
        }

        // Collapsed: only the headline, no diff information.
        let mut lines: Vec<Line> = Vec::new();
        let mut positions = Vec::new();
        render_tool_inline(&mut lines, &display, false, true, &mut positions);
        let text = rendered_text(&lines);
        assert_eq!(text, vec!["▸ $ echo hi".to_string()]);

        // Expanded: command, diff summary and the capped diff body.
        let ToolDisplay::Terminal {
            pinned,
            finished_at,
            ..
        } = &mut display
        else {
            unreachable!()
        };
        *pinned = Some(true);
        *finished_at = Some(Instant::now());
        let mut lines: Vec<Line> = Vec::new();
        let mut positions = Vec::new();
        render_tool_inline(&mut lines, &display, false, true, &mut positions);
        let text = rendered_text(&lines);
        assert_eq!(text[0], "▾ $ echo hi");
        assert!(text[2].contains("± 1 file (+12 −0)"));
        assert!(text[11].contains("4 more diff lines"));
    }

    #[test]
    fn toggle_expands_and_collapses() {
        let mut chat = ChatStream::new();
        chat.push_tool("t1".to_string(), terminal_display("a\nb\nc\nd"));
        assert_eq!(
            build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false).len(),
            1 // headline only
        );
        chat.toggle_expanded("t1");
        assert_eq!(
            build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false).len(),
            5 // headline + 4 command lines
        );
        chat.toggle_expanded("t1");
        assert_eq!(
            build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false).len(),
            1
        );
    }

    fn image_attachment() -> Attachment {
        Attachment::Image {
            path: PathBuf::from("/tmp/shot.png"),
            data: vec![0x89, 0x50, 0x4E, 0x47],
        }
    }

    #[test]
    fn user_image_reserves_placeholder_rows_when_supported() {
        let mut chat = ChatStream::new();
        chat.push_user("look", vec![image_attachment()], 0);

        let supported = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, true);
        assert_eq!(supported.len(), 2 + IMAGE_HEIGHT); // "> look" + header + blanks + trailing ""

        let unsupported =
            build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert_eq!(unsupported.len(), 3); // "> look" + "[image]" + trailing ""
        assert_eq!(unsupported[1], "[image]");
    }

    #[test]
    fn input_preview_rows_appear_above_prompt() {
        let attachments = vec![image_attachment()];
        let ctx = InputContext {
            input: "send it",
            cursor: 7,
            attachments: &attachments,
        };
        let mut lines: Vec<Line> = Vec::new();
        let mut positions = Vec::new();
        render_input_line(&mut lines, &ctx, true, Some(&mut positions));

        assert_eq!(lines.len(), IMAGE_HEIGHT + 1);
        assert!(
            lines[..IMAGE_HEIGHT]
                .iter()
                .all(|line| line.spans.iter().all(|span| span.content.is_empty()))
        );
        assert_eq!(positions.len(), 1);
        let prompt = lines.last().expect("input line");
        assert_eq!(prompt.spans[0].content, "> ");
    }

    #[test]
    fn input_without_image_support_shows_marker_only() {
        let attachments = vec![image_attachment()];
        let ctx = InputContext {
            input: "send it",
            cursor: 7,
            attachments: &attachments,
        };
        let mut lines: Vec<Line> = Vec::new();
        render_input_line(&mut lines, &ctx, false, None);

        assert_eq!(lines.len(), 1);
        let rendered: String = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(rendered.contains("[image]"));
    }

    #[test]
    fn tool_image_reserves_rows_and_registers_position() {
        let mut display = terminal_display("screenshot");
        let ToolDisplay::Terminal { output, pinned, .. } = &mut display else {
            unreachable!()
        };
        *pinned = Some(true);
        *output = Some(
            super::TerminalCallInfo::new("screenshot".to_string()).with_image(ToolCallImage {
                key: "tool-image:t1".to_string(),
                label: "shot.png".to_string(),
                path: "outputs/shot.png".to_string(),
            }),
        );

        let mut lines: Vec<Line> = Vec::new();
        let mut positions = Vec::new();
        render_tool_inline(&mut lines, &display, false, true, &mut positions);

        // headline + "$ screenshot" + image block
        assert_eq!(lines.len(), 2 + IMAGE_HEIGHT);
        assert_eq!(
            positions,
            vec![(
                "tool-image:t1".to_string(),
                2,
                "outputs/shot.png".to_string()
            )]
        );

        // Without graphics support the block collapses to nothing — the text
        // output already carries the [image] marker.
        let mut lines: Vec<Line> = Vec::new();
        let mut positions = Vec::new();
        render_tool_inline(&mut lines, &display, false, false, &mut positions);
        assert_eq!(lines.len(), 2);
        assert_eq!(positions, Vec::<(String, usize, String)>::new());
    }

    #[test]
    fn click_region_covers_collapsible_call() {
        let mut chat = ChatStream::new();
        chat.push_tool("t1".to_string(), terminal_display("a\nb\nc\nd"));
        chat.push_tool("t2".to_string(), terminal_display("echo short"));
        let mut regions = Vec::new();
        let lines = build_plain_wrapped_lines_unified(
            &chat,
            80,
            None,
            None,
            None,
            Some(&mut regions),
            false,
        );
        // Both calls collapsed to their headline; each is a one-row region.
        assert_eq!(lines.len(), 2);
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].element_id, "t1");
        assert_eq!(regions[0].start, 0);
        assert_eq!(regions[0].end, 1);
        assert_eq!(regions[1].element_id, "t2");
        assert_eq!(regions[1].start, 1);
        assert_eq!(regions[1].end, 2);
    }

    #[test]
    fn thinking_streams_then_collapses_and_reopens() {
        let mut chat = ChatStream::new();
        chat.push_thinking("step one\nstep two");
        chat.update_thinking(3);
        // While streaming, the header plus both content lines are visible.
        let open = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert_eq!(open.len(), 4); // header + 2 lines + trailing blank
        assert!(open[0].contains("Thinking"));
        assert!(open[1].contains("step one"));

        // Burst end collapses to a single clickable header.
        chat.collapse_thinking();
        let mut regions = Vec::new();
        let collapsed = build_plain_wrapped_lines_unified(
            &chat,
            80,
            None,
            None,
            None,
            Some(&mut regions),
            false,
        );
        assert_eq!(collapsed.len(), 2); // collapsed header + trailing blank
        assert!(collapsed[0].contains("Thought for 3 sec"));
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].element_id, "thinking-0");

        // Clicking the header reopens the content.
        chat.toggle_expanded("thinking-0");
        let reopened = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert_eq!(reopened.len(), 4);
        assert!(reopened[1].contains("step one"));
    }

    #[test]
    fn new_reasoning_burst_starts_fresh_block() {
        let mut chat = ChatStream::new();
        chat.push_thinking("first");
        chat.collapse_thinking();
        chat.push_text("answer");
        chat.push_thinking("second");
        let lines = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert!(lines[0].contains("Thought")); // first burst collapsed
        assert!(lines.iter().any(|l| l.contains("second")));
    }

    /// A tasks.md document parsed into a checklist feeds the todo block —
    /// the same path `refresh_todos` uses after tool calls and on session
    /// load.
    #[test]
    fn set_todos_renders_parsed_session_checklist() {
        let checklist =
            crate::todo_parser::TodoChecklist::parse("- [x] write report\n- [ ] ship it\n");
        let mut chat = ChatStream::new();
        chat.push_text("earlier content");
        chat.end_text();
        chat.set_todos(checklist.items.clone());

        let items = chat
            .elements()
            .iter()
            .find_map(|element| match element {
                ChatElement::TodoBlock { items } => Some(items),
                _ => None,
            })
            .expect("a non-empty checklist should produce a todo block");
        assert_eq!(items, &checklist.items);

        // A subsequent parse replaces the previous block rather than
        // stacking a second one.
        chat.set_todos(Vec::new());
        assert!(
            chat.elements()
                .iter()
                .all(|element| !matches!(element, ChatElement::TodoBlock { .. })),
            "an empty checklist should remove the todo block"
        );
    }
}
