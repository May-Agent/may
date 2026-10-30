//! Chat stream display with inline tools and todos.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    /// Search completed with N results.
    SearchDone(usize),
    /// Tool call failed with an error message.
    Failed(String),
}

/// An image produced by a tool call, rendered as an overlay when the
/// terminal supports inline graphics.
#[derive(Debug, Clone)]
pub struct ToolCallImage {
    /// Key the image state is cached under.
    pub key: String,
    /// Label shown on the image placeholder header line.
    pub label: String,
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
    /// Web search query executed through the terminal command tool.
    Search {
        /// Search query text.
        query: String,
        /// Call status.
        status: ToolStatus,
        /// Result count once completed.
        results: Option<usize>,
        /// The underlying terminal call info.
        terminal_info: Option<TerminalCallInfo>,
    },
    /// URL fetch executed through the terminal command tool.
    Fetch {
        /// Fetched URL.
        url: String,
        /// Call status.
        status: ToolStatus,
        /// The underlying terminal call info.
        terminal_info: Option<TerminalCallInfo>,
    },
    /// Subagent task.
    Task {
        /// Subagent name.
        subagent: String,
        /// Prompt given to the subagent.
        prompt: String,
        /// Call status.
        status: ToolStatus,
        /// Terminal calls made by this subagent (most recent at end).
        terminal_calls: Vec<TerminalCallInfo>,
    },
    /// Direct terminal command.
    Terminal {
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
        /// Long commands/diffs start collapsed; a click toggles this.
        expanded: bool,
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
    },
}

