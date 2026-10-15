//! Chat stream display with inline tools and todos.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use aither_sandbox::TerminalMode;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

use super::super::queue::Attachment;
use super::markdown;

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

/// Information about a terminal command execution.
#[derive(Debug, Clone)]
pub struct TerminalCallInfo {
    /// The command that was executed.
    pub command: String,
    /// The output (may be truncated, matching what agent sees).
    pub output: String,
    /// Path to full output if truncated/offloaded.
    pub output_file: Option<PathBuf>,
}

impl TerminalCallInfo {
    /// Create a new terminal call info.
    #[must_use]
    pub const fn new(command: String) -> Self {
        Self {
            command,
            output: String::new(),
            output_file: None,
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
        /// Execution mode.
        mode: TerminalMode,
        /// Call status.
        status: ToolStatus,
        /// The output from this command.
        output: Option<TerminalCallInfo>,
    },
}

impl ToolDisplay {
    /// Get the most recent terminal call for this tool.
    #[must_use]
    pub fn latest_terminal(&self) -> Option<&TerminalCallInfo> {
        match self {
            Self::Search { terminal_info, .. } | Self::Fetch { terminal_info, .. } => {
                terminal_info.as_ref()
            }
            Self::Task { terminal_calls, .. } => terminal_calls.last(),
            Self::Terminal { output, .. } => output.as_ref(),
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
        }
    }

    /// Get status reference.
    #[must_use]
    pub const fn status(&self) -> &ToolStatus {
        match self {
            Self::Search { status, .. }
            | Self::Fetch { status, .. }
            | Self::Task { status, .. }
            | Self::Terminal { status, .. } => status,
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
        }
    }

    /// Check if this tool call is complete (not running).
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        !matches!(self.status(), ToolStatus::Running)
    }
}

/// Status of a todo item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    /// Todo not started.
    Pending,
    /// Todo in progress.
    InProgress,
    /// Todo completed.
    Completed,
}

/// A todo item.
#[derive(Debug, Clone)]
pub struct TodoItem {
    /// Todo text.
    pub content: String,
    /// Item status.
    pub status: TodoStatus,
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
        items: Vec<TodoItem>,
    },
    /// Error message
    Error {
        /// Error text shown in the stream.
        message: String,
    },
    /// Loading indicator (waiting for LLM response)
    Loading {
        /// Spinner frame counter.
        frame: usize,
    },
    /// Thinking indicator (for thinking models)
    Thinking {
        /// Seconds spent thinking.
        seconds: u64,
    },
}

impl ChatElement {}

/// The chat stream containing all elements.
#[derive(Debug, Default)]
pub struct ChatStream {
    elements: Vec<ChatElement>,
}

impl ChatStream {
    /// Create a new empty chat stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a user message.
    pub fn push_user(&mut self, content: &str, attachments: Vec<Attachment>) {
        self.elements.push(ChatElement::UserMessage {
            content: content.to_string(),
            attachments,
        });
    }

    /// Add an error message.
    pub fn push_error(&mut self, message: &str) {
        self.elements.push(ChatElement::Error {
            message: message.to_string(),
        });
    }

    /// Start loading indicator.
    pub fn start_loading(&mut self) {
        self.elements.push(ChatElement::Loading { frame: 0 });
    }

    /// Update loading animation frame.
    pub fn tick_loading(&mut self) {
        if let Some(ChatElement::Loading { frame }) = self.elements.last_mut() {
            *frame = (*frame + 1) % 4;
        }
    }

    /// Start thinking indicator.
    pub fn start_thinking(&mut self) {
        // Remove loading if present
        if matches!(self.elements.last(), Some(ChatElement::Loading { .. })) {
            self.elements.pop();
        }
        self.elements.push(ChatElement::Thinking { seconds: 0 });
    }

    /// Update thinking time.
    pub fn update_thinking(&mut self, seconds: u64) {
        if let Some(ChatElement::Thinking { seconds: s }) = self.elements.last_mut() {
            *s = seconds;
        }
    }