/// Multi-line commands longer than this collapse to their first line.
const COLLAPSE_COMMAND_LINES: usize = 3;
/// Diffs with more lines than this collapse to their `±` summary.
const COLLAPSE_DIFF_LINES: usize = 8;

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
    /// Whether this call's body is long enough to be collapsible.
    #[must_use]
    pub fn is_collapsible(&self) -> bool {
        match self {
            Self::Terminal { command, diffs, .. } => {
                command.lines().count() > COLLAPSE_COMMAND_LINES
                    || diffs.iter().map(|d| d.text.lines().count()).sum::<usize>()
                        > COLLAPSE_DIFF_LINES
            }
            Self::Search { .. } | Self::Fetch { .. } | Self::Task { .. } | Self::Builtin { .. } => {
                false
            }
        }
    }

    /// Get the most recent terminal call for this tool.
    #[must_use]
    pub fn latest_terminal(&self) -> Option<&TerminalCallInfo> {
        match self {
            Self::Search { terminal_info, .. } | Self::Fetch { terminal_info, .. } => {
                terminal_info.as_ref()
            }
            Self::Task { terminal_calls, .. } => terminal_calls.last(),
            Self::Terminal { output, .. } => output.as_ref(),
            Self::Builtin { .. } => None,
        }
    }

    /// Get all terminal calls for this tool.
    #[must_use]
    pub fn all_terminal_calls(&self) -> Vec<&TerminalCallInfo> {
        match self {
            Self::Search { terminal_info, .. } | Self::Fetch { terminal_info, .. } => {
                terminal_info.iter().collect()
            }
            Self::Task { terminal_calls, .. } => terminal_calls.iter().collect(),
            Self::Terminal { output, .. } => output.iter().collect(),
            Self::Builtin { .. } => Vec::new(),
        }
    }

    /// Get status reference.
    #[must_use]
    pub const fn status(&self) -> &ToolStatus {
        match self {
            Self::Search { status, .. }
            | Self::Fetch { status, .. }
            | Self::Task { status, .. }
            | Self::Terminal { status, .. }
            | Self::Builtin { status, .. } => status,
        }
    }

    /// Check if this is a subagent task.
    #[must_use]
    pub const fn is_subagent(&self) -> bool {
        matches!(self, Self::Task { .. })
    }

    /// Get a short type name for display.
    #[must_use]
    pub fn type_name(&self) -> &str {
        match self {
            Self::Search { .. } => "search",
            Self::Fetch { .. } => "fetch",
            Self::Task { subagent, .. } => subagent,
            Self::Terminal { .. } => "terminal",
            Self::Builtin { tool, .. } => tool,
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
            },
        });
    }

    /// Mark a provider-hosted call complete. Providers that emit a result
    /// without a matching call event get a completed entry appended instead.
    pub fn finish_builtin(&mut self, id: &str, tool: &str, result: String) {
        for elem in &mut self.elements {
            if let ChatElement::ToolCall {
                id: tool_id,
                display: ToolDisplay::Builtin { status, output, .. },
            } = elem
                && tool_id == id
            {
                *status = ToolStatus::Done;
                *output = Some(result);
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
            },
        });
    }

    /// Update a tool's status and output by ID.
    pub fn update_tool_with_output(
        &mut self,
        id: &str,
        status: ToolStatus,
        results: Option<usize>,
        output: Option<&str>,
    ) {
        for elem in &mut self.elements {
            if let ChatElement::ToolCall {
                id: tool_id,
                display,
            } = elem
                && tool_id == id
            {
                match display {
                    ToolDisplay::Search {
                        status: s,
                        results: r,
                        terminal_info,
                        ..
                    } => {
                        *s = status;
                        *r = results;
                        if let Some(out) = output
                            && let Some(info) = terminal_info
                        {
                            info.output.clear();
                            info.output.push_str(out);
                        }
                    }
                    ToolDisplay::Fetch {
                        status: s,
                        terminal_info,
                        ..
                    } => {
                        *s = status;
                        if let Some(out) = output
                            && let Some(info) = terminal_info
                        {
                            info.output.clear();
                            info.output.push_str(out);
                        }
                    }
                    ToolDisplay::Task { status: s, .. } => {
                        *s = status;
                        // For tasks, output is parsed to extract terminal calls
                        // This is handled separately via update_subagent_terminal
                    }
                    ToolDisplay::Terminal {
                        status: s,
                        command,
                        output: out,
                        ..
                    } => {
                        *s = status;
                        if let Some(output_text) = output {
                            *out = Some(TerminalCallInfo {
                                command: command.clone(),
                                output: output_text.to_string(),
                                output_file: None,
                                image: None,
                            });
                        }
                    }
                    ToolDisplay::Builtin {
                        status: s,
                        output: out,
                        ..
                    } => {
                        *s = status;
                        if let Some(output_text) = output {
                            *out = Some(output_text.to_string());
                        }
                    }
                }
                break;
            }
        }
    }

    /// Flip the expanded state of a collapsible element (tool call or
    /// thinking block).
    pub fn toggle_expanded(&mut self, id: &str) {
        for elem in &mut self.elements {
            match elem {
                ChatElement::ToolCall {
                    id: tool_id,
                    display,
                } if tool_id == id && display.is_collapsible() => {
                    if let ToolDisplay::Terminal { expanded, .. } = display {
                        *expanded = !*expanded;
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
                    ToolDisplay::Search { terminal_info, .. }
                    | ToolDisplay::Fetch { terminal_info, .. } => {
                        if let Some(info) = terminal_info {
                            info.image = Some(image);
                        }
                    }
                    ToolDisplay::Task { terminal_calls, .. } => {
                        if let Some(latest) = terminal_calls.last_mut() {
                            latest.image = Some(image);
                        }
                    }
                    ToolDisplay::Terminal { output, .. } => {
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

    /// Add a terminal call to a subagent task.
    pub fn update_subagent_terminal(&mut self, id: &str, terminal_info: TerminalCallInfo) {
        for elem in &mut self.elements {
            if let ChatElement::ToolCall {
                id: tool_id,
                display: ToolDisplay::Task { terminal_calls, .. },
            } = elem
                && tool_id == id
            {
                terminal_calls.push(terminal_info);
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
    /// Queue indicator context, when messages are queued.
    pub queue_ctx: Option<QueueContext<'a>>,
    /// Status bar context, when the bar is shown.
    pub status_ctx: Option<StatusContext<'a>>,
    /// Whether terminal supports image rendering
    pub images_supported: bool,
    /// Output: accumulated image positions (url, `line_index`) for overlay rendering
    pub image_positions_out: Option<&'a mut Vec<(String, usize)>>,
}

impl<'a> From<UnifiedRenderContext<'a>> for RenderContext<'a> {
    fn from(ctx: UnifiedRenderContext<'a>) -> Self {
        Self {
            selected_tool: ctx.selected_tool,
            content_height_out: ctx.content_height_out,
            view_height_out: ctx.view_height_out,
            input_ctx: ctx.input_ctx,
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
            ChatElement::ToolCall { id, display } if display.is_collapsible() => Some(id.clone()),
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

/// Push a `🖼` image placeholder block: a header line followed by
/// `IMAGE_HEIGHT - 1` blank rows reserved for the graphics overlay.
/// Returns the header row index, which the overlay uses as its anchor.
fn push_image_rows(lines: &mut Vec<Line<'_>>, label: &str) -> usize {
    let index = lines.len();
    lines.push(Line::from(vec![
        Span::styled("🖼 ", Style::default()),
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    for _ in 1..IMAGE_HEIGHT {
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
    positions: Option<&mut Vec<(String, usize)>>,
) {
    if images_supported && let Attachment::Image { path, .. } = attachment {
        let label = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image");
        let index = push_image_rows(lines, label);
        if let Some(key) = attachment.image_cache_key()
            && let Some(positions) = positions
        {
            positions.push((key, index));
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
    positions: &mut Vec<(String, usize)>,
) {
    if !images_supported {
        return;
    }
    if let Some(image) = info.and_then(|info| info.image.as_ref()) {
        let index = push_image_rows(lines, &image.label);
        positions.push((image.key.clone(), index));
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

/// Plain-text mirror of `render_tool_inline` for one tool call.
fn plain_tool_lines(display: &ToolDisplay, lines: &mut Vec<Line<'static>>, images_supported: bool) {
    match display {
        ToolDisplay::Search {
            query,
            status,
            results,
            terminal_info,
        } => {
            let status_text = match status {
                ToolStatus::Running => "...".to_string(),
                ToolStatus::Done => results.map_or(String::new(), |n| format!("{n} results")),
                ToolStatus::SearchDone(n) => format!("{n} results"),
                ToolStatus::Failed(e) => format!("failed: {e}"),
            };
            let query_display = if status_text.is_empty() {
                format!("\"{query}\"")
            } else {
                format!("\"{query}\" {status_text}")
            };
            lines.push(Line::from(format!("  [search] {query_display}")));
            if let Some(info) = terminal_info
                && !info.output.is_empty()
            {
                for line in info.output_preview(2).lines() {
                    lines.push(Line::from(format!("    {line}")));
                }
            }
            if images_supported
                && let Some(image) = terminal_info.as_ref().and_then(|info| info.image.as_ref())
            {
                push_image_rows(lines, &image.label);
            }
        }
        ToolDisplay::Fetch {
            url,
            status,
            terminal_info,
        } => {
            let status_text = match status {
                ToolStatus::Running => "...",
                ToolStatus::Done | ToolStatus::SearchDone(_) => "",
                ToolStatus::Failed(_) => "failed",
            };
            let url_display = if status_text.is_empty() {
                url.clone()
            } else {
                format!("{url} {status_text}")
            };
            lines.push(Line::from(format!("  [fetch] {url_display}")));
            if let Some(info) = terminal_info
                && !info.output.is_empty()
            {
                for line in info.output_preview(2).lines() {
                    lines.push(Line::from(format!("    {line}")));
                }
            }
            if images_supported
                && let Some(image) = terminal_info.as_ref().and_then(|info| info.image.as_ref())
            {
                push_image_rows(lines, &image.label);
            }
        }
        ToolDisplay::Task {
            subagent,
            prompt,
            status,
            terminal_calls,
        } => {
            let status_text = match status {
                ToolStatus::Running => "...",
                ToolStatus::Done | ToolStatus::SearchDone(_) => "",
                ToolStatus::Failed(_) => "failed",
            };
            let prompt_preview = match prompt.char_indices().nth(50) {
                Some((pos, _)) => format!("{}...", &prompt[..pos]),
                None => prompt.clone(),
            };
            let prompt_display = if status_text.is_empty() {
                prompt_preview
            } else {
                format!("{prompt_preview} {status_text}")
            };
            lines.push(Line::from(format!(
                "  [subagent:{subagent}] {prompt_display}"
            )));
            if let Some(latest) = terminal_calls.last() {
                lines.push(Line::from(format!("    $ {}", latest.command_preview(60))));
                if !latest.output.is_empty() {
                    for line in latest.output_preview(2).lines() {
                        lines.push(Line::from(format!("      {line}")));
                    }
                }
                if images_supported && let Some(image) = latest.image.as_ref() {
                    push_image_rows(lines, &image.label);
                }
            }
        }
        ToolDisplay::Terminal {
            command,
            mode,
            status,
            output,
            diffs,
            expanded,
        } => {
            let status_text = match status {
                ToolStatus::Running => Some("running".to_string()),
                ToolStatus::Done => None,
                ToolStatus::SearchDone(n) => Some(format!("{n} results")),
                ToolStatus::Failed(e) => Some(format!("failed: {e}")),
            };
            let mode_prefix = terminal_mode_marker(*mode);
            let mut command_lines = command.lines();
            if let Some(first) = command_lines.next() {
                lines.push(Line::from(format!("  {mode_prefix}$ {first}")));
                let hidden = command.lines().count() - 1;
                if !*expanded && command.lines().count() > COLLAPSE_COMMAND_LINES {
                    lines.push(Line::from(format!(
                        "  ▸ … {hidden} more lines — click to expand"
                    )));
                } else {
                    for line in command_lines {
                        lines.push(Line::from(format!("  │ {line}")));
                    }
                }
            } else {
                lines.push(Line::from(format!("  {mode_prefix}$")));
            }
            if let Some(status) = status_text {
                lines.push(Line::from(format!("  ↳ {status}")));
            }
            if let Some(info) = output
                && !info.output.is_empty()
            {
                for line in info.output_preview(3).lines() {
                    lines.push(Line::from(format!("  │ {line}")));
                }
            }
            if images_supported
                && let Some(image) = output.as_ref().and_then(|info| info.image.as_ref())
            {
                push_image_rows(lines, &image.label);
            }
            plain_diff_lines(diffs, *expanded, lines);
        }
        ToolDisplay::Builtin {
            tool,
            detail,
            status,
            output,
        } => {
            let status_text = match status {
                ToolStatus::Running => "running...".to_string(),
                ToolStatus::Done => String::new(),
                ToolStatus::SearchDone(n) => format!("{n} results"),
                ToolStatus::Failed(e) => format!("failed: {e}"),
            };
            let header = if status_text.is_empty() {
                format!("  [{tool}] {detail}")
            } else {
                format!("  [{tool}] {detail} {status_text}")
            };
            lines.push(Line::from(header.trim_end().to_string()));
            if let Some(out) = output {
                for line in out.lines().take(3) {
                    lines.push(Line::from(format!("    {line}")));
                }
            }
        }
    }
}

/// Plain-text mirror of `render_diff_preview`.
fn plain_diff_lines(
    diffs: &[crate::file_tracker::RootDiff],
    expanded: bool,
    lines: &mut Vec<Line<'static>>,
) {
    const PREVIEW_LINES: usize = 8;
    let total: usize = diffs.iter().map(|d| d.text.lines().count()).sum();
    let collapsed = !expanded && total > COLLAPSE_DIFF_LINES;

    for (index, diff) in diffs.iter().enumerate() {
        let summary = format!(
            "± {} file{} (+{} −{}){}",
            diff.files,
            if diff.files == 1 { "" } else { "s" },
            diff.additions,
            diff.deletions,
            if collapsed { " ▸" } else { "" },
        );
        if diffs.len() > 1 {
            lines.push(Line::from(format!(
                "  {summary} in {}",
                diff.root.display()
            )));
        } else {
            lines.push(Line::from(format!("  {summary}")));
        }
        if collapsed {
            continue;
        }
        let count = diff.text.lines().count();
        for line in diff.text.lines().take(PREVIEW_LINES) {
            lines.push(Line::from(format!("  │ {line}")));
        }
        if count > PREVIEW_LINES {
            lines.push(Line::from(format!(
                "  │ … {} more diff lines",
                count - PREVIEW_LINES
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
    queue_ctx: Option<QueueContext<'a>>,
    status_ctx: Option<StatusContext<'a>>,
    images_supported: bool,
    image_positions_out: Option<&'a mut Vec<(String, usize)>>,
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
        queue_ctx,
        status_ctx,
        images_supported,
        image_positions_out,
    } = ctx;

    let mut lines: Vec<Line> = Vec::new();
    let mut collected_image_positions: Vec<(String, usize)> = Vec::new();

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
                for (url, pos) in md_result.image_positions {
                    collected_image_positions.push((url, line_offset + pos));
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
    if let Some(ctx) = input_ctx {
        render_input_line(
            &mut lines,
            &ctx,
            images_supported,
            Some(&mut collected_image_positions),
        );
    }

    // Render status bar if provided
    if let Some(ref ctx) = status_ctx {
        render_status_line(&mut lines, ctx, area.width as usize);
    }

    // Export collected image positions for overlay rendering
    if let Some(out) = image_positions_out {
        out.clear();
        out.extend(collected_image_positions);
    }

    stats.lines_build = build_start.elapsed();

    // Calculate dimensions using VISUAL line count (accounting for wrap)
    let view_height = area.height as usize;
    let render_width = area.width as usize;
    let wrap_start = std::time::Instant::now();
    let visual_content_height = total_wrapped_lines(&lines, render_width);
    stats.wrap_count = wrap_start.elapsed();
    stats.content_height = visual_content_height;

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
    let (scroll_y, final_lines) = if visual_content_height < view_height {
        let padding = view_height - visual_content_height;
        let mut padded = Vec::with_capacity(padding + lines.len());
        for _ in 0..padding {
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
    stats.widget = widget_start.elapsed();
    stats
}

/// Convert a character index to a byte index in a string.
fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(i, _)| i)
}

/// Render the input line into the lines vector. Attached images get a
/// placeholder block above the input row — the overlay paints the preview
/// into it — when the terminal supports graphics.
fn render_input_line(
    lines: &mut Vec<Line<'_>>,
    ctx: &InputContext<'_>,
    images_supported: bool,
    mut image_positions: Option<&mut Vec<(String, usize)>>,
) {
    if images_supported {
        for att in ctx.attachments {
            if let Attachment::Image { path, .. } = att {
                let label = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("image");
                let index = push_image_rows(lines, label);
                if let Some(key) = att.image_cache_key()
                    && let Some(positions) = image_positions.as_mut()
                {
                    positions.push((key, index));
                }
            }
        }
    }
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

    // Input text with cursor (convert char index to byte index)
    let byte_idx = char_to_byte_index(ctx.input, ctx.cursor);
    let (before, after) = ctx.input.split_at(byte_idx);
    spans.push(Span::raw(before.to_string()));
    spans.push(Span::styled("│", Style::default().fg(Color::Yellow)));
    spans.push(Span::raw(after.to_string()));

    lines.push(Line::from(spans));
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

fn render_tool_inline<'a>(
    lines: &mut Vec<Line<'a>>,
    display: &'a ToolDisplay,
    is_selected: bool,
    images_supported: bool,
    image_positions: &mut Vec<(String, usize)>,
) {
    let selection_marker = if is_selected { "▶ " } else { "  " };
    let selection_style = if is_selected {
        Style::default().bg(Color::DarkGray)
    } else {
        Style::default()
    };

    match display {
        ToolDisplay::Search {
            query,
            status,
            results,
            terminal_info,
        } => {
            let status_text = match status {
                ToolStatus::Running => "...".to_string(),
                ToolStatus::Done => results.map_or(String::new(), |n| format!("{n} results")),
                ToolStatus::SearchDone(n) => format!("{n} results"),
                ToolStatus::Failed(e) => format!("failed: {e}"),
            };
            let query_display = if status_text.is_empty() {
                format!("\"{query}\"")
            } else {
                format!("\"{query}\" {status_text}")
            };
            lines.push(Line::from(vec![
                Span::raw(selection_marker),
                Span::styled("[search] ", Style::default().fg(Color::Blue)),
                Span::styled(query_display, selection_style),
            ]));

            // Show terminal output preview if available
            if let Some(info) = terminal_info
                && !info.output.is_empty()
            {
                let preview = info.output_preview(2);
                for line in preview.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("    {line}"),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            push_tool_call_image(
                lines,
                terminal_info.as_ref(),
                images_supported,
                image_positions,
            );
        }
        ToolDisplay::Fetch {
            url,
            status,
            terminal_info,
        } => {
            let status_text = match status {
                ToolStatus::Running => "...",
                ToolStatus::Done | ToolStatus::SearchDone(_) => "",
                ToolStatus::Failed(_) => "failed",
            };
            let url_display = if status_text.is_empty() {
                url.clone()
            } else {
                format!("{url} {status_text}")
            };
            lines.push(Line::from(vec![
                Span::raw(selection_marker),
                Span::styled("[fetch] ", Style::default().fg(Color::Cyan)),
                Span::styled(url_display, selection_style),
            ]));

            // Show terminal output preview if available
            if let Some(info) = terminal_info
                && !info.output.is_empty()
            {
                let preview = info.output_preview(2);
                for line in preview.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("    {line}"),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            push_tool_call_image(
                lines,
                terminal_info.as_ref(),
                images_supported,
                image_positions,
            );
        }
        ToolDisplay::Task {
            subagent,
            prompt,
            status,
            terminal_calls,
        } => {
            let status_text = match status {
                ToolStatus::Running => "...",
                ToolStatus::Done | ToolStatus::SearchDone(_) => "",
                ToolStatus::Failed(_) => "failed",
            };

            // Truncate prompt for display (unicode-safe)
            let prompt_preview = match prompt.char_indices().nth(50) {
                Some((pos, _)) => format!("{}...", &prompt[..pos]),
                None => prompt.clone(),
            };

            let prompt_display = if status_text.is_empty() {
                prompt_preview
            } else {
                format!("{prompt_preview} {status_text}")
            };

            lines.push(Line::from(vec![
                Span::raw(selection_marker),
                Span::styled(
                    format!("[subagent:{subagent}] "),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(prompt_display, selection_style),
            ]));

            // Show most recent terminal call for subagent
            if let Some(latest) = terminal_calls.last() {
                let cmd_preview = latest.command_preview(60);
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled("$ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(cmd_preview, Style::default().fg(Color::DarkGray)),
                ]));

                // Show output preview
                if !latest.output.is_empty() {
                    let preview = latest.output_preview(2);
                    for line in preview.lines() {
                        lines.push(Line::from(Span::styled(
                            format!("      {line}"),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
                push_tool_call_image(lines, Some(latest), images_supported, image_positions);
            }
        }
        ToolDisplay::Terminal {
            command,
            mode,
            status,
            output,
            diffs,
            expanded,
        } => {
            let status_text = match status {
                ToolStatus::Running => Some("running".to_string()),
                ToolStatus::Done => None,
                ToolStatus::SearchDone(n) => Some(format!("{n} results")),
                ToolStatus::Failed(e) => Some(format!("failed: {e}")),
            };

            let mode_prefix = terminal_mode_marker(*mode);

            let mut command_lines = command.lines();
            if let Some(first) = command_lines.next() {
                lines.push(Line::from(vec![
                    Span::raw(selection_marker),
                    Span::styled(mode_prefix, Style::default().fg(Color::Red).bold()),
                    Span::styled("$ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(first.to_string(), selection_style),
                ]));

                let hidden = command.lines().count() - 1;
                if !*expanded && command.lines().count() > COLLAPSE_COMMAND_LINES {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(
                            format!("▸ … {hidden} more lines — click to expand"),
                            Style::default().fg(Color::DarkGray).italic(),
                        ),
                    ]));
                } else {
                    for line in command_lines {
                        lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                            Span::styled(line.to_string(), selection_style),
                        ]));
                    }
                }
            } else {
                lines.push(Line::from(vec![
                    Span::raw(selection_marker),
                    Span::styled(mode_prefix, Style::default().fg(Color::Red).bold()),
                    Span::styled("$", Style::default().fg(Color::DarkGray)),
                ]));
            }

            if let Some(status) = status_text {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled("↳ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(status, Style::default().fg(Color::DarkGray)),
                ]));
            }

            if let Some(info) = output
                && !info.output.is_empty()
            {
                let preview = info.output_preview(3);
                for line in preview.lines() {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(line.to_string(), Style::default().fg(Color::DarkGray)),
                    ]));
                }
            }
            push_tool_call_image(lines, output.as_ref(), images_supported, image_positions);

            render_diff_preview(lines, diffs, *expanded);
        }
        ToolDisplay::Builtin {
            tool,
            detail,
            status,
            output,
        } => {
            let status_text = match status {
                ToolStatus::Running => "running...".to_string(),
                ToolStatus::Done => String::new(),
                ToolStatus::SearchDone(n) => format!("{n} results"),
                ToolStatus::Failed(e) => format!("failed: {e}"),
            };
            let detail_display = if status_text.is_empty() {
                detail.clone()
            } else {
                format!("{detail} {status_text}")
            };
            lines.push(Line::from(vec![
                Span::raw(selection_marker),
                Span::styled(format!("[{tool}] "), Style::default().fg(Color::Magenta)),
                Span::styled(detail_display, selection_style),
            ]));
            if let Some(out) = output {
                for line in out.lines().take(3) {
                    lines.push(Line::from(Span::styled(
                        format!("    {line}"),
                        Style::default().fg(Color::DarkGray),
                    )));
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
/// under an inline terminal tool call. Long diffs collapse to their
/// summary until `expanded`.
fn render_diff_preview<'a>(
    lines: &mut Vec<Line<'a>>,
    diffs: &'a [crate::file_tracker::RootDiff],
    expanded: bool,
) {
    const PREVIEW_LINES: usize = 8;

    let total: usize = diffs.iter().map(|d| d.text.lines().count()).sum();
    let collapsed = !expanded && total > COLLAPSE_DIFF_LINES;

    for (index, diff) in diffs.iter().enumerate() {
        let summary = format!(
            "± {} file{} (+{} −{}){}",
            diff.files,
            if diff.files == 1 { "" } else { "s" },
            diff.additions,
            diff.deletions,
            if collapsed { " ▸" } else { "" },
        );
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                if diffs.len() > 1 {
                    format!("{summary} in {}", diff.root.display())
                } else {
                    summary
                },
                Style::default().fg(Color::Yellow),
            ),
        ]));
        if collapsed {
            continue;
        }

        let total = diff.text.lines().count();
        for line in diff.text.lines().take(PREVIEW_LINES) {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(line.to_string(), diff_line_style(line)),
            ]));
        }
        if total > PREVIEW_LINES {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("… {} more diff lines", total - PREVIEW_LINES),
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
        ToolDisplay::Search { query, .. } => format!("Search: \"{query}\""),
        ToolDisplay::Fetch { url, .. } => format!("Fetch: {url}"),
        ToolDisplay::Task {
            subagent, prompt, ..
        } => format!("Subagent:{subagent} - {prompt}"),
        ToolDisplay::Terminal { command, mode, .. } => {
            format!("{}$ {command}", terminal_mode_marker(*mode))
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
        ToolStatus::SearchDone(n) => Span::styled(
            format!("Status: {n} results"),
            Style::default().fg(Color::Green),
        ),
        ToolStatus::Failed(e) => Span::styled(
            format!("Status: Failed - {e}"),
            Style::default().fg(Color::Red),
        ),
    };
    lines.push(Line::from(status_line));
    lines.push(Line::from(""));

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
        ChatElement, ChatStream, InputContext, ToolCallImage, ToolDisplay, ToolStatus,
        build_plain_wrapped_lines_unified, render_input_line, render_tool_inline,
        terminal_mode_marker,
    };
    use crate::tui::queue::Attachment;
    use crate::tui::ui::streaming_markdown::IMAGE_HEIGHT;
    use aither_sandbox::TerminalExecutionMode;
    use ratatui::text::Line;
    use std::path::PathBuf;

    #[test]
    fn live_output_appends_to_running_terminal_call() {
        let mut stream = ChatStream::new();
        stream.push_tool(
            "call-1".to_string(),
            ToolDisplay::Terminal {
                command: "echo hi".to_string(),
                mode: TerminalExecutionMode::Default,
                status: ToolStatus::Running,
                output: None,
                diffs: Vec::new(),
                expanded: false,
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
        stream.update_tool_with_output("call-1", ToolStatus::Done, None, Some("hello\nworld\n"));
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
                command: "echo hi".to_string(),
                mode,
                status: ToolStatus::Done,
                output: None,
                diffs: Vec::new(),
                expanded: false,
            };
            let mut lines: Vec<Line> = Vec::new();
            let mut positions = Vec::new();
            render_tool_inline(&mut lines, &display, false, true, &mut positions);
            let rendered: String = lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert_eq!(rendered, format!("  {marker}$ echo hi"));
        }
    }

    fn terminal_display(command: &str) -> ToolDisplay {
        ToolDisplay::Terminal {
            command: command.to_string(),
            mode: TerminalExecutionMode::Default,
            status: ToolStatus::Done,
            output: None,
            diffs: Vec::new(),
            expanded: false,
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
        chat.push_text("终端通道工作正常。要做更具体的测试吗？比如：\n\n- 并发/时序测试 — 比如先跑 `sleep 5` 再跑 `sleep 10`，验证顺序与完成回调\n- 后台任务测试 — `terminal_background` + `terminal_read` 增量读取\n- 文件写入测试 — 在 `artifacts/` 或 `data/` 里写文件并校验持久性\n- 特定命令 — 你直接给出要跑的命令，我执行后按你指定的格式回传（比如只回 `finished`）\n\n直接说要测什么就行。\n");
        chat.push_user("你可以跑后台命令吗", Vec::new(), 0);
        chat.push_text("可以，用 `terminal_background` 就行。我跑一个演示：\n");
        chat.push_tool(
            "call-1".to_string(),
            ToolDisplay::Terminal {
                command: "terminal_background {…}".to_string(),
                mode: TerminalExecutionMode::Default,
                status: ToolStatus::Done,
                output: Some(
                    super::TerminalCallInfo::new("terminal_background".to_string()).with_output(
                        "tick 1 at 21:20:24\ntick 2 at 21:20:25\ntick 3 at 21:20:26".to_string(),
                    ),
                ),
                diffs: Vec::new(),
                expanded: false,
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
    fn collapsible_only_when_long() {
        assert!(!terminal_display("echo hi").is_collapsible());
        assert!(!terminal_display("a\nb\nc").is_collapsible());
        assert!(terminal_display("a\nb\nc\nd").is_collapsible());
    }

    #[test]
    fn collapsed_command_shows_hint_then_expands() {
        let mut display = terminal_display("one\ntwo\nthree\nfour\nfive");
        let text = {
            let mut lines: Vec<Line> = Vec::new();
            let mut positions = Vec::new();
            render_tool_inline(&mut lines, &display, false, true, &mut positions);
            rendered_text(&lines)
        };
        assert_eq!(text[0], "  $ one");
        assert!(text[1].contains("4 more lines"));
        assert_eq!(text.len(), 2);

        let ToolDisplay::Terminal { expanded, .. } = &mut display else {
            unreachable!()
        };
        *expanded = true;
        let text = {
            let mut lines: Vec<Line> = Vec::new();
            let mut positions = Vec::new();
            render_tool_inline(&mut lines, &display, false, true, &mut positions);
            rendered_text(&lines)
        };
        assert_eq!(text.len(), 5);
        assert_eq!(text[4], "  │ five");
    }

    #[test]
    fn collapsed_diff_shows_summary_only() {
        let body = "+line\n".repeat(12);
        let display = ToolDisplay::Terminal {
            command: "echo hi".to_string(),
            mode: TerminalExecutionMode::Default,
            status: ToolStatus::Done,
            output: None,
            diffs: vec![crate::file_tracker::RootDiff {
                root: std::path::PathBuf::from("/tmp/root"),
                text: body,
                files: 1,
                additions: 12,
                deletions: 0,
            }],
            expanded: false,
        };
        assert!(display.is_collapsible());
        let mut lines: Vec<Line> = Vec::new();
        let mut positions = Vec::new();
        render_tool_inline(&mut lines, &display, false, true, &mut positions);
        let text = rendered_text(&lines);
        // command line + summary with ▸ marker, no diff body
        assert_eq!(text.len(), 2);
        assert!(text[1].contains("± 1 file (+12 −0) ▸"));
    }

    #[test]
    fn toggle_expands_and_collapses() {
        let mut chat = ChatStream::new();
        chat.push_tool("t1".to_string(), terminal_display("a\nb\nc\nd"));
        assert_eq!(
            build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false).len(),
            2 // "$ a" + collapse hint
        );
        chat.toggle_expanded("t1");
        assert_eq!(
            build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false).len(),
            4
        );
        chat.toggle_expanded("t1");
        assert_eq!(
            build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false).len(),
            2
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
        assert!(lines[0].spans[0].content.contains("🖼"));
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
        let ToolDisplay::Terminal { output, .. } = &mut display else {
            unreachable!()
        };
        *output = Some(
            super::TerminalCallInfo::new("screenshot".to_string()).with_image(ToolCallImage {
                key: "tool-image:t1".to_string(),
                label: "shot.png".to_string(),
            }),
        );

        let mut lines: Vec<Line> = Vec::new();
        let mut positions = Vec::new();
        render_tool_inline(&mut lines, &display, false, true, &mut positions);

        assert_eq!(lines.len(), 1 + IMAGE_HEIGHT); // "$ screenshot" + image block
        assert_eq!(positions, vec![("tool-image:t1".to_string(), 1)]);

        // Without graphics support the block collapses to nothing — the text
        // output already carries the [image] marker.
        let mut lines: Vec<Line> = Vec::new();
        let mut positions = Vec::new();
        render_tool_inline(&mut lines, &display, false, false, &mut positions);
        assert_eq!(lines.len(), 1);
        assert_eq!(positions, Vec::<(String, usize)>::new());
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
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].element_id, "t1");
        assert_eq!(regions[0].start, 0);
        assert_eq!(regions[0].end, 2);
        assert!(lines.len() >= 3);
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