    /// End loading/thinking state (called when first text arrives).
    pub fn end_loading(&mut self) {
        // Remove loading or thinking element if present
        if matches!(
            self.elements.last(),
            Some(ChatElement::Loading { .. } | ChatElement::Thinking { .. })
        ) {
            self.elements.pop();
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
                            });
                        }
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
    pub fn set_todos(&mut self, items: Vec<TodoItem>) {
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

/// Status bar rendering context for combined view.
#[derive(Debug)]
pub struct StatusContext<'a> {
    /// Model display string.
    pub model: &'a str,
    /// Token usage info (`prompt_tokens`, `context_window`).
    pub usage: Option<(u64, Option<u64>)>,
    /// Queued message count.
    pub queue_count: usize,
    /// Background task count.
    pub background_tasks: usize,
    /// Whether currently processing.
    pub processing: bool,
    /// Optional status message.
    pub status_message: Option<&'a str>,
    /// Current profile name (shown in status bar).
    pub profile_name: Option<&'a str>,
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
) {
    render_impl(frame, area, chat, scroll, ctx.into());
}

/// Build plain wrapped lines for selection/copy from unified viewport content.
#[must_use]
pub fn build_plain_wrapped_lines_unified(
    chat: &ChatStream,
    width: u16,
    queue_ctx: Option<&QueueContext<'_>>,
    input_ctx: Option<InputContext<'_>>,
    status_ctx: Option<&StatusContext<'_>>,
) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }

    let mut lines: Vec<Line<'static>> = build_plain_chat_lines(chat);

    if let Some(ctx) = queue_ctx
        && !ctx.messages.is_empty()
    {
        render_queue_lines(&mut lines, ctx, width);
    }

    if let Some(ctx) = input_ctx {
        render_input_line(&mut lines, &ctx);
    }

    if let Some(ctx) = status_ctx {
        render_status_line(&mut lines, ctx);
    }

    paragraph_to_wrapped_plain(lines, width)
}

fn build_plain_chat_lines(chat: &ChatStream) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for elem in chat.active_elements() {
        match elem {
            ChatElement::UserMessage { content, .. } => {
                for (i, line) in content.lines().enumerate() {
                    if i == 0 {
                        lines.push(Line::from(format!("> {line}")));
                    } else {
                        lines.push(Line::from(format!("  {line}")));
                    }
                }
                lines.push(Line::from(""));
            }
            ChatElement::AssistantText { content, .. } => {
                lines.extend(markdown::render_markdown_with_config(
                    content,
                    markdown::ImageSupport::Disabled,
                ));
                lines.push(Line::from(""));
            }
            ChatElement::Error { message } => {
                lines.push(Line::from(format!("error: {message}")));
                lines.push(Line::from(""));
            }
            ChatElement::ToolCall { .. }
            | ChatElement::TodoBlock { .. }
            | ChatElement::Loading { .. }
            | ChatElement::Thinking { .. } => {}
        }
    }
    lines
}

fn paragraph_to_wrapped_plain(lines: Vec<Line>, width: u16) -> Vec<String> {
    let mut out = Vec::new();
    let max_width = width as usize;

    for line in lines {
        let plain = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        let mut wrapped = wrap_plain_line(&plain, max_width);
        out.append(&mut wrapped);
    }

    out
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
) {
    // Guard against zero-dimension areas to prevent overflow
    if area.width < 3 || area.height < 3 {
        return;
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

    for elem in chat.active_elements() {
        match elem {
            ChatElement::UserMessage {
                content,
                attachments,
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

                // Attachments
                for att in attachments {
                    let att_text = match att {
                        Attachment::Image { .. } => "[image]".to_string(),
                        Attachment::FileReference { path } => {
                            format!("@{}", path.display())
                        }
                        Attachment::PastedText {
                            content,
                            saved_path,
                        } => {
                            let char_count = content.chars().count();
                            saved_path.as_ref().map_or_else(
                                || format!("[pasted text: {char_count} chars]"),
                                |path| {
                                    format!(
                                        "[pasted text: {char_count} chars -> {}]",
                                        path.display()
                                    )
                                },
                            )
                        }
                    };
                    lines.push(Line::from(Span::styled(
                        att_text,
                        Style::default().fg(Color::DarkGray),
                    )));
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
                render_tool_inline(&mut lines, display, is_selected);
            }

            ChatElement::TodoBlock { items } => {
                lines.push(Line::from(Span::styled(
                    "[todo] ────────────────────────────",
                    Style::default().fg(Color::Green),
                )));

                for item in items {
                    let (marker, style) = match item.status {
                        TodoStatus::Pending => ("[ ]", Style::default()),
                        TodoStatus::InProgress => ("[>]", Style::default().fg(Color::Yellow)),
                        TodoStatus::Completed => ("[x]", Style::default().fg(Color::DarkGray)),
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {marker} "), style),
                        Span::styled(&item.content, style),
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

            ChatElement::Loading { frame } => {
                // Animated loading indicator
                let spinner = match frame % 4 {
                    0 => "⠋",
                    1 => "⠙",
                    2 => "⠹",
                    _ => "⠸",
                };
                lines.push(Line::from(Span::styled(
                    format!("{spinner} Waiting for response..."),
                    Style::default().fg(Color::DarkGray),
                )));
            }

            ChatElement::Thinking { seconds } => {
                // Thinking indicator with time
                let text = if *seconds == 0 {
                    "◐ Thinking...".to_string()
                } else {
                    format!("◐ Thinking... ({seconds} sec)")
                };
                lines.push(Line::from(Span::styled(
                    text,
                    Style::default().fg(Color::Magenta),
                )));
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
        render_input_line(&mut lines, &ctx);
    }

    // Render status bar if provided
    if let Some(ref ctx) = status_ctx {
        render_status_line(&mut lines, ctx);
    }

    // Export collected image positions for overlay rendering
    if let Some(out) = image_positions_out {
        out.clear();
        out.extend(collected_image_positions);
    }

    // Calculate dimensions using VISUAL line count (accounting for wrap)
    let view_height = area.height as usize;
    let render_width = area.width as usize;
    let visual_content_height = total_wrapped_lines(&lines, render_width);

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

    frame.render_widget(Clear, area);
    let paragraph = Paragraph::new(final_lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0));
    frame.render_widget(paragraph, area);
}

/// Convert a character index to a byte index in a string.
fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(i, _)| i)
}

/// Render the input line into the lines vector.
fn render_input_line(lines: &mut Vec<Line<'_>>, ctx: &InputContext<'_>) {
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

/// Render the status bar into the lines vector.
fn render_status_line(lines: &mut Vec<Line<'_>>, ctx: &StatusContext<'_>) {
    // If there's a status message (like Ctrl+C hint), show it prominently
    if let Some(status) = ctx.status_message {
        lines.push(Line::from(Span::styled(
            status.to_string(),
            Style::default().fg(Color::Yellow),
        )));
        return;
    }

    let mut parts = Vec::new();

    // Show profile name if not the default "Default"
    if let Some(name) = ctx.profile_name
        && name != "Default"
    {
        parts.push(name.to_string());
    }

    parts.push(ctx.model.to_string());

    // Add token usage info
    if let Some((prompt_tokens, context_window)) = ctx.usage {
        let used = format_number(usize::try_from(prompt_tokens).unwrap_or(usize::MAX));
        if let Some(window) = context_window {
            parts.push(format!(
                "{}/{}",
                used,
                format_number(usize::try_from(window).unwrap_or(usize::MAX))
            ));
        } else {
            parts.push(format!("{used}tok"));
        }
    }

    if ctx.queue_count > 0 {
        parts.push(format!("Q:{}", ctx.queue_count));
    }

    if ctx.background_tasks > 0 {
        parts.push(format!("BG:{}", ctx.background_tasks));
    }

    if ctx.processing {
        parts.push("...".to_string());
    }

    let status_text = parts.join(" | ");

    lines.push(Line::from(Span::styled(
        status_text,
        Style::default().fg(Color::Cyan),
    )));
}

/// Format large numbers with K/M suffix for readability.
fn format_number(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Render a tool call inline with optional selection highlight.
fn render_tool_inline<'a>(lines: &mut Vec<Line<'a>>, display: &'a ToolDisplay, is_selected: bool) {
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
            }
        }
        ToolDisplay::Terminal {
            command,
            mode,
            status,
            output,
        } => {
            let status_text = match status {
                ToolStatus::Running => Some("running".to_string()),
                ToolStatus::Done => None,
                ToolStatus::SearchDone(n) => Some(format!("{n} results")),
                ToolStatus::Failed(e) => Some(format!("failed: {e}")),
            };

            let mode_prefix = match mode {
                TerminalMode::Unsafe => "[unsafe] ",
                TerminalMode::Sandboxed => "",
            };

            let mut command_lines = command.lines();
            if let Some(first) = command_lines.next() {
                lines.push(Line::from(vec![
                    Span::raw(selection_marker),
                    Span::styled(mode_prefix, Style::default().fg(Color::Red).bold()),
                    Span::styled("$ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(first.to_string(), selection_style),
                ]));

                for line in command_lines {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(line.to_string(), selection_style),
                    ]));
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
            let mode_prefix = match mode {
                TerminalMode::Unsafe => "[unsafe] ",
                TerminalMode::Sandboxed => "",
            };
            format!("{mode_prefix}$ {command}")
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
