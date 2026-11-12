//! Chat stream display with inline tools and todos.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use aither_agent::ContextWindowPhase;
use aither_sandbox::SandboxMode;
use askama::Template as _;

use crate::agent::cache_miss::CacheMissCause;
use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph},
};

use ratatui::Frame;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::super::agent_event_adapter::COMPACT_TOOL;
use super::super::grapheme::drawn_width;
use super::super::queue::Attachment;
use super::band;
use super::chat_layout::{self, ChatLayout};
use super::markdown;
use super::streaming_markdown::{IMAGE_HEIGHT, IncrementalMarkdown};
use crate::agent::ContextReport;
use crate::command_reading::{
    Card, CommandReading, DiffLine, DiffOp, FileChange, FileEdit, Intent, Link, RowChangeKind,
    RowView, SubagentState,
};
use crate::task_rows::{TaskRow, TaskSection};
use crate::tui::state::CompactionMeter;

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
        mode: SandboxMode,
        /// Name of the SSH server the command runs on; `None` for the home
        /// environment.
        location: Option<String>,
        /// How May reads the script; `None` while the script streams in
        /// and for `!` bash calls.
        reading: Option<CommandReading>,
        /// The card the call renders as, when it ran one of May's commands
        /// and the card represents what it did.
        card: Option<Box<Card>>,
        /// Call status.
        status: ToolStatus,
        /// The output from this command.
        output: Option<TerminalCallInfo>,
        /// File changes attributed to this call (one per tracked root).
        diffs: Vec<crate::file_tracker::RootDiff>,
        /// Expansion pinned by a click; `None` follows the automatic rule.
        pinned: Option<bool>,
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
    },
}

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
            Self::Action { chip: name, .. } | Self::Builtin { tool: name, .. }
                if is_one_row_tool(name) =>
            {
                false
            }
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

    /// Whether the body renders open: a click pin wins; otherwise the body
    /// is open while the call runs and folds to its headline when it ends.
    #[must_use]
    pub fn body_open(&self) -> bool {
        self.pinned()
            .unwrap_or_else(|| matches!(self.status(), ToolStatus::Running))
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

/// A reply translated into the communication language for display. The
/// translation is shown, marked as translated; a click on the marker swaps
/// in the original.
#[derive(Debug, Clone)]
pub struct ReplyTranslation {
    /// Stable ID for click hit-testing of the marker row.
    id: String,
    /// The reply translated by the fast model.
    text: String,
    /// Display name of the communication language.
    language: String,
    /// Whether the original is shown instead of the translation.
    show_original: bool,
}

impl ReplyTranslation {
    /// The marker row's text.
    fn marker(&self) -> String {
        if self.show_original {
            format!("⇄ Original reply (translated to {}) ▾", self.language)
        } else {
            format!("⇄ Translated to {} ▸ show original", self.language)
        }
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
        /// Message text: the original the model wrote.
        content: String,
        /// Whether tokens are still arriving.
        streaming: bool,
        /// The display translation of `content`, when the reply was not in
        /// the communication language.
        translation: Option<ReplyTranslation>,
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
        /// The rows of `tasks.md`.
        rows: Vec<TaskRow>,
    },
    /// Daemon-written catch-up card (persisted `recap` history row).
    Recap {
        /// The catch-up summary.
        summary: String,
        /// The suggested next step, when the recap named one.
        next_action: Option<String>,
    },
    /// Error message
    Error {
        /// Error text shown in the stream.
        message: String,
    },
    /// Warning for the user, such as an abnormally long CI job; never part
    /// of the agent's context.
    Warning {
        /// Warning text shown in the stream.
        message: String,
    },
    /// Welcome header: logo, name, version and working directory. Always
    /// the first element of a stream.
    Welcome {
        /// Working directory as shown, home written `~`.
        cwd: String,
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
        /// Seconds spent thinking. Sub-second phases keep the fraction.
        seconds: f64,
        /// Open while reasoning streams, or after the user reopens a header.
        expanded: bool,
        /// Still receiving the current burst. Live bodies are a 5-line tail
        /// window; a user-reopened block (`expanded` without `live`) is full.
        live: bool,
    },
    /// A finished turn's work — thinking bursts, tool calls and the
    /// intermediate messages between them — folded into one row when the
    /// turn ends. A click expands the whole group; a second folds it shut.
    WorkedFor {
        /// Stable ID for click hit-testing.
        id: String,
        /// Wall-clock seconds the turn ran.
        seconds: u64,
        /// The folded elements.
        elements: Vec<Self>,
        /// Open after a click; folded by default.
        expanded: bool,
    },
    /// `/context` report — estimated context-window usage per category.
    ContextReport(ContextReport),
    /// A `/usage` provider-usage report block.
    UsageReport(crate::agent::SessionUsageReport),
}

impl ChatElement {
    /// Toggle the collapsible element with `id`, recursing into the
    /// elements folded inside a "Worked for" group. Returns whether an
    /// element toggled.
    fn toggle_expanded(&mut self, id: &str) -> bool {
        match self {
            Self::ToolCall {
                id: tool_id,
                display,
            } if tool_id == id && display.expandable() => {
                let open = display.body_open();
                match display {
                    ToolDisplay::Terminal { pinned, .. }
                    | ToolDisplay::Action { pinned, .. }
                    | ToolDisplay::Builtin { pinned, .. } => *pinned = Some(!open),
                }
                true
            }
            Self::Thinking {
                id: think_id,
                expanded,
                content,
                ..
            } if think_id == id && !content.is_empty() => {
                *expanded = !*expanded;
                true
            }
            Self::AssistantText {
                translation: Some(translation),
                ..
            } if translation.id == id => {
                translation.show_original = !translation.show_original;
                true
            }
            Self::WorkedFor {
                id: work_id,
                expanded,
                elements,
                ..
            } => {
                if work_id == id {
                    *expanded = !*expanded;
                    return true;
                }
                elements.iter_mut().any(|inner| inner.toggle_expanded(id))
            }
            _ => false,
        }
    }
}

/// The notice for a compaction that failed and reruns on a cleaned context.
#[derive(askama::Template)]
#[template(path = "tui_compaction_recovery.txt", escape = "none")]
struct CompactionRecoveryNotice<'a> {
    error: &'a aither_agent::AgentError,
    alerts_removed: usize,
    outputs_spilled: usize,
}

/// The chat stream containing all elements.
#[derive(Debug, Default)]
pub struct ChatStream {
    elements: Vec<ChatElement>,
    /// Revision of each element, parallel to `elements`. Every mutation of
    /// an element stamps it with a fresh revision, unique across every
    /// stream of the process (a rebuilt stream must never alias the
    /// layout cache of the one it replaced), so the render cache
    /// ([`ChatLayout`]) re-lays-out only what changed.
    revisions: Vec<u64>,
    /// Counter generating thinking-block IDs.
    next_thinking_id: usize,
    /// Counter generating worked-for row IDs.
    next_worked_id: usize,
    /// Counter generating translation-marker IDs.
    next_translation_id: usize,
    /// Elements before this index belong to already-folded turns and are
    /// never folded again.
    fold_cursor: usize,
    /// Elements before this index belong to model responses that finished.
    /// The elements from it on are the response in flight, which a retry
    /// discards.
    response_floor: usize,
    /// Base `http://host/tunnel` URL `<tunnel id>` tags resolve against.
    tunnel_base: String,
    /// The reply streaming text was last appended to: an identity unique
    /// across every stream of the process, under which a layout cache keeps
    /// what it rendered of the reply, and the revision the append left.
    streaming_reply: Option<(u64, u64)>,
}

/// The `<tunnel id="…"/>` control tag.
fn tunnel_tag() -> &'static regex::Regex {
    static TAG: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    TAG.get_or_init(|| {
        regex::Regex::new(r#"<tunnel\b[^>]*?\bid\s*=\s*["']([^"']+)["'][^>]*/>"#)
            .expect("tunnel tag pattern must compile")
    })
}

/// Renders `<tunnel id="…"/>` control tags as daemon URLs so the terminal
/// shows a link instead of the raw marker.
fn resolve_tunnel_tags<'a>(content: &'a str, tunnel_base: &str) -> std::borrow::Cow<'a, str> {
    tunnel_tag().replace_all(content, |captures: &regex::Captures<'_>| {
        format!("{tunnel_base}/{}/", &captures[1])
    })
}

/// Whether appended text can no longer change the tunnel tags of `content`.
///
/// It holds when every `<tunnel` opening lies inside a tag found in
/// `content` or meets a `>` before any quote: a tag holds a `>` only
/// inside its quoted id, so such an opening is no tag whatever follows.
fn tunnel_tags_settled(content: &str) -> bool {
    let tags: Vec<std::ops::Range<usize>> = tunnel_tag()
        .find_iter(content)
        .map(|tag| tag.range())
        .collect();
    content.match_indices("<tunnel").all(|(start, _)| {
        tags.iter().any(|tag| tag.contains(&start))
            || content[start..]
                .find(['>', '"', '\''])
                .is_some_and(|at| content[start + at..].starts_with('>'))
    })
}

impl ChatStream {
    /// Create a new empty chat stream.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tunnel_base: format!("http://localhost:{}/tunnel", crate::daemon::DEFAULT_PORT),
            ..Self::default()
        }
    }

    /// Sets the daemon base URL `<tunnel id>` tags render against.
    #[must_use]
    pub fn with_tunnel_base(mut self, tunnel_base: String) -> Self {
        self.tunnel_base = tunnel_base;
        self
    }

    /// The base URL `<tunnel id>` tags resolve against.
    pub(super) fn tunnel_base(&self) -> &str {
        &self.tunnel_base
    }

    fn next_revision() -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    /// Append `element` with a fresh revision.
    fn push_element(&mut self, element: ChatElement) {
        let revision = Self::next_revision();
        self.elements.push(element);
        self.revisions.push(revision);
    }

    /// Mark the element at `index` changed.
    fn touch(&mut self, index: usize) {
        let revision = Self::next_revision();
        if let Some(slot) = self.revisions.get_mut(index) {
            *slot = revision;
        }
    }

    /// Mark the last element changed.
    fn touch_last(&mut self) {
        if let Some(index) = self.elements.len().checked_sub(1) {
            self.touch(index);
        }
    }

    /// Index of the tool call `id`.
    fn tool_index(&self, id: &str) -> Option<usize> {
        self.elements.iter().position(
            |elem| matches!(elem, ChatElement::ToolCall { id: tool_id, .. } if tool_id == id),
        )
    }

    /// Revision of the element at `index` (see [`Self::revisions`]).
    #[must_use]
    pub fn revision(&self, index: usize) -> u64 {
        self.revisions.get(index).copied().unwrap_or_default()
    }

    /// Add a user message. `history_index` is the position of the
    /// persisted user row in session history.
    pub fn push_user(&mut self, content: &str, attachments: Vec<Attachment>, history_index: usize) {
        self.push_element(ChatElement::UserMessage {
            content: content.to_string(),
            attachments,
            history_index,
        });
        self.seal_response();
    }

    /// Start the stream with the welcome header showing `cwd`.
    ///
    /// # Panics
    /// When the stream already holds elements: the header only ever opens it.
    pub fn push_welcome(&mut self, cwd: String) {
        assert!(
            self.elements.is_empty(),
            "the welcome header must be the first element"
        );
        self.push_element(ChatElement::Welcome { cwd });
    }

    /// Add an error message.
    pub fn push_error(&mut self, message: &str) {
        self.push_element(ChatElement::Error {
            message: message.to_string(),
        });
    }

    /// Add a warning: a warning icon and yellow text.
    pub fn push_warning(&mut self, message: &str) {
        self.end_text();
        self.push_element(ChatElement::Warning {
            message: message.to_string(),
        });
    }

    /// Add a neutral system notice.
    pub fn push_notice(&mut self, message: &str) {
        self.end_text();
        self.push_element(ChatElement::Notice {
            message: message.to_string(),
        });
    }

    /// Add the notice for a failed compaction that is cleaned up and rerun.
    ///
    /// # Panics
    /// When the compile-time checked notice template fails to render.
    pub fn push_compaction_recovery(
        &mut self,
        error: &aither_agent::AgentError,
        alerts_removed: usize,
        outputs_spilled: usize,
    ) {
        let notice = CompactionRecoveryNotice {
            error,
            alerts_removed,
            outputs_spilled,
        }
        .render()
        .expect("compaction recovery template renders");
        self.push_notice(&notice);
    }

    /// Add a catch-up recap card.
    pub fn push_recap(&mut self, summary: &str, next_action: Option<String>) {
        self.push_element(ChatElement::Recap {
            summary: summary.to_string(),
            next_action,
        });
    }

    /// Add a `/context` usage report.
    pub fn push_context_report(&mut self, report: ContextReport) {
        self.end_text();
        self.push_element(ChatElement::ContextReport(report));
    }

    /// Add a `/usage` provider-usage report.
    pub fn push_usage_report(&mut self, report: crate::agent::SessionUsageReport) {
        self.end_text();
        self.push_element(ChatElement::UsageReport(report));
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
                        target.files += diffs.iter().map(|d| d.changes.len()).sum::<usize>();
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
    /// editing). `None` when no user message carries `history_index`.
    #[must_use]
    pub fn prompt_rewind_payload(&self, history_index: usize) -> Option<(String, Vec<Attachment>)> {
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
        Some((content.clone(), attachments.clone()))
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
        self.revisions.truncate(pos);
        true
    }

    /// Drop the latest user message whose text is `content`, with everything
    /// after it (the turn's output and markers) — the stream's half of a
    /// withdrawn turn. Returns that message's attachments, `None` when no
    /// such message is shown.
    pub fn withdraw_last_prompt(&mut self, content: &str) -> Option<Vec<Attachment>> {
        let pos = self.elements.iter().rposition(
            |elem| matches!(elem, ChatElement::UserMessage { content: shown, .. } if shown == content),
        )?;
        let removed = self.elements.drain(pos..).next();
        self.revisions.truncate(pos);
        match removed {
            Some(ChatElement::UserMessage { attachments, .. }) => Some(attachments),
            _ => unreachable!("rposition matched a user message"),
        }
    }

    fn pop_last(&mut self) {
        self.elements.pop();
        self.revisions.pop();
    }

    /// Start thinking block.
    pub fn start_thinking(&mut self) {
        let id = self.next_thinking_id;
        self.next_thinking_id += 1;
        self.push_element(ChatElement::Thinking {
            id: format!("thinking-{id}"),
            content: String::new(),
            seconds: 0.0,
            expanded: true,
            live: true,
        });
    }

    /// Append streamed reasoning text to the open thinking block, starting a
    /// new block when the previous burst already collapsed. An empty 0-second
    /// stub left by a premature phase close is reused instead of sitting in
    /// front of the real burst.
    pub fn push_thinking(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        match self.elements.last() {
            Some(ChatElement::Thinking { live: true, .. }) => {}
            Some(ChatElement::Thinking {
                content, seconds, ..
            }) if content.is_empty() && thought_millis(*seconds) == 0 => {
                if let Some(ChatElement::Thinking { expanded, live, .. }) = self.elements.last_mut()
                {
                    *expanded = true;
                    *live = true;
                    self.touch_last();
                }
            }
            _ => self.start_thinking(),
        }
        if let Some(ChatElement::Thinking { content, .. }) = self.elements.last_mut() {
            content.push_str(chunk);
            self.touch_last();
        }
    }

    /// Update thinking time.
    pub fn update_thinking(&mut self, seconds: f64) {
        if let Some(ChatElement::Thinking { seconds: s, .. }) = self.elements.last_mut()
            && thought_duration_label(*s) != thought_duration_label(seconds)
        {
            *s = seconds;
            self.touch_last();
        }
    }

    /// Stamp the daemon-measured duration of a reasoning phase and collapse
    /// the block — the phase ended, nothing more can stream into it. A phase
    /// that produced no readable text leaves a collapsed, non-expandable
    /// header when the duration is measurable. A zero-duration empty close
    /// is dropped so it cannot sit in front of the next burst as a bare
    /// `Thought`.
    pub fn finish_thinking(&mut self, seconds: f64) {
        enum Finish {
            Stamp,
            Pop,
            Push,
            None,
        }
        let zero = thought_millis(seconds) == 0;
        let action = match self.elements.last() {
            Some(ChatElement::Thinking {
                content,
                expanded,
                live,
                ..
            }) if *expanded || *live => {
                if content.is_empty() && zero {
                    Finish::Pop
                } else {
                    Finish::Stamp
                }
            }
            Some(ChatElement::Thinking { content, .. }) if content.is_empty() => {
                if zero {
                    Finish::Pop
                } else {
                    Finish::Stamp
                }
            }
            Some(ChatElement::Thinking { .. }) => Finish::Stamp,
            _ if zero => Finish::None,
            _ => Finish::Push,
        };
        match action {
            Finish::Pop => self.pop_last(),
            Finish::None => {}
            Finish::Stamp => {
                if let Some(ChatElement::Thinking {
                    seconds: s,
                    expanded,
                    live,
                    ..
                }) = self.elements.last_mut()
                {
                    *s = seconds;
                    *expanded = false;
                    *live = false;
                    self.touch_last();
                }
            }
            Finish::Push => {
                let id = self.next_thinking_id;
                self.next_thinking_id += 1;
                self.push_element(ChatElement::Thinking {
                    id: format!("thinking-{id}"),
                    content: String::new(),
                    seconds,
                    expanded: false,
                    live: false,
                });
            }
        }
    }

    /// Collapse the open thinking block (called when the reasoning burst
    /// ends or first text arrives). The block stays as a collapsed header,
    /// unless it is an empty 0-second stub, which is dropped.
    pub fn collapse_thinking(&mut self) {
        match self.elements.last() {
            Some(ChatElement::Thinking {
                content,
                seconds,
                expanded,
                live,
                ..
            }) if (*expanded || *live) && content.is_empty() && thought_millis(*seconds) == 0 => {
                self.pop_last();
            }
            Some(ChatElement::Thinking { expanded, live, .. }) if *expanded || *live => {
                if let Some(ChatElement::Thinking { expanded, live, .. }) = self.elements.last_mut()
                {
                    *expanded = false;
                    *live = false;
                    self.touch_last();
                }
            }
            _ => {}
        }
    }

    /// Mark every call still marked running as interrupted — the run ended
    /// without reporting their completion, so holding their bodies open
    /// would be wrong. They collapse once the recent window lapses.
    pub fn finish_running_calls(&mut self) {
        for index in 0..self.elements.len() {
            let Some(ChatElement::ToolCall { display, .. }) = self.elements.get_mut(index) else {
                continue;
            };
            if !matches!(display.status(), ToolStatus::Running) {
                continue;
            }
            match display {
                ToolDisplay::Terminal { status, .. }
                | ToolDisplay::Action { status, .. }
                | ToolDisplay::Builtin { status, .. } => {
                    *status = ToolStatus::Failed("interrupted".to_string());
                }
            }
            self.touch(index);
        }
    }

    /// Fold the finished turn's work — thinking bursts, tool calls and the
    /// intermediate messages between them — into a single "Worked for" row.
    /// The turn's final reply stays visible, as do notices and reports.
    pub fn fold_turn_work(&mut self, seconds: u64) {
        // A turn that ends on a thought never saw text or a call close it:
        // a folded thought is finished, so its body can expand.
        self.collapse_thinking();
        self.fold_cursor = self.fold_cursor.min(self.elements.len());
        let start = self
            .elements
            .iter()
            .rposition(|e| matches!(e, ChatElement::UserMessage { .. }))
            .map_or(self.fold_cursor, |i| (i + 1).max(self.fold_cursor));
        // The reply ends the turn: everything before its last block is work.
        // A reply that is the turn's first element leaves nothing to fold.
        let end = self
            .elements
            .iter()
            .rposition(|e| matches!(e, ChatElement::AssistantText { .. }))
            .filter(|index| *index >= start)
            .unwrap_or(self.elements.len());
        if end <= start {
            return;
        }
        // A failure is the turn's outcome, not its work: it stays visible
        // after the row.
        let (errors, folded): (Vec<_>, Vec<_>) = self
            .elements
            .splice(start..end, [])
            .zip(self.revisions.splice(start..end, []))
            .partition(|(element, _)| matches!(element, ChatElement::Error { .. }));
        let mut at = start;
        // The previous turn left no result of its own (it was interrupted,
        // resumed by a background command or ended on a question): its row
        // takes this turn's work, so no two rows stand next to each other.
        let mut folded = folded;
        if let Some(index) = start.checked_sub(1)
            && !folded.is_empty()
            && let ChatElement::WorkedFor {
                seconds: total,
                elements,
                ..
            } = &mut self.elements[index]
        {
            *total += seconds;
            elements.extend(
                std::mem::take(&mut folded)
                    .into_iter()
                    .map(|(element, _)| element),
            );
            self.touch(index);
        }
        // A single item is its own row: a group around it hides nothing.
        if let [_] = folded.as_slice() {
            for (element, revision) in folded {
                self.elements.insert(at, element);
                self.revisions.insert(at, revision);
                at += 1;
            }
        } else if !folded.is_empty() {
            let id = self.next_worked_id;
            self.next_worked_id += 1;
            self.elements.insert(
                at,
                ChatElement::WorkedFor {
                    id: format!("work-{id}"),
                    seconds,
                    elements: folded.into_iter().map(|(element, _)| element).collect(),
                    expanded: false,
                },
            );
            self.revisions.insert(at, Self::next_revision());
            at += 1;
        }
        for (element, revision) in errors {
            self.elements.insert(at, element);
            self.revisions.insert(at, revision);
            at += 1;
        }
        self.fold_cursor = at;
    }

    /// Start a new assistant text block.
    pub fn start_assistant(&mut self) {
        self.push_reply(String::new());
    }

    /// Append a streaming reply holding `content` so far.
    fn push_reply(&mut self, content: String) {
        self.push_element(ChatElement::AssistantText {
            content,
            streaming: true,
            translation: None,
        });
        self.streaming_reply = Some((Self::next_revision(), self.revision_of_last()));
    }

    /// Identity of the reply in the last element, when the last change to
    /// that element appended text to it.
    ///
    /// Between two calls that return the same identity, the reply only
    /// had text appended.
    pub(super) fn streaming_reply(&self) -> Option<u64> {
        self.streaming_reply
            .and_then(|(reply, revision)| (revision == self.revision_of_last()).then_some(reply))
    }

    /// Revision of the last element; `0` when there is none.
    fn revision_of_last(&self) -> u64 {
        self.revisions.last().copied().unwrap_or_default()
    }

    /// Attach the display translation of the reply streamed last: the
    /// translation is shown, marked as translated, with the original one
    /// click away. `language` is the communication language's ISO 639-3 code.
    pub fn set_translation(&mut self, text: String, language: &str) {
        let Some(index) = self
            .elements
            .iter()
            .rposition(|e| matches!(e, ChatElement::AssistantText { .. }))
        else {
            return;
        };
        let id = format!("translation-{}", self.next_translation_id);
        self.next_translation_id += 1;
        if let ChatElement::AssistantText { translation, .. } = &mut self.elements[index] {
            *translation = Some(ReplyTranslation {
                id,
                text,
                language: crate::session::language_display_name(language),
                show_original: false,
            });
        }
        self.touch(index);
    }

    /// Append text to the current streaming element.
    pub fn push_text(&mut self, chunk: &str) {
        if let Some(ChatElement::AssistantText {
            content,
            streaming: true,
            ..
        }) = self.elements.last_mut()
        {
            content.push_str(chunk);
            // When anything else changed the last element since text was
            // last appended to it, the reply streams under a new identity.
            let reply = self.streaming_reply().unwrap_or_else(Self::next_revision);
            self.touch_last();
            self.streaming_reply = Some((reply, self.revision_of_last()));
        } else {
            self.push_reply(chunk.to_string());
        }
    }

    /// Everything shown so far belongs to model responses that finished: a
    /// retry discards only what streams after this.
    pub const fn seal_response(&mut self) {
        self.response_floor = self.elements.len();
    }

    /// Drop what the attempt being retried streamed: its text, its thinking
    /// and the tool calls it began to stream or that the provider ran for
    /// it. Tool calls that ran, and everything before the response in
    /// flight, stay.
    pub fn retract_response(&mut self) {
        let floor = self.response_floor.min(self.elements.len());
        let belongs_to_attempt = |element: &ChatElement| match element {
            ChatElement::Thinking { .. } | ChatElement::AssistantText { .. } => true,
            ChatElement::ToolCall { display, .. } => {
                matches!(display, ToolDisplay::Builtin { .. }) || !display.is_complete()
            }
            _ => false,
        };
        for index in (floor..self.elements.len()).rev() {
            if belongs_to_attempt(&self.elements[index]) {
                self.elements.remove(index);
                self.revisions.remove(index);
            }
        }
    }

    /// Mark the current text element as done streaming.
    pub fn end_text(&mut self) {
        if let Some(ChatElement::AssistantText { streaming, .. }) = self.elements.last_mut()
            && *streaming
        {
            *streaming = false;
            self.touch_last();
        }
    }

    /// Add a tool call.
    pub fn push_tool(&mut self, id: String, display: ToolDisplay) {
        // End any current text block first
        self.end_text();
        self.push_element(ChatElement::ToolCall { id, display });
    }

    /// Insert a tool call, or replace its display when the element already
    /// exists: a call materialized from streamed arguments gets its
    /// canonical display once `ToolCallStart` lands, and a delta
    /// projection refreshes it on every fragment. A finished call keeps
    /// its display — a trailing event must not resurrect it — and the
    /// user's expansion pin and any attached diffs carry over.
    pub fn upsert_tool_display(&mut self, id: String, display: ToolDisplay) {
        if let Some(index) = self.tool_index(&id) {
            if let ChatElement::ToolCall { display: slot, .. } = &mut self.elements[index]
                && !slot.is_complete()
            {
                let mut display = display;
                let pinned = slot.pinned();
                match &mut display {
                    ToolDisplay::Terminal {
                        pinned: pin, diffs, ..
                    } => {
                        *pin = pinned;
                        if let ToolDisplay::Terminal { diffs: old, .. } = slot {
                            *diffs = std::mem::take(old);
                        }
                    }
                    ToolDisplay::Action { pinned: pin, .. }
                    | ToolDisplay::Builtin { pinned: pin, .. } => *pin = pinned,
                }
                *slot = display;
                self.touch(index);
            }
            return;
        }
        self.push_tool(id, display);
    }

    /// Attach the daemon's reading of a terminal call's script to the call.
    pub fn set_tool_reading(&mut self, id: &str, reading: CommandReading) {
        let Some(index) = self.tool_index(id) else {
            return;
        };
        if let ChatElement::ToolCall {
            display: ToolDisplay::Terminal { reading: slot, .. },
            ..
        } = &mut self.elements[index]
        {
            *slot = Some(reading);
            self.touch(index);
        }
    }

    /// Attach the card of a terminal call that ran one of May's commands, or
    /// take it away so the call shows as the ordinary command row.
    pub fn set_tool_card(&mut self, id: &str, card: Option<Card>) {
        let Some(index) = self.tool_index(id) else {
            return;
        };
        if let ChatElement::ToolCall {
            display: ToolDisplay::Terminal { card: slot, .. },
            ..
        } = &mut self.elements[index]
        {
            *slot = card.map(Box::new);
            self.touch(index);
        }
    }

    /// Record a provider-hosted tool call the model just invoked.
    pub fn push_builtin(&mut self, id: String, tool: String, detail: String) {
        self.end_text();
        self.push_element(ChatElement::ToolCall {
            id,
            display: ToolDisplay::Builtin {
                tool,
                detail,
                status: ToolStatus::Running,
                output: None,
                pinned: None,
            },
        });
    }

    /// Mark a provider-hosted call complete. Providers that emit a result
    /// without a matching call event get a completed entry appended instead.
    pub fn finish_builtin(&mut self, id: &str, tool: &str, result: String) {
        let found = self.elements.iter().position(|elem| {
            matches!(elem, ChatElement::ToolCall { id: tool_id, display: ToolDisplay::Builtin { .. } } if tool_id == id)
        });
        if let Some(index) = found {
            if let ChatElement::ToolCall {
                display:
                    ToolDisplay::Builtin {
                        tool,
                        detail,
                        status,
                        output,
                        ..
                    },
                ..
            } = &mut self.elements[index]
            {
                *status = ToolStatus::Done;
                if detail.is_empty()
                    && is_one_row_tool(tool)
                    && let Some(query) = quoted_result(&result)
                {
                    *detail = query.to_string();
                }
                *output = Some(result);
            }
            self.touch(index);
            return;
        }
        self.end_text();
        self.push_element(ChatElement::ToolCall {
            id: id.to_string(),
            display: ToolDisplay::Builtin {
                tool: tool.to_string(),
                detail: if is_one_row_tool(tool) {
                    quoted_result(&result).unwrap_or_default().to_string()
                } else {
                    String::new()
                },
                status: ToolStatus::Done,
                output: Some(result),
                pinned: None,
            },
        });
    }

    /// Update a tool's status and output by ID. A non-`Running` status stamps
    /// the call's finish time, which the automatic collapse counts from.
    pub fn update_tool_with_output(&mut self, id: &str, status: ToolStatus, output: Option<&str>) {
        let Some(index) = self.tool_index(id) else {
            return;
        };
        self.touch(index);
        if let ChatElement::ToolCall { display, .. } = &mut self.elements[index] {
            match display {
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
                ToolDisplay::Action {
                    status: s, chip, ..
                } if chip == COMPACT_TOOL => {
                    // A compaction is a status row with nothing to expand, so
                    // its result text is not kept; a refusal arrives as a
                    // failed status from the tool's typed error.
                    *s = status;
                }
                ToolDisplay::Action {
                    status: s,
                    chip,
                    task_id,
                    output: out,
                    ..
                } => {
                    *s = status;
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
                    ..
                } => {
                    *s = status;
                    if let Some(output_text) = output {
                        *out = Some(output_text.to_string());
                    }
                }
            }
        }
    }

    /// Flip the expansion of a collapsible element (tool call or thinking
    /// block). For tool calls this sets the click pin: `Some(true)` pins the
    /// body open, `Some(false)` pins it shut, overriding the automatic rule.
    /// An element folded inside a "Worked for" group toggles on its own —
    /// the group stays expanded.
    pub fn toggle_expanded(&mut self, id: &str) {
        for (index, elem) in self.elements.iter_mut().enumerate() {
            if elem.toggle_expanded(id) {
                self.touch(index);
                return;
            }
        }
    }

    /// Attach file diffs to a terminal tool call.
    pub fn update_tool_diff(&mut self, id: &str, diffs: Vec<crate::file_tracker::RootDiff>) {
        let mut touched = None;
        for (index, elem) in self.elements.iter_mut().enumerate() {
            if let ChatElement::ToolCall {
                id: tool_id,
                display: ToolDisplay::Terminal { diffs: slot, .. },
            } = elem
                && tool_id == id
            {
                *slot = diffs;
                touched = Some(index);
                break;
            }
        }
        if let Some(index) = touched {
            self.touch(index);
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
        let mut touched = None;
        for (index, elem) in self.elements.iter_mut().enumerate() {
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
                touched = Some(index);
                break;
            }
        }
        if let Some(index) = touched {
            self.touch(index);
        }
    }

    /// Append live output to a running terminal call's buffer. Creates the
    /// `TerminalCallInfo` on first chunk; the running renderer already shows
    /// `output` mid-run.
    pub fn append_tool_output(&mut self, id: &str, chunk: &str) {
        let mut touched = None;
        for (index, elem) in self.elements.iter_mut().enumerate() {
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
                touched = Some(index);
                break;
            }
        }
        if let Some(index) = touched {
            self.touch(index);
        }
    }

    /// Set the todo block (replaces previous).
    pub fn set_todos(&mut self, rows: Vec<TaskRow>) {
        // Remove existing todo block
        let mut kept_revisions = Vec::with_capacity(self.revisions.len());
        for (elem, revision) in self.elements.iter().zip(&self.revisions) {
            if !matches!(elem, ChatElement::TodoBlock { .. }) {
                kept_revisions.push(*revision);
            }
        }
        self.elements
            .retain(|e| !matches!(e, ChatElement::TodoBlock { .. }));
        self.revisions = kept_revisions;
        // Add new one at end
        if !rows.is_empty() {
            self.push_element(ChatElement::TodoBlock { rows });
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

/// Quoted `url` attribute of an `<artifact …>` control tag.
fn artifact_tag_url(tag: &str) -> Option<&str> {
    let tail = tag
        .split("url")
        .nth(1)?
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let quote = *tail.as_bytes().first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let inner = &tail[1..];
    let end = inner.find(quote as char)?;
    let path = &inner[..end];
    (!path.is_empty()).then_some(path)
}

/// Replaces `<artifact url="path" … />` control tags with a readable
/// markdown line; a malformed or still-streaming tag is left alone.
fn artifact_tag_text(content: &str) -> std::borrow::Cow<'_, str> {
    replace_artifact_tags(content).0
}

/// [`artifact_tag_text`], and whether every `<artifact` opening was closed.
///
/// An opening with no `/>` after it can still become a tag when text is
/// appended.
fn replace_artifact_tags(content: &str) -> (std::borrow::Cow<'_, str>, bool) {
    if !content.contains("<artifact") {
        return (std::borrow::Cow::Borrowed(content), true);
    }
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    let mut closed = true;
    while let Some(start) = rest.find("<artifact") {
        let after = &rest[start..];
        let Some(end) = after.find("/>") else {
            closed = false;
            break;
        };
        let Some(path) = artifact_tag_url(&after[..end]) else {
            out.push_str(&rest[..start + "<artifact".len()]);
            rest = &after["<artifact".len()..];
            continue;
        };
        out.push_str(&rest[..start]);
        // Image artifacts render through the markdown-image pipeline:
        // `image_fetch_targets` resolves the sandbox-relative path.
        if is_image_artifact(path) {
            out.push_str("![artifact](");
            out.push_str(path);
            out.push(')');
        } else {
            out.push_str("**artifact:** `");
            out.push_str(path);
            out.push('`');
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    (std::borrow::Cow::Owned(out), closed)
}

/// The artifact path names a raster image the TUI can decode.
fn is_image_artifact(path: &str) -> bool {
    let lower = path.rsplit('.').next().unwrap_or("");
    matches!(lower, "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp") && path.len() > lower.len() + 1
}

/// Input rendering context for combined view.
#[derive(Debug)]
pub struct InputContext<'a> {
    /// Current input text.
    pub input: &'a str,
    /// Cursor position: a byte offset of `input` on a grapheme boundary.
    pub cursor: usize,
    /// Pending attachments.
    pub attachments: &'a [Attachment],
    /// Ghost text suggested for the empty input; `None` hides it.
    pub suggestion: Option<&'a str>,
    /// Greeting line drawn directly above the input line.
    pub greeting: Option<&'a str>,
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
    /// Token usage info (`prompt_tokens`, model `context_window`). The status
    /// bar denominator is the auto-compact window derived from the latter.
    pub usage: Option<(u64, Option<u64>)>,
    /// A compaction's state, shown in place of the usage.
    pub compaction: CompactionMeter,
    /// Compaction thresholds the usage denominator follows.
    pub context_policy: aither_agent::ContextPolicy,
    /// Queued message count.
    pub queue_count: usize,
    /// Questions the agent asked that wait for an answer.
    pub open_questions: usize,
    /// The session grants every approval request without asking.
    pub yolo: bool,
    /// Running background terminal jobs (the sidebar lists each command).
    pub shells: usize,
    /// Running subagents (the sidebar lists each one).
    pub subagents: usize,
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
    /// Coverage of the session's worktree index: a slim blue bar while the
    /// index is incomplete, `index ✓` once full, a red `index ✗` when
    /// repo-search is down.
    pub index_coverage: Option<&'a crate::client::IndexCoverage>,
    /// Repo-search could not be reached; the status bar shows a red cross.
    pub index_failed: bool,
    /// The first index snapshot has not arrived; the status bar shows a
    /// spinner.
    pub index_loading: bool,
}

/// Queued message context for rendering queue indicator.
#[derive(Debug)]
pub struct QueueContext<'a> {
    /// Queued message previews.
    pub messages: &'a [String],
}

/// Render the chat stream with a throwaway layout cache.
#[cfg(test)]
pub fn render(frame: &mut Frame, area: Rect, chat: &ChatStream, scroll: usize) {
    let mut layout = ChatLayout::default();
    render_impl(
        frame,
        area,
        chat,
        &mut layout,
        scroll,
        RenderContext::default(),
    );
}

/// One inline image block placed relative to the chat area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePlacement {
    /// Image cache key.
    pub key: String,
    /// Path or URL shown under the zoomed image.
    pub caption: String,
    /// First reserved row relative to the chat area's top edge; negative
    /// when the block starts above the view.
    pub top: i32,
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
    /// Todo blocks render nothing (the sidebar shows the task tree).
    pub hide_todos: bool,
    /// Output: image blocks intersecting the view, for the overlay.
    pub image_positions_out: Option<&'a mut Vec<ImagePlacement>>,
    /// Animation frame counter for running tool calls' status spinners.
    pub spinner_frame: usize,
    /// Background of the input band, from the terminal's own colours;
    /// `None` keeps the terminal's default background.
    pub band_bg: Option<Color>,
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
            hide_todos: ctx.hide_todos,
            image_positions_out: ctx.image_positions_out,
            spinner_frame: ctx.spinner_frame,
            band_bg: ctx.band_bg,
        }
    }
}

/// Render the chat stream with full-screen scrolling (chat + queue + input + status).
pub fn render_unified(
    frame: &mut Frame,
    area: Rect,
    chat: &ChatStream,
    layout: &mut ChatLayout,
    scroll: usize,
    ctx: UnifiedRenderContext<'_>,
) -> crate::tui::perf::ChatRenderStats {
    render_impl(frame, area, chat, layout, scroll, ctx.into())
}

/// Plain wrapped rows of the unified view (chat + queue + input + status),
/// exactly as [`render_unified`] draws them. When `regions_out` is given,
/// it also reports the row span of every collapsible element.
#[cfg(test)]
#[must_use]
pub fn build_plain_wrapped_lines_unified(
    chat: &ChatStream,
    width: u16,
    queue_ctx: Option<&QueueContext<'_>>,
    input_ctx: Option<&InputContext<'_>>,
    status_ctx: Option<&StatusContext<'_>>,
    regions_out: Option<&mut Vec<ClickRegion>>,
    images_supported: bool,
) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    // Columns are relative to the content area: the horizontal padding is
    // left out, the rows are the ones the screen shows.
    let inner = width - 2 * band::padding(width);
    let mut layout = ChatLayout::default();
    layout.sync(chat, inner, images_supported, false, None, 0);
    let mut out: Vec<String> = (0..layout.element_rows())
        .map(|row| layout.plain_line(row).unwrap_or_default().to_string())
        .collect();

    let tail = build_tail(
        queue_ctx,
        input_ctx,
        status_ctx,
        inner,
        images_supported,
        false,
    );
    let rows = tail
        .lines
        .iter()
        .map(|line| chat_layout::line_rows(line, inner))
        .sum();
    out.extend(chat_layout::plain_rows(&tail.lines, inner, rows));

    if let Some(slot) = regions_out {
        *slot = layout.collapse_regions();
    }
    out
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

/// Milliseconds as `1d 2h 3m` or `4m 5s`, biggest unit first.
fn usage_duration(ms: u64) -> String {
    const MIN: u64 = 60_000;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    let (days, rem) = (ms / DAY, ms % DAY);
    let (hours, rem) = (rem / HOUR, rem % HOUR);
    let (mins, secs) = (rem / MIN, rem % MIN);
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if mins > 0 || parts.is_empty() {
        parts.push(format!("{mins}m"));
    }
    if parts.len() < 2 && secs > 0 {
        parts.push(format!("{}s", secs / 1000));
    }
    parts.join(" ")
}

/// The `/usage` wall clock and where it went.
#[derive(askama::Template)]
#[template(path = "usage_wall_clock.txt", escape = "none")]
struct UsageWallClock {
    wall: String,
    api: String,
    tools: String,
    waiting: String,
    idle: String,
}

impl UsageWallClock {
    fn new(report: &crate::agent::SessionUsageReport) -> Self {
        Self {
            wall: usage_duration(report.wall_ms),
            api: usage_duration(report.split.api_ms),
            tools: usage_duration(report.split.tools_ms),
            waiting: usage_duration(report.split.user_wait_ms),
            idle: usage_duration(report.split.idle_ms),
        }
    }
}

/// A model's request count on its `/usage` row, with the requests that only
/// kept the prompt cache warm.
#[derive(askama::Template)]
#[template(path = "usage_model_requests.txt", escape = "none")]
struct UsageModelRequests {
    requests: u64,
    keep_warm: u64,
}

/// A model's cache misses under its `/usage` row.
#[derive(askama::Template)]
#[template(path = "usage_model_cache_misses.txt", escape = "none")]
struct UsageModelCacheMisses {
    misses: u64,
    tokens: String,
}

/// Where a listed cache miss first changed the prefix.
enum Diverged<'a> {
    Tools,
    Message { index: u64, kind: &'a str },
    Nothing,
}

/// One cause's line of the `/usage` cache-miss breakdown.
struct UsageMissCause {
    cause: crate::agent::cache_miss::CacheMissCause,
    misses: u64,
    tokens: String,
}

/// One listed cache miss.
struct UsageMissLine<'a> {
    at: String,
    model: &'a str,
    cause: crate::agent::cache_miss::CacheMissCause,
    tokens: String,
    gap: String,
    diverged: Diverged<'a>,
}

/// The `/usage` cache misses: count, causes and the newest misses.
#[derive(askama::Template)]
#[template(path = "usage_cache_misses.txt", escape = "none")]
struct UsageCacheMisses<'a> {
    total: u64,
    tokens: String,
    causes: Vec<UsageMissCause>,
    recent: Vec<UsageMissLine<'a>>,
}

impl<'a> UsageCacheMisses<'a> {
    /// The section for `report`, `None` when the session had no miss.
    fn new(report: &'a crate::agent::SessionUsageReport) -> Option<Self> {
        if report.cache_miss_causes.is_empty() {
            return None;
        }
        let zone = jiff::tz::TimeZone::system();
        Some(Self {
            total: report.cache_miss_causes.iter().map(|row| row.misses).sum(),
            tokens: compact_count(
                report
                    .cache_miss_causes
                    .iter()
                    .map(|row| row.missed_tokens)
                    .sum(),
            ),
            causes: report
                .cache_miss_causes
                .iter()
                .map(|row| UsageMissCause {
                    cause: row.cause,
                    misses: row.misses,
                    tokens: compact_count(row.missed_tokens),
                })
                .collect(),
            recent: report
                .recent_cache_misses
                .iter()
                .map(|miss| UsageMissLine {
                    at: miss.timestamp.parse::<jiff::Timestamp>().map_or_else(
                        |_| miss.timestamp.clone(),
                        |at| at.to_zoned(zone.clone()).strftime("%H:%M").to_string(),
                    ),
                    model: &miss.model,
                    cause: miss.cause,
                    tokens: compact_count(miss.missed_tokens),
                    gap: usage_duration(miss.gap_ms),
                    diverged: match (miss.diverged_kind.as_deref(), miss.diverged_index) {
                        (Some("tools"), _) => Diverged::Tools,
                        (Some(kind), Some(index)) => Diverged::Message { index, kind },
                        _ => Diverged::Nothing,
                    },
                })
                .collect(),
        })
    }
}

/// Styled lines for a `/usage` report block: session totals, then one row
/// per model with its cache and cost breakdown, then the cache misses.
fn usage_report_lines(report: &crate::agent::SessionUsageReport) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let value = Style::default().fg(Color::Cyan);
    let mut lines: Vec<Line<'static>> = UsageWallClock::new(report)
        .render()
        .expect("the wall-clock template renders")
        .lines()
        .map(|line| Line::styled(line.to_string(), value))
        .collect();
    if let Some(cost) = report.total_cost_usd {
        lines.push(Line::from(vec![
            Span::styled("Total cost ", dim),
            Span::styled(format_cost_usd(cost), value),
        ]));
    }
    for model in &report.models {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(model.model.clone(), value),
            Span::styled(
                UsageModelRequests {
                    requests: model.requests,
                    keep_warm: model.keep_warm_requests,
                }
                .render()
                .expect("the model requests template renders"),
                dim,
            ),
        ]));
        let cost_suffix = model
            .cost_usd
            .map_or_else(String::new, |cost| format!("  {}", format_cost_usd(cost)));
        lines.push(Line::styled(
            format!(
                "    out {}  cache read {}  write {}  hit {:.0}%{cost_suffix}",
                compact_count(model.completion_tokens),
                compact_count(model.cache_read_tokens),
                compact_count(model.cache_write_tokens),
                model.cache_hit_rate * 100.0,
            ),
            dim,
        ));
        if model.cache_misses > 0 {
            let misses = UsageModelCacheMisses {
                misses: model.cache_misses,
                tokens: compact_count(model.cache_miss_tokens),
            };
            lines.push(Line::styled(
                misses
                    .render()
                    .expect("the model cache-miss template renders"),
                dim,
            ));
        }
    }
    if let Some(misses) = UsageCacheMisses::new(report) {
        lines.push(Line::from(""));
        lines.extend(
            misses
                .render()
                .expect("the cache-miss template renders")
                .lines()
                .map(|line| Line::styled(line.to_string(), value)),
        );
    }
    lines
}

/// A USD amount: cents from $0.01 up, two significant digits below that so
/// a sub-cent spend never reads as `$0.00`.
fn format_cost_usd(cost: f64) -> String {
    assert!(cost >= 0.0, "a cost is never negative: {cost}");
    if cost == 0.0 || cost >= 0.01 {
        return format!("${cost:.2}");
    }
    // The first significant digit sits at 10^-exp; keep one digit past it.
    // 324 bounds the search at the smallest positive f64.
    let exp = (3_i32..=324)
        .find(|&exp| cost >= 10f64.powi(-exp))
        .expect("a positive f64 is at least 10^-324");
    let decimals = usize::try_from(exp + 1).expect("exp is positive");
    format!("${cost:.decimals$}")
}

/// `1234567` as `1.2m`, `12345` as `12.3k`.
fn compact_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
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
            format!("   reserved above {}", format_number(report.compact_tokens)),
            dim,
        ),
    ]));
    lines.push(Line::from(vec![Span::styled(
        format!(
            "  compaction starts at {} · pauses at {}",
            format_number(report.compact_start_tokens),
            format_number(report.compact_tokens),
        ),
        dim,
    )]));
    if report.phase == ContextWindowPhase::Compacting {
        lines.push(Line::from(Span::styled(
            "  compaction in flight",
            Style::default().fg(Color::Yellow),
        )));
    } else if report.phase != ContextWindowPhase::Stable {
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
    hide_todos: bool,
    image_positions_out: Option<&'a mut Vec<ImagePlacement>>,
    /// Animation frame counter shared with the status-bar spinner.
    spinner_frame: usize,
    /// Background of the input band; `None` keeps the terminal's own.
    band_bg: Option<Color>,
}

/// How one element renders this frame, beyond its own content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ElementState {
    /// The element is the selected tool call.
    pub selected: bool,
    /// The element's body is open (tool calls: [`ToolDisplay::body_open`]).
    pub open: bool,
    /// Todo blocks render nothing (the sidebar shows the task tree).
    pub hide_todos: bool,
    /// Spinner frame for a running call's status icon; `0` for everything
    /// else so only running calls re-lay-out as the frame advances.
    pub spinner_frame: usize,
}

impl ElementState {
    /// State of `elem`.
    pub(super) fn of(
        elem: &ChatElement,
        selected_tool: Option<&str>,
        hide_todos: bool,
        spinner_frame: usize,
    ) -> Self {
        match elem {
            ChatElement::ToolCall { id, display } => Self {
                selected: selected_tool == Some(id.as_str()),
                open: display.expandable() && display.body_open(),
                hide_todos,
                spinner_frame: if matches!(display.status(), ToolStatus::Running) {
                    spinner_frame
                } else {
                    0
                },
            },
            _ => Self {
                selected: false,
                open: false,
                hide_todos,
                spinner_frame: 0,
            },
        }
    }
}

/// Styled rows of one chat element plus the image anchors among them:
/// `(cache key, line index, caption)`.
#[derive(Debug, Default)]
pub(super) struct ElementLines {
    pub lines: Vec<Line<'static>>,
    pub images: Vec<(String, usize, String)>,
    /// `(id, first line index, one past the last)` of each collapsible
    /// (sub-)element these lines contain — a click on those rows toggles
    /// that element, not the one owning the lines around it.
    pub regions: Vec<(String, usize, usize)>,
}

/// `52s`, `1m27s` — the worked-for row's compact duration.
fn compact_seconds(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}

fn thought_millis(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    let ms = seconds * 1000.0;
    if ms >= u64::MAX as f64 {
        return u64::MAX;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        ms.round() as u64
    }
}

/// Thinking-header duration: integer milliseconds below 0.1s, one decimal
/// below 1s, then `12s` / `1m 4s` / `1h 2m`.
#[must_use]
pub fn thought_duration_label(seconds: f64) -> String {
    let ms = thought_millis(seconds);
    if ms < 100 {
        return format!("{ms}ms");
    }
    if ms < 1000 {
        let tenths = (ms + 50) / 100;
        if tenths < 10 {
            return format!("{}.{}s", tenths / 10, tenths % 10);
        }
        return thought_whole_seconds(1);
    }
    thought_whole_seconds(ms.saturating_add(500) / 1000)
}

fn thought_whole_seconds(total: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * 60;
    if total >= HOUR {
        let hours = total / HOUR;
        let minutes = (total % HOUR) / MIN;
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {minutes}m")
        }
    } else if total >= MIN {
        let minutes = total / MIN;
        let secs = total % MIN;
        if secs == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m {secs}s")
        }
    } else {
        format!("{total}s")
    }
}

/// Id a click on `elem` toggles, when it is collapsible.
pub(super) fn collapsible_id(elem: &ChatElement) -> Option<&str> {
    match elem {
        ChatElement::ToolCall { id, display } if display.expandable() => Some(id.as_str()),
        ChatElement::Thinking { id, content, .. } if !content.is_empty() => Some(id.as_str()),
        ChatElement::AssistantText {
            translation: Some(translation),
            ..
        } => Some(translation.id.as_str()),
        ChatElement::WorkedFor { id, .. } => Some(id.as_str()),
        _ => None,
    }
}

/// Body rows shown while a thinking burst is still streaming.
const THINKING_LIVE_LINES: usize = 5;

/// Last `n` logical lines of `text`, matching [`str::lines`] (a trailing
/// newline does not count as an extra empty line).
fn last_lines(text: &str, n: usize) -> &str {
    if n == 0 {
        return "";
    }
    let text = text.strip_suffix('\n').unwrap_or(text);
    let bytes = text.as_bytes();
    let mut remaining = n;
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        if bytes[i] == b'\n' {
            remaining -= 1;
            if remaining == 0 {
                return &text[i + 1..];
            }
        }
    }
    text
}

/// Suffix of `text` whose display width is at most `max_width`.
pub(super) fn suffix_max_width(text: &str, max_width: usize) -> &str {
    if max_width == 0 || text.is_empty() {
        return "";
    }
    let mut width = 0usize;
    let mut start = text.len();
    for (i, ch) in text.char_indices().rev() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width.saturating_add(w) > max_width {
            break;
        }
        width += w;
        start = i;
    }
    &text[start..]
}

/// Tail of live thinking shown in the 5-line window: last `THINKING_LIVE_LINES`
/// logical lines, clipped to about that many columns at `width` so a
/// newline-less blob cannot fill the screen.
fn live_thinking_body(content: &str, width: u16) -> &str {
    let slice = last_lines(content, THINKING_LIVE_LINES);
    let budget = usize::from(width.max(1)).saturating_mul(THINKING_LIVE_LINES);
    if UnicodeWidthStr::width(slice) <= budget {
        slice
    } else {
        suffix_max_width(slice, budget)
    }
}

/// One past the last row reserved for the images at these line indices.
fn reserved_end(images: &[(String, usize, String)]) -> usize {
    images
        .iter()
        .map(|(_, pos, _)| pos + IMAGE_HEIGHT)
        .max()
        .unwrap_or(0)
}

/// End a reply's markdown lines as the transcript shows them.
///
/// `lines` are the last of the reply's lines; the `before` lines that
/// precede them are left alone. Trailing blank rows are stripped so a
/// message does not end in a run of them — but never into the rows
/// reserved for an image (below line `reserved`): those are blank on
/// purpose, and dropping them let the image overlay paint over whatever
/// followed the message. A streaming reply then gets its cursor, and
/// every reply a closing blank row.
fn finish_reply(
    lines: &mut Vec<Line<'static>>,
    before: usize,
    reserved: usize,
    streaming: bool,
    width: u16,
) {
    while before + lines.len() > reserved && lines.last().is_some_and(|l| l.spans.is_empty()) {
        lines.pop();
    }
    if streaming {
        // Append cursor to the last line of text (inline), unless that line
        // exactly fills the width — a full-width table row would wrap its
        // closing border away — then on a line of its own.
        if let Some(last_line) = lines
            .last_mut()
            .filter(|line| line.width() != usize::from(width))
        {
            last_line
                .spans
                .push(Span::styled("_", Style::default().fg(Color::DarkGray)));
        } else {
            // No content yet, show cursor on its own line
            lines.push(Line::from(Span::styled(
                "_",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    lines.push(Line::from(""));
}

/// A streaming reply laid out frame after frame.
///
/// Each frame works only on what earlier frames left unfinished. The
/// reply's display text (control tags replaced) settles at the
/// reply's last line break once no `<artifact` or `<tunnel` opening
/// before it can still become a tag ([`replace_artifact_tags`],
/// [`tunnel_tags_settled`]): no tag then spans the break, so both passes
/// give the same text run over the whole reply as over its two sides. An
/// opening still waiting for its end keeps the text from its line on
/// unsettled. Settled text goes to the markdown renderer as final, the
/// rest as its tail.
#[derive(Debug)]
pub(super) struct StreamingReply {
    /// Bytes of the reply whose display text has settled.
    settled: usize,
    markdown: IncrementalMarkdown,
}

/// One frame of a [`StreamingReply`].
///
/// Its lines are the reply's markdown lines finished as [`element_lines`]
/// finishes a reply's.
#[derive(Debug)]
pub(super) struct ReplyFrame<'a> {
    /// Leading lines no later text changes.
    pub settled: &'a [Line<'static>],
    /// `(url, line index, caption)` of the images among `settled`.
    pub settled_images: &'a [(String, usize, String)],
    /// The lines after `settled`.
    pub open: Vec<Line<'static>>,
    /// The images among `open`, their line indices counted from the first
    /// settled line.
    pub open_images: Vec<(String, usize, String)>,
}

impl StreamingReply {
    /// A reply with no text yet, laid out at `width` columns.
    pub(super) fn new(images_supported: bool, width: u16) -> Self {
        Self {
            settled: 0,
            markdown: IncrementalMarkdown::new(images_supported, usize::from(width)),
        }
    }

    /// Lay out the reply holding `content`.
    ///
    /// `content` is the previous frame's content with text appended.
    pub(super) fn render(
        &mut self,
        content: &str,
        tunnel_base: &str,
        width: u16,
    ) -> ReplyFrame<'_> {
        let pending = &content[self.settled..];
        let cut = pending.rfind('\n').map_or(0, |at| at + 1);
        let (head, tail) = pending.split_at(cut);
        let (head_text, closed) = replace_artifact_tags(head);
        let head_is_final = closed && tunnel_tags_settled(&head_text);
        let (settled_text, open_source) = if head_is_final {
            self.settled += cut;
            (resolve_tunnel_tags(&head_text, tunnel_base), tail)
        } else {
            (Cow::Borrowed(""), pending)
        };
        let open_text = artifact_tag_text(open_source);
        let open_text = resolve_tunnel_tags(&open_text, tunnel_base);
        let frame = self.markdown.render(&settled_text, &open_text);

        let committed = frame.committed;
        let committed_images = frame.committed_images;
        // Lines below `kept` stay in the reply whatever follows: each is
        // not blank or lies in an image's reserved rows, so stripping stops
        // above them. The last of them may still take the cursor.
        let kept = committed
            .iter()
            .rposition(|line| !line.spans.is_empty())
            .map_or(0, |at| at + 1)
            .max(reserved_end(committed_images))
            .min(committed.len());
        let settled = kept.saturating_sub(1);
        let settled_images = committed_images.partition_point(|(_, line, _)| *line < settled);
        let mut open = committed[settled..].to_vec();
        open.extend(frame.open);
        let mut open_images = committed_images[settled_images..].to_vec();
        open_images.extend(frame.open_images);
        let reserved = reserved_end(committed_images).max(reserved_end(&open_images));
        finish_reply(&mut open, settled, reserved, true, width);
        ReplyFrame {
            settled: &committed[..settled],
            settled_images: &committed_images[..settled_images],
            open,
            open_images,
        }
    }
}

/// Lay out one element as styled lines (before wrapping).
pub(super) fn element_lines(
    elem: &ChatElement,
    state: ElementState,
    images_supported: bool,
    tunnel_base: &str,
    width: u16,
) -> ElementLines {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut images: Vec<(String, usize, String)> = Vec::new();
    let mut regions: Vec<(String, usize, usize)> = Vec::new();
    match elem {
        ChatElement::UserMessage {
            content,
            attachments,
            ..
        } => {
            // The block's own top padding row, shaded with the rest of the
            // message: it never depends on what precedes the message.
            lines.push(Line::from(""));
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
                push_attachment_line(&mut lines, att, images_supported, Some(&mut images));
            }

            lines.push(Line::from(""));
        }

        ChatElement::AssistantText {
            content,
            streaming,
            translation,
        } => {
            // A translated reply shows the translation under a clickable
            // marker; the marker swaps in the original.
            let content = translation.as_ref().map_or(content, |translation| {
                let start = lines.len();
                lines.push(Line::from(Span::styled(
                    translation.marker(),
                    Style::default().fg(Color::DarkGray),
                )));
                regions.push((translation.id.clone(), start, lines.len()));
                if translation.show_original {
                    content
                } else {
                    &translation.text
                }
            });
            // Track current line offset before adding markdown
            let line_offset = lines.len();

            // Render markdown with position tracking
            let image_mode = if images_supported {
                markdown::ImageSupport::Enabled
            } else {
                markdown::ImageSupport::Disabled
            };
            let display = artifact_tag_text(content);
            let content = resolve_tunnel_tags(&display, tunnel_base);
            let md_result =
                markdown::render_markdown_with_positions(&content, image_mode, usize::from(width));
            let reserved = reserved_end(&md_result.image_positions);
            let mut md_lines = md_result.lines;
            finish_reply(&mut md_lines, 0, reserved, *streaming, width);

            // Collect image positions with offset adjustment
            for (url, pos, caption) in md_result.image_positions {
                images.push((url, line_offset + pos, caption));
            }

            lines.extend(md_lines);
        }

        ChatElement::ToolCall { id, display } => {
            let start = lines.len();
            let mut tool_lines = Vec::new();
            render_tool_inline_at(
                &mut tool_lines,
                display,
                state.selected,
                images_supported,
                &mut images,
                state.open,
                state.spinner_frame,
            );
            lines.extend(tool_lines.into_iter().map(line_into_static));
            if display.expandable() {
                regions.push((id.clone(), start, lines.len()));
            }
        }

        ChatElement::TodoBlock { .. } if state.hide_todos => {}
        ChatElement::TodoBlock { rows } => {
            lines.push(Line::from(Span::styled(
                "[todo] ────────────────────────────",
                Style::default().fg(Color::Green),
            )));

            // Roots, compactly: the issues the session serves, on one line.
            let roots: Vec<&str> = rows
                .iter()
                .filter(|row| row.section == TaskSection::Roots)
                .map(|row| row.title.as_str())
                .collect();
            if !roots.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("  Roots ", Style::default().fg(Color::DarkGray)),
                    Span::raw(roots.join(", ")),
                ]));
            }
            for (section, title) in [
                (TaskSection::Doing, "Doing"),
                (TaskSection::Queued, "Queued"),
            ] {
                let mut section_rows = rows.iter().filter(|row| row.section == section).peekable();
                if section_rows.peek().is_none() {
                    continue;
                }
                lines.push(Line::from(Span::styled(
                    format!("  {title}"),
                    Style::default().fg(Color::DarkGray),
                )));
                for row in section_rows {
                    let text = row.title.split_whitespace().collect::<Vec<_>>().join(" ");
                    match &row.blocking {
                        Some(what) => lines.push(Line::from(vec![
                            Span::styled("    ⏸ ", Style::default().fg(Color::Red)),
                            Span::raw(text),
                            Span::styled(
                                format!(" (blocked: {what})"),
                                Style::default().fg(Color::Red),
                            ),
                        ])),
                        None => lines.push(Line::from(vec![
                            Span::styled("    ○ ", Style::default().fg(Color::Yellow)),
                            Span::raw(text),
                        ])),
                    }
                }
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
                Span::styled(message.clone(), Style::default().fg(Color::Red)),
            ]));
            lines.push(Line::from(""));
        }

        ChatElement::Warning { message } => {
            for (index, line) in message.lines().enumerate() {
                let icon = if index == 0 { "⚠ " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(icon, Style::default().fg(Color::Yellow)),
                    Span::styled(line.to_string(), Style::default().fg(Color::Yellow)),
                ]));
            }
            lines.push(Line::from(""));
        }

        ChatElement::Welcome { cwd } => {
            lines.extend(super::welcome::welcome_lines(cwd, width));
        }

        ChatElement::Notice { message } => {
            lines.push(Line::from(vec![
                Span::styled("◈ ", Style::default().fg(Color::Cyan)),
                Span::styled(message.clone(), Style::default().fg(Color::DarkGray)),
            ]));
            lines.push(Line::from(""));
        }

        ChatElement::Recap {
            summary,
            next_action,
        } => {
            lines.push(Line::from(vec![
                Span::styled("◈ ", Style::default().fg(Color::Cyan)),
                Span::styled("Recap ", Style::default().fg(Color::Cyan)),
                Span::styled(summary.clone(), Style::default().fg(Color::DarkGray)),
            ]));
            if let Some(next_action) = next_action {
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        format!("Next: {next_action}"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            lines.push(Line::from(""));
        }

        ChatElement::ContextReport(report) => {
            lines.extend(context_report_lines(report));
        }

        ChatElement::UsageReport(report) => {
            lines.extend(usage_report_lines(report));
        }

        ChatElement::Thinking {
            id,
            content,
            seconds,
            expanded,
            live,
            ..
        } => {
            let start = lines.len();
            let header = if *live {
                if thought_millis(*seconds) == 0 {
                    "◐ Thinking...".to_string()
                } else {
                    format!("◐ Thinking... ({})", thought_duration_label(*seconds))
                }
            } else {
                // An empty block is a timing marker only: no expand affordance.
                let arrow = match (content.is_empty(), *expanded) {
                    (true, _) => "",
                    (false, true) => " ▾",
                    (false, false) => " ▸",
                };
                format!("◐ Thought for {}{arrow}", thought_duration_label(*seconds))
            };
            lines.push(Line::from(Span::styled(
                header,
                Style::default().fg(Color::DarkGray),
            )));
            if *expanded {
                let body = if *live {
                    live_thinking_body(content, width)
                } else {
                    content.as_str()
                };
                let mut md_lines = markdown::render_markdown_with_positions(
                    body,
                    markdown::ImageSupport::Disabled,
                    usize::from(width),
                )
                .lines;
                while md_lines.last().is_some_and(|l| l.spans.is_empty()) {
                    md_lines.pop();
                }
                let dim = Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);
                lines.extend(md_lines.into_iter().map(|line| {
                    Line::from(
                        line.spans
                            .into_iter()
                            .map(|span| Span::styled(span.content, span.style.patch(dim)))
                            .collect::<Vec<_>>(),
                    )
                }));
            }
            lines.push(Line::from(""));
            if !content.is_empty() {
                regions.push((id.clone(), start, lines.len()));
            }
        }

        ChatElement::WorkedFor {
            seconds,
            elements,
            expanded,
            ..
        } => {
            let arrow = if *expanded { " ▾" } else { " ▸" };
            lines.push(Line::from(Span::styled(
                format!("✦ Worked for {}{arrow}", compact_seconds(*seconds)),
                Style::default().fg(Color::DarkGray),
            )));
            if *expanded {
                for inner in elements {
                    // Members carry their own expansion state — the group's
                    // `state` describes the group row itself.
                    let inner_state =
                        ElementState::of(inner, None, state.hide_todos, state.spinner_frame);
                    let sub =
                        element_lines(inner, inner_state, images_supported, tunnel_base, width);
                    let offset = lines.len();
                    lines.extend(sub.lines);
                    regions.extend(
                        sub.regions
                            .into_iter()
                            .map(|(id, start, end)| (id, offset + start, offset + end)),
                    );
                    images.extend(
                        sub.images
                            .into_iter()
                            .map(|(url, row, mime)| (url, offset + row, mime)),
                    );
                }
            }
            lines.push(Line::from(""));
        }
    }
    ElementLines {
        lines,
        images,
        regions,
    }
}

/// Detach a line from the data it borrows.
pub(super) fn line_into_static(line: Line<'_>) -> Line<'static> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .into_iter()
            .map(|span| Span::styled(span.content.into_owned(), span.style))
            .collect(),
    }
}

/// Internal implementation for rendering chat stream.
///
/// Elements come from `layout`, which re-lays-out only what changed; the
/// tail (queue, input, status) is rebuilt every frame since it is small and
/// carries the spinner. Only the lines intersecting the view are handed to
/// the paragraph, so a frame costs O(view), not O(history).
fn render_impl(
    frame: &mut Frame,
    area: Rect,
    chat: &ChatStream,
    layout: &mut ChatLayout,
    scroll: usize,
    ctx: RenderContext<'_>,
) -> crate::tui::perf::ChatRenderStats {
    let mut stats = crate::tui::perf::ChatRenderStats::default();
    // Guard against zero-dimension areas to prevent overflow
    if area.width < 3 || area.height < 3 {
        if let Some(out) = ctx.image_positions_out {
            out.clear();
        }
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
        hide_todos,
        image_positions_out,
        spinner_frame,
        band_bg,
    } = ctx;

    // Chat, queue and band content share the padded column; the band
    // background reaches the column's edges.
    let inner = band::inset(area);

    let build_start = std::time::Instant::now();
    layout.sync(
        chat,
        inner.width,
        images_supported,
        hide_todos,
        selected_tool,
        spinner_frame,
    );
    stats.elements = layout.rebuilt();

    // Tail: queue indicator, then the band (input line with attachment
    // previews, status line).
    let Tail {
        lines: tail,
        band_start,
        greeting_idx: greeting_line_idx,
        input_idx: input_line_idx,
        caret_line,
        images: tail_images,
    } = build_tail(
        queue_ctx.as_ref(),
        input_ctx.as_ref(),
        status_ctx.as_ref(),
        inner.width,
        images_supported,
        input_caret_active,
    );
    stats.lines_build = build_start.elapsed();

    let wrap_start = std::time::Instant::now();
    let mut tail_starts = Vec::with_capacity(tail.len());
    let mut tail_rows = 0usize;
    for line in &tail {
        tail_starts.push(tail_rows);
        tail_rows += chat_layout::line_rows(line, inner.width);
    }
    let element_rows = layout.element_rows();
    let content_height = element_rows + tail_rows;
    stats.wrap_count = wrap_start.elapsed();
    stats.content_height = content_height;

    let view_height = area.height as usize;
    if let Some(cell) = content_height_out {
        cell.store(content_height, Ordering::Relaxed);
    }
    if let Some(cell) = view_height_out {
        cell.store(view_height, Ordering::Relaxed);
    }

    // Underflowing content sits at the bottom, above blank padding rows.
    let top_padding = view_height.saturating_sub(content_height);
    let top = if content_height <= view_height {
        0
    } else {
        scroll.min(content_height - view_height)
    };
    layout.set_top_padding(top_padding);

    // Lines intersecting the window [top, top + view_height).
    let (mut lines, mut skip) = layout.visible_lines(top, view_height);
    if top + view_height > element_rows {
        if top >= element_rows {
            // The window starts inside the tail.
            let row = top - element_rows;
            let first = tail_starts
                .partition_point(|&start| start <= row)
                .saturating_sub(1);
            skip = row - tail_starts.get(first).copied().unwrap_or(0);
            lines.extend(tail.iter().skip(first).cloned().map(line_into_static));
        } else {
            lines.extend(tail.iter().cloned().map(line_into_static));
        }
    }

    if let Some(out) = image_positions_out {
        out.clear();
        let to_screen = |row: usize| {
            i32::try_from(top_padding + row).unwrap_or(i32::MAX)
                - i32::try_from(top).unwrap_or(i32::MAX)
        };
        for anchor in layout.images_in(top, view_height) {
            out.push(ImagePlacement {
                top: to_screen(anchor.row),
                key: anchor.key,
                caption: anchor.caption,
            });
        }
        for (key, index, caption) in tail_images {
            let row = element_rows + tail_starts.get(index).copied().unwrap_or(0);
            if row < top + view_height && row + IMAGE_HEIGHT > top {
                out.push(ImagePlacement {
                    top: to_screen(row),
                    key,
                    caption,
                });
            }
        }
    }

    let widget_start = std::time::Instant::now();
    frame.render_widget(Clear, area);
    let body = Rect {
        y: inner.y + u16::try_from(top_padding).unwrap_or(0),
        height: inner
            .height
            .saturating_sub(u16::try_from(top_padding).unwrap_or(0)),
        ..inner
    };
    // Rows come pre-wrapped so none reaches past the column (see `wrap`).
    Paragraph::new(super::wrap::wrap_lines(&lines, inner.width))
        .scroll((u16::try_from(skip).unwrap_or(u16::MAX), 0))
        .render(body, frame.buffer_mut());

    // The band background: every cell of the band's rows and of user
    // message rows — padding columns and trailing space included.
    // Cells with a background of their own keep it. The band span runs to
    // the content's end even when it is scrolled out of view, so each
    // span's rows clamp to the window.
    if let Some(bg) = band_bg {
        let mut spans = layout.user_rows_in(top, view_height);
        if let Some(start) = band_start {
            let first = element_rows + tail_starts.get(start).copied().unwrap_or(0);
            spans.push((first, element_rows + tail_rows));
        }
        for (first, last) in spans {
            for row in first.max(top)..last.min(top + view_height) {
                let y = area.y + u16::try_from(top_padding + row - top).unwrap_or(u16::MAX);
                for x in area.x..area.x + area.width {
                    let cell = &mut frame.buffer_mut()[(x, y)];
                    if cell.bg == Color::Reset {
                        cell.set_bg(bg);
                    }
                }
            }
        }
    }

    // Plain mirror of the tail, for selection.
    layout.set_tail_plain(chat_layout::plain_rows(&tail, inner.width, tail_rows));
    layout.set_greeting_row(
        greeting_line_idx.map(|index| element_rows + tail_starts.get(index).copied().unwrap_or(0)),
    );
    layout.set_input_span(input_line_idx.map(|index| {
        (
            element_rows + tail_starts.get(index).copied().unwrap_or(0),
            chat_layout::line_rows(&tail[index], inner.width),
        )
    }));

    // Anchor the terminal cursor on the caret cell so IME composition and
    // its candidate window track the real insertion point.
    if let (Some(caret_line), Some(input_idx)) = (caret_line, input_line_idx)
        && let Some((cx, cy)) = wrapped_tail_cell(&caret_line, inner.width)
    {
        let caret_row = top_padding
            + element_rows
            + tail_starts.get(input_idx).copied().unwrap_or(0)
            + usize::from(cy);
        if caret_row >= top && caret_row - top < usize::from(area.height) {
            frame.set_cursor_position((
                inner.x + cx,
                area.y + u16::try_from(caret_row - top).unwrap_or(u16::MAX),
            ));
        }
    }
    stats.widget = widget_start.elapsed();
    stats
}

/// The rows below the chat, built for one frame.
struct Tail {
    lines: Vec<Line<'static>>,
    /// Index in `lines` of the band's first (padding) row; the band runs to
    /// the last row. `None` when there is no band.
    band_start: Option<usize>,
    greeting_idx: Option<usize>,
    input_idx: Option<usize>,
    /// The input up to the caret, when the caret owns the terminal cursor.
    caret_line: Option<Line<'static>>,
    /// Attached image blocks: `(key, index in lines, caption)`.
    images: Vec<(String, usize, String)>,
}

/// Build the tail at the content `width`: queue lines, one plain margin row,
/// then the band — a padding row, the greeting and input line(s), a blank
/// row, the status line and a padding row. A band without an input or a
/// status line leaves out the blank row.
fn build_tail(
    queue_ctx: Option<&QueueContext<'_>>,
    input_ctx: Option<&InputContext<'_>>,
    status_ctx: Option<&StatusContext<'_>>,
    width: u16,
    images_supported: bool,
    caret_active: bool,
) -> Tail {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let mut images = Vec::new();
    if let Some(ctx) = queue_ctx
        && !ctx.messages.is_empty()
    {
        render_queue_lines(&mut lines, ctx, width);
    }
    let mut band_start = None;
    let (mut greeting_idx, mut input_idx, mut caret_line) = (None, None, None);
    if input_ctx.is_some() || status_ctx.is_some() {
        lines.push(Line::from(""));
        band_start = Some(lines.len());
        lines.push(Line::from(""));
    }
    if let Some(ctx) = input_ctx {
        if let Some(text) = ctx.greeting {
            greeting_idx = Some(lines.len());
            lines.push(greeting_line(text));
        }
        if caret_active {
            caret_line = Some(input_caret_line(ctx));
        }
        input_idx = Some(render_input_line(
            &mut lines,
            ctx,
            width,
            images_supported,
            Some(&mut images),
        ));
    }
    if let Some(ctx) = status_ctx {
        if input_ctx.is_some() {
            lines.push(Line::from(""));
        }
        render_status_line(&mut lines, ctx, usize::from(width));
    }
    if band_start.is_some() {
        lines.push(Line::from(""));
    }
    Tail {
        lines: lines.into_iter().map(line_into_static).collect(),
        band_start,
        greeting_idx,
        input_idx,
        caret_line,
        images,
    }
}

/// The New Year greeting drawn above the input line.
fn greeting_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ))
}

/// Style of the leading `!` while the composer is in bash mode.
const BANG_PROMPT_STYLE: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);

/// `true` when the composer input is a bash-mode (`!`) command — the same
/// gate `submit_bash` uses.
fn bash_mode(input: &str) -> bool {
    input.starts_with('!')
}

/// Byte offset in `input` where the value of a `/secret KEY VALUE` command
/// begins; `None` for every other input.
fn secret_value_start(input: &str) -> Option<usize> {
    let rest = input.trim_start().strip_prefix("/secret")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    let key_end = rest.find(char::is_whitespace)?;
    let value = rest[key_end..].trim_start();
    if value.is_empty() {
        return None;
    }
    // `value` is a suffix of `input`.
    Some(input.len() - value.len())
}

/// `text` with one `•` per non-whitespace grapheme, so a masked cursor
/// still lands on a grapheme boundary of what is drawn.
fn mask_graphemes(text: &str) -> String {
    text.graphemes(true)
        .map(|cluster| {
            if cluster.chars().all(char::is_whitespace) {
                cluster
            } else {
                "•"
            }
        })
        .collect()
}

/// The part of the composer's visible text that is `text`, which sits at
/// byte offset `base` of `input`: a `/secret` value renders as `•`s.
fn composer_segment(text: &str, base: usize, secret_start: Option<usize>) -> Cow<'_, str> {
    let Some(start) = secret_start else {
        return Cow::Borrowed(text);
    };
    if base + text.len() <= start {
        return Cow::Borrowed(text);
    }
    let visible = start.saturating_sub(base);
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..visible]);
    out.push_str(&mask_graphemes(&text[visible..]));
    Cow::Owned(out)
}

/// The composer's visible text: `/secret KEY VALUE` renders the value as
/// one `•` per grapheme so a secret never sits on screen; every other
/// input shows verbatim.
fn composer_display(input: &str) -> Cow<'_, str> {
    composer_segment(input, 0, secret_value_start(input))
}

/// The composer's visible text split at the caret (`cursor`, a byte offset
/// of `input` on a grapheme boundary), each half masked like
/// [`composer_display`].
fn composer_display_split(input: &str, cursor: usize) -> (Cow<'_, str>, Cow<'_, str>) {
    let secret_start = secret_value_start(input);
    (
        composer_segment(&input[..cursor], 0, secret_start),
        composer_segment(&input[cursor..], cursor, secret_start),
    )
}

/// The input line's leading spans: the bash-mode prompt and the attachment
/// chips. Their width is what the typed text is offset by on the first row.
fn input_prefix_spans(input: &str, attachments: &[Attachment]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();

    // The composer has no `>` prompt marker: input starts flush left and
    // a leading `!` gets the bash-mode highlight instead of a prompt cell.
    if bash_mode(input) {
        spans.push(Span::styled("! ", BANG_PROMPT_STYLE));
    }

    // Render image attachments
    for att in attachments {
        if matches!(att, Attachment::Image { .. }) {
            spans.push(Span::styled(
                "[image] ",
                Style::default().bg(Color::DarkGray),
            ));
        }
    }

    // File references
    for att in attachments {
        if let Attachment::FileReference { path } = att {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
            spans.push(Span::styled(
                format!("@{name} "),
                Style::default().fg(Color::Cyan),
            ));
        }
    }

    // Pasted text attachments
    for att in attachments {
        if let Attachment::PastedText { content, .. } = att {
            let char_count = content.chars().count();
            spans.push(Span::styled(
                format!("[pasted: {char_count} chars] "),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    spans
}

/// The byte offset of the cursor a click `column` cells into the input lands on.
///
/// `column` counts from the left edge of the input line's first row, the
/// widths of the rows above the clicked one included. The result is the
/// boundary before the grapheme the click hits, measured with the widths the
/// renderer draws, and `input.len()` past the end.
#[must_use]
pub fn caret_at_column(input: &str, attachments: &[Attachment], column: usize) -> usize {
    let prefix_width: usize = input_prefix_spans(input, attachments)
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let target = column.saturating_sub(prefix_width);
    // The leading `!` of a bash-mode input is drawn as part of the prefix.
    let first = usize::from(bash_mode(input));
    let secret_start = secret_value_start(input);
    let mut covered = 0usize;
    for (offset, cluster) in input[first..].grapheme_indices(true) {
        let at = first + offset;
        let masked = secret_start.is_some_and(|start| at >= start)
            && !cluster.chars().all(char::is_whitespace);
        let width = if masked {
            UnicodeWidthStr::width("•")
        } else {
            drawn_width(cluster)
        };
        if covered + width > target {
            return at;
        }
        covered += width;
    }
    input.len()
}

/// The input `Line` up to and including the caret marker: prompt,
/// attachment chips and the text before the caret. The caret marker's
/// cell is where the terminal cursor — and therefore IME composition —
/// anchors.
fn input_caret_line(ctx: &InputContext<'_>) -> Line<'static> {
    let mut spans = input_prefix_spans(ctx.input, ctx.attachments);
    let bash = bash_mode(ctx.input);
    let (before, _) = composer_display_split(ctx.input, ctx.cursor);
    // The bash prompt already draws the leading `!`.
    let start = usize::from(bash);
    spans.push(Span::raw(before.get(start..).unwrap_or("").to_string()));
    spans.push(Span::styled("│", Style::default().fg(Color::Yellow)));
    Line::from(spans)
}

/// Cell of `line`'s last grapheme once wrapped to `width`: the caret cell
/// of the input caret line. Rendering the same pre-wrapped rows into a
/// scratch buffer keeps this exactly in step with the chat's wrapping
/// (wide graphemes included).
fn wrapped_tail_cell(line: &Line<'static>, width: u16) -> Option<(u16, u16)> {
    if width == 0 {
        return None;
    }
    let wrapped = super::wrap::wrap_line(line, width);
    let rows = u16::try_from(wrapped.len()).unwrap_or(u16::MAX);
    if rows == 0 {
        return None;
    }
    let buf_area = Rect::new(0, 0, width, rows);
    let mut buf = Buffer::empty(buf_area);
    Paragraph::new(wrapped).render(buf_area, &mut buf);
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
    width: u16,
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

    // Input text with caret (a byte offset on a grapheme boundary). The real
    // terminal cursor marks the caret — the `│` stays only in the
    // scratch anchor used for cursor placement.
    let mut line = input_caret_line(ctx);
    line.spans.pop();
    let display = composer_display(ctx.input);
    let (before, after) = composer_display_split(ctx.input, ctx.cursor);
    // The bash prompt already draws the leading `!`, which sits before the
    // caret unless the caret is at the very start.
    let skipped = usize::from(bash_mode(&display)).saturating_sub(before.len());
    line.spans
        .push(Span::raw(after.get(skipped..).unwrap_or("").to_string()));

    // Ghost text trails the caret of an empty input.
    if display.is_empty()
        && let Some(suggestion) = ctx.suggestion
    {
        let ghost = format!("{suggestion}  (tab to accept)");
        line.spans
            .push(Span::styled(ghost, Style::default().fg(Color::DarkGray)));
    }

    // The bash-mode affordance hugs the input line, below any attachment
    // previews.
    if bash_mode(&display) {
        const LABEL: &str = " (bash mode) ";
        let fill = usize::from(width).saturating_sub(UnicodeWidthStr::width(LABEL));
        lines.push(Line::from(Span::styled(
            format!("{LABEL}{}", "─".repeat(fill)),
            Style::default().fg(Color::DarkGray),
        )));
    }

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

    if ctx.yolo {
        parts.push("yolo on · /yolo".to_string());
    }

    if ctx.open_questions > 0 {
        parts.push(format!("? {} open · /questions", ctx.open_questions));
    }

    if let Some(background) = background_summary(ctx.shells, ctx.subagents) {
        parts.push(background);
    }

    // Context usage pinned to the right edge: `4.8K / 120.0K (4%)`.
    let usage = match ctx.compaction {
        CompactionMeter::Compacting => Some(StatusUsage {
            usage: "Compacting".to_string(),
            warn: None,
        }),
        CompactionMeter::Compacted { .. } => Some(StatusUsage {
            usage: "Compacted".to_string(),
            warn: None,
        }),
        CompactionMeter::Idle => ctx.usage.map(|(prompt_tokens, context_window)| {
            format_status_usage(prompt_tokens, context_window, &ctx.context_policy)
        }),
    };

    // The worktree index: fill bar while incomplete, `index ✓` once full,
    // red `index ✗` when repo-search is down.
    let index_bar = index_bar(
        ctx.index_coverage,
        ctx.index_failed,
        ctx.index_loading.then_some(ctx.spinner_frame),
    );

    let status_text = parts.join(" · ");
    let status_style = Style::default().fg(Color::Cyan);
    let warn_style = Style::default().fg(Color::Yellow);

    let bar_width = index_bar.as_ref().map_or(0, |bar| {
        bar.iter().map(Span::width).sum::<usize>() + " · ".width()
    });
    if let Some(usage) = usage {
        let pad = width
            .saturating_sub(status_text.width() + bar_width + usage.width())
            .max(1);
        let mut spans = vec![Span::styled(status_text, status_style)];
        if let Some(bar) = index_bar {
            spans.push(Span::styled(" · ", status_style));
            spans.extend(bar);
        }
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(usage.usage, status_style));
        if let Some(warn) = usage.warn {
            spans.push(Span::styled(" · ", status_style));
            spans.push(Span::styled(warn, warn_style));
        }
        lines.push(Line::from(spans));
    } else {
        let mut spans = vec![Span::styled(status_text, status_style)];
        if let Some(bar) = index_bar {
            spans.push(Span::styled(" · ", status_style));
            spans.extend(bar);
        }
        lines.push(Line::from(spans));
    }
}

/// Status-bar counts for running shells and subagents: `2 shells, 1 subagent ↓`.
/// A class with count zero is omitted. `None` when nothing is running.
fn background_summary(shells: usize, subagents: usize) -> Option<String> {
    match (shells, subagents) {
        (0, 0) => None,
        (shells, 0) => Some(format!("{} ↓", counted(shells, "shell", "shells"))),
        (0, subagents) => Some(format!("{} ↓", counted(subagents, "subagent", "subagents"))),
        (shells, subagents) => Some(format!(
            "{}, {} ↓",
            counted(shells, "shell", "shells"),
            counted(subagents, "subagent", "subagents")
        )),
    }
}

fn counted(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// Cells in the status bar's index progress bar.
const INDEX_BAR_CELLS: usize = 8;

/// The status bar's index coverage segment: `index ▕██████░░░░▏ 62%` while
/// incomplete, `index ✓` once full, `index ✗` in red when repo-search is
/// down, `index` with a spinner at `loading` frame until the first snapshot;
/// `None` when there is nothing to index.
fn index_bar(
    coverage: Option<&crate::client::IndexCoverage>,
    failed: bool,
    loading: Option<usize>,
) -> Option<Vec<Span<'static>>> {
    if failed {
        return Some(vec![Span::styled(
            "index ✗",
            Style::default().fg(Color::Red),
        )]);
    }
    if let Some(frame) = loading {
        let spinner = SPINNER_FRAMES[frame % SPINNER_FRAMES.len()];
        return Some(vec![Span::styled(
            format!("index {spinner}"),
            Style::default().fg(Color::DarkGray),
        )]);
    }
    let coverage = coverage?;
    let total = coverage.total;
    if total == 0 {
        return None;
    }
    let bar_style = Style::default().fg(Color::Blue);
    if coverage.with_vector >= total {
        return Some(vec![Span::styled("index ✓", bar_style)]);
    }
    let filled = usize::try_from(coverage.with_vector * INDEX_BAR_CELLS as u64 / total)
        .expect("a share of at most the cell count");
    let percent = coverage.with_vector * 100 / total;
    Some(vec![
        Span::styled("index ▕", bar_style),
        Span::styled("█".repeat(filled), bar_style),
        Span::styled(
            "░".repeat(INDEX_BAR_CELLS - filled),
            bar_style.add_modifier(Modifier::DIM),
        ),
        Span::styled(format!("▏ {percent}%"), bar_style),
    ])
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

/// Remaining percent of the auto-compact window below which the status bar
/// warns that compaction is close.
const AUTOCOMPACT_WARN_REMAINING_PCT: u8 = 10;

/// Right-edge context usage for the status bar.
struct StatusUsage {
    /// `used / autocompact-window (percent%)`, or a bare count with no window.
    usage: String,
    /// Remaining-to-compact hint when headroom is under 10%.
    warn: Option<String>,
}

impl StatusUsage {
    fn width(&self) -> usize {
        self.usage.width()
            + self
                .warn
                .as_ref()
                .map_or(0, |warn| " · ".width() + warn.width())
    }
}

/// Format prompt-token usage against the auto-compact window.
fn format_status_usage(
    prompt_tokens: u64,
    context_window: Option<u64>,
    policy: &aither_agent::ContextPolicy,
) -> StatusUsage {
    let used = format_number(usize::try_from(prompt_tokens).unwrap_or(usize::MAX));
    let Some(window) = context_window else {
        return StatusUsage {
            usage: used,
            warn: None,
        };
    };
    let compact = policy.pause_point(u32::try_from(window).unwrap_or(u32::MAX));
    if compact == 0 {
        return StatusUsage {
            usage: used,
            warn: None,
        };
    }
    let used_n = usize::try_from(prompt_tokens).unwrap_or(usize::MAX);
    #[allow(clippy::cast_precision_loss)]
    let percent = used_n as f64 / compact as f64 * 100.0;
    let usage = format!("{} / {} ({percent:.0}%)", used, format_number(compact));
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let remaining_pct =
        ((compact.saturating_sub(used_n) as f64 / compact as f64) * 100.0).round() as u8;
    let warn = (remaining_pct < AUTOCOMPACT_WARN_REMAINING_PCT)
        .then(|| format!("{remaining_pct}% to auto-compact"));
    StatusUsage { usage, warn }
}

/// Render a tool call inline with optional selection highlight.
/// Marker prefix shown before a terminal command for its execution
/// backend. A host-run (`unsafe`) command is marked by the red status
/// icon instead — see [`status_icon`].
const fn terminal_mode_marker(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::Sandboxed | SandboxMode::Unsandboxed => "",
    }
}

/// Marker prefix naming the SSH server a command runs on; empty for the
/// home environment.
fn terminal_location_marker(location: Option<&str>) -> String {
    location.map_or_else(String::new, |name| format!("[ssh {name}] "))
}

/// The icon prefixing a call's headline — spinner while running, tick on
/// success, cross on failure — and its style. An `unsafe` (host-run)
/// command is red in every state so it stands out from sandboxed calls
/// at a glance.
fn status_icon(status: &ToolStatus, spinner_frame: usize, unsafe_exec: bool) -> (char, Style) {
    let state_color = if unsafe_exec {
        Color::Red
    } else {
        match status {
            ToolStatus::Running => Color::DarkGray,
            ToolStatus::Done => Color::Green,
            ToolStatus::Failed(_) => Color::Red,
        }
    };
    let icon = match status {
        ToolStatus::Running => SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()],
        ToolStatus::Done => '✓',
        ToolStatus::Failed(_) => '✗',
    };
    (icon, Style::default().fg(state_color))
}

/// A tool call's styled headline: expand marker + status icon +
/// chip/description + collapsed status suffix.
fn tool_header_line(
    display: &ToolDisplay,
    selection_style: Style,
    open: bool,
    spinner_frame: usize,
) -> Line<'_> {
    let marker = if !display.expandable() {
        "  "
    } else if open {
        "▾ "
    } else {
        "▸ "
    };
    let dim = Style::default().fg(Color::DarkGray);
    let unsafe_exec = matches!(
        display,
        ToolDisplay::Terminal {
            mode: SandboxMode::Unsandboxed,
            ..
        }
    );
    let (icon, icon_style) = status_icon(display.status(), spinner_frame, unsafe_exec);
    let mut spans = vec![
        Span::styled(marker, dim),
        Span::styled(format!("{icon} "), icon_style),
    ];
    match display {
        ToolDisplay::Terminal {
            description,
            command,
            mode,
            location,
            diffs,
            ..
        } => {
            let location_marker = terminal_location_marker(location.as_deref());
            if description.is_empty() {
                // A `!` bash call headlines its command, highlighted like
                // the expanded body.
                spans.push(Span::styled(
                    terminal_mode_marker(*mode),
                    Style::default()
                        .fg(Color::Red)
                        .bold()
                        .patch(selection_style),
                ));
                spans.push(Span::styled(
                    location_marker,
                    Style::default()
                        .fg(Color::Red)
                        .bold()
                        .patch(selection_style),
                ));
                spans.push(Span::styled("$ ", dim.patch(selection_style)));
                spans.extend(
                    super::highlight::shell_spans(command.lines().next().unwrap_or_default())
                        .into_iter()
                        .flatten()
                        .map(|span| Span::styled(span.content, span.style.patch(selection_style))),
                );
            } else {
                if !location_marker.is_empty() {
                    spans.push(Span::styled(
                        location_marker,
                        Style::default()
                            .fg(Color::Red)
                            .bold()
                            .patch(selection_style),
                    ));
                }
                spans.push(Span::styled(description.clone(), selection_style));
            }
            // File changes the call caused, as a compact badge: which
            // files and how many lines, never the patch itself — the
            // command text already carries anything the model wrote.
            if !diffs.is_empty() {
                let files: usize = diffs.iter().map(|d| d.changes.len()).sum();
                let additions: usize = diffs.iter().map(|d| d.additions).sum();
                let deletions: usize = diffs.iter().map(|d| d.deletions).sum();
                let yellow = Style::default().fg(Color::Yellow);
                spans.push(Span::styled(
                    format!("  ± {files} file{} (", if files == 1 { "" } else { "s" }),
                    yellow,
                ));
                spans.push(Span::styled(
                    format!("+{additions}"),
                    Style::default().fg(Color::Green),
                ));
                spans.push(Span::styled(
                    format!(" −{deletions}"),
                    Style::default().fg(Color::Red),
                ));
                spans.push(Span::styled(")", yellow));
            }
        }
        ToolDisplay::Action { chip, summary, .. } => {
            let (label, rest) = semantic_headline(chip, summary);
            spans.push(Span::styled(label, chip_style(chip)));
            if !rest.is_empty() {
                spans.push(Span::styled(format!(" {rest}"), selection_style));
            }
        }
        ToolDisplay::Builtin { tool, detail, .. } => {
            let (label, rest) = semantic_headline(tool, detail);
            spans.push(Span::styled(label, chip_style(tool)));
            if !rest.is_empty() {
                spans.push(Span::styled(format!(" {rest}"), selection_style));
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

/// Search and fetch calls are one headline row (`Searched "query"`,
/// `Fetched url`) with nothing to unfold, whichever path renders them.
fn is_one_row_tool(name: &str) -> bool {
    matches!(
        name,
        "websearch" | "web_search" | "google_search" | "webfetch" | "web_fetch"
    )
}

/// The query or URL a hosted call's result repeats, when the provider
/// announced the call before it knew it (the `OpenAI` Responses API does).
fn quoted_result(result: &str) -> Option<&str> {
    result
        .trim()
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .filter(|inner| !inner.is_empty() && !inner.contains('\n'))
}

/// A tool headline as `(label, rest)`: hosted and IPC search names become
/// `Searched "query"`, fetch becomes `Fetched url`, everything else keeps
/// its `[name]` label.
fn semantic_headline(name: &str, summary: &str) -> (String, String) {
    match name {
        "websearch" | "web_search" | "google_search" => (
            "Searched".to_string(),
            if summary.is_empty() {
                String::new()
            } else {
                format!("\"{summary}\"")
            },
        ),
        "webfetch" | "web_fetch" => ("Fetched".to_string(), summary.to_string()),
        COMPACT_TOOL => ("Compaction".to_string(), String::new()),
        _ => (format!("[{name}]"), summary.to_string()),
    }
}

/// Color for an Action or Builtin chip label.
fn chip_style(chip: &str) -> Style {
    match chip {
        "kill" => Style::default().fg(Color::Red),
        "stdin" => Style::default().fg(Color::Yellow),
        "websearch" | "web_search" | "google_search" | "webfetch" | "web_fetch" => {
            Style::default().fg(Color::Blue)
        }
        _ => Style::default().fg(Color::Cyan),
    }
}

#[cfg(test)]
fn render_tool_inline<'a>(
    lines: &mut Vec<Line<'a>>,
    display: &'a ToolDisplay,
    is_selected: bool,
    images_supported: bool,
    image_positions: &mut Vec<(String, usize, String)>,
) {
    let open = display.expandable() && display.body_open();
    render_tool_inline_at(
        lines,
        display,
        is_selected,
        images_supported,
        image_positions,
        open,
        0,
    );
}

/// Inline rows of a tool call whose body is `open`.
fn render_tool_inline_at<'a>(
    lines: &mut Vec<Line<'a>>,
    display: &'a ToolDisplay,
    is_selected: bool,
    images_supported: bool,
    image_positions: &mut Vec<(String, usize, String)>,
    open: bool,
    spinner_frame: usize,
) {
    let selection_style = if is_selected {
        Style::default().bg(Color::DarkGray)
    } else {
        Style::default()
    };
    let dim = Style::default().fg(Color::DarkGray);

    lines.push(tool_header_line(
        display,
        selection_style,
        open,
        spinner_frame,
    ));
    // A call that ran one of May's commands shows what the command did, open
    // or folded; only a subagent's full result waits for the expansion.
    if let ToolDisplay::Terminal {
        card: Some(card), ..
    } = display
    {
        lines.extend(card_lines(card, selection_style, open));
        return;
    }
    if !open {
        // A folded edit keeps a compact diff; every other call folds to its
        // headline.
        if let ToolDisplay::Terminal {
            reading: Some(reading),
            ..
        } = display
        {
            lines.extend(edit_preview_lines(reading, selection_style));
        }
        return;
    }

    match display {
        ToolDisplay::Terminal {
            command,
            mode,
            location,
            status,
            output,
            reading,
            ..
        } => {
            let mode_prefix = format!(
                "{}{}",
                terminal_mode_marker(*mode),
                terminal_location_marker(location.as_deref())
            );
            // A script May recognises shows as what it does; one it does
            // not shows as the script, as before.
            let recognised = reading.as_ref().filter(|reading| reading.recognises_any());
            let mut command_rows = if recognised.is_some() {
                Vec::new()
            } else {
                super::highlight::shell_spans(command)
            }
            .into_iter();
            if let Some(reading) = recognised {
                lines.extend(reading_lines(reading, selection_style));
            } else if let Some(first) = command_rows.next() {
                let mut first_spans = vec![
                    Span::styled(
                        mode_prefix,
                        Style::default()
                            .fg(Color::Red)
                            .bold()
                            .patch(selection_style),
                    ),
                    Span::styled("$ ", dim.patch(selection_style)),
                ];
                first_spans.extend(
                    first
                        .into_iter()
                        .map(|span| Span::styled(span.content, span.style.patch(selection_style))),
                );
                lines.push(Line::from(first_spans));
                for row in command_rows {
                    let mut row_spans = vec![Span::styled("│ ", dim.patch(selection_style))];
                    row_spans.extend(
                        row.into_iter().map(|span| {
                            Span::styled(span.content, span.style.patch(selection_style))
                        }),
                    );
                    lines.push(Line::from(row_spans));
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

/// Diff rows an edit shows inline before the rest folds into a count; the
/// full-screen view still carries the whole script.
const EDIT_PREVIEW_ROWS: usize = 40;

/// What joins a step to the one before it.
const fn link_marker(link: Link) -> &'static str {
    match link {
        Link::Start => "",
        Link::Pipe => "| ",
        Link::And => "&& ",
        Link::Or => "|| ",
        Link::Then => "; ",
    }
}

/// Rows of the summary a subagent card shows folded.
const CARD_SUMMARY_CHARS: usize = 160;

/// Elapsed time as `1m 03s`.
fn elapsed_text(from: jiff::Timestamp, to: jiff::Timestamp) -> String {
    let seconds = to.duration_since(from).as_secs().max(0);
    if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// The first line of `text`, cut to [`CARD_SUMMARY_CHARS`] characters.
fn card_summary(text: &str) -> String {
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let mut summary: String = first.chars().take(CARD_SUMMARY_CHARS).collect();
    if first.chars().nth(CARD_SUMMARY_CHARS).is_some() || text.lines().nth(1).is_some() {
        summary.push('…');
    }
    summary
}

fn row_text(row: &RowView) -> String {
    if row.detail.is_empty() {
        format!("{} ({})", row.title, row.section)
    } else {
        format!(
            "{} ({}) — {}",
            row.title,
            row.section,
            row.detail.replace('\n', "; ")
        )
    }
}

/// The rows a card renders as under its call's headline. `open` shows a
/// subagent's whole result instead of its summary.
#[must_use]
pub fn card_lines(card: &Card, selection_style: Style, open: bool) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray).patch(selection_style);
    let mut lines = Vec::new();
    match card {
        Card::Tasks { changes } => {
            for change in changes {
                let label = match change.change {
                    RowChangeKind::Added => "added",
                    RowChangeKind::Started => "started",
                    RowChangeKind::Finished => "finished",
                    RowChangeKind::Blocked => "blocked",
                    RowChangeKind::Unblocked => "unblocked",
                    RowChangeKind::Edited => "edited",
                };
                lines.push(Line::from(Span::styled(
                    format!("  tasks {label}"),
                    Style::default().fg(Color::Cyan).patch(selection_style),
                )));
                if let Some(before) = &change.before {
                    lines.push(Line::from(Span::styled(
                        format!("  - {}", row_text(before)),
                        Style::default().fg(Color::Red).patch(selection_style),
                    )));
                }
                if let Some(after) = &change.after {
                    lines.push(Line::from(Span::styled(
                        format!("  + {}", row_text(after)),
                        Style::default().fg(Color::Green).patch(selection_style),
                    )));
                }
            }
        }
        Card::Subagent {
            agent,
            title,
            state,
            started_at,
            finished_at,
            result,
        } => {
            let status = match (state, finished_at) {
                (SubagentState::Running, _) => {
                    format!(
                        "running {}",
                        elapsed_text(*started_at, jiff::Timestamp::now())
                    )
                }
                (SubagentState::Done, Some(end)) => {
                    format!("done in {}", elapsed_text(*started_at, *end))
                }
                (SubagentState::Done, None) => "done".to_owned(),
                (SubagentState::Interrupted, _) => "interrupted".to_owned(),
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {agent}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .bold()
                        .patch(selection_style),
                ),
                Span::styled(format!(" · {title} · {status}"), dim),
            ]));
            if let Some(result) = result.as_deref().filter(|text| !text.trim().is_empty()) {
                if open {
                    lines.extend(
                        result
                            .lines()
                            .map(|row| Line::from(Span::styled(format!("  {row}"), dim))),
                    );
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", card_summary(result)),
                        dim,
                    )));
                }
            }
        }
        Card::AskUser {
            questions, answers, ..
        } => {
            for question in questions {
                lines.push(Line::from(Span::styled(
                    format!("  ? {}", question.question),
                    Style::default().fg(Color::Yellow).patch(selection_style),
                )));
                for option in &question.options {
                    lines.push(Line::from(Span::styled(format!("    · {option}"), dim)));
                }
                if let Some(answer) = answers
                    .iter()
                    .flatten()
                    .find(|answer| answer.section == question.section)
                {
                    lines.push(Line::from(Span::styled(
                        format!("  ✓ {}", answer.text),
                        Style::default().fg(Color::Green).patch(selection_style),
                    )));
                }
            }
            if answers.is_none() {
                lines.push(Line::from(Span::styled("  waiting for an answer", dim)));
            }
        }
    }
    lines
}

/// The rows a recognised script renders as: one line per step in plain
/// words (an unknown step as its raw command), and a line-numbered diff
/// under every edit.
#[must_use]
pub fn reading_lines(reading: &CommandReading, selection_style: Style) -> Vec<Line<'static>> {
    reading_rows(reading, selection_style, false, EDIT_PREVIEW_ROWS)
}

/// What a finished, collapsed call keeps of its edits: each file and a
/// compact diff preview capped at [`DIFF_PREVIEW_LINES`] rows. Empty when
/// the script edits nothing.
#[must_use]
pub fn edit_preview_lines(reading: &CommandReading, selection_style: Style) -> Vec<Line<'static>> {
    reading_rows(reading, selection_style, true, DIFF_PREVIEW_LINES)
}

fn reading_rows(
    reading: &CommandReading,
    selection_style: Style,
    edits_only: bool,
    max_diff_rows: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = Vec::new();
    let total_diff_rows: usize = reading
        .steps
        .iter()
        .filter_map(|step| match &step.intent {
            Intent::Edit { files } => Some(files),
            _ => None,
        })
        .flatten()
        .flat_map(|file| file_hunks(&file.change))
        .map(|(_, rows)| rows.len())
        .sum();
    let mut diff_rows = 0;
    for step in &reading.steps {
        if edits_only && !matches!(step.intent, Intent::Edit { .. }) {
            continue;
        }
        let marker = Span::styled(
            if edits_only {
                ""
            } else {
                link_marker(step.link)
            },
            dim.patch(selection_style),
        );
        if let Intent::Unknown { source } = &step.intent {
            let mut rows = source.lines();
            let first = rows.next().unwrap_or_default();
            lines.push(Line::from(vec![
                marker,
                Span::styled("$ ", dim.patch(selection_style)),
                Span::styled(
                    first.to_string(),
                    Style::default().fg(Color::Yellow).patch(selection_style),
                ),
            ]));
            for row in rows {
                lines.push(Line::from(Span::styled(
                    format!("│ {row}"),
                    dim.patch(selection_style),
                )));
            }
            continue;
        }
        if !edits_only {
            lines.push(Line::from(vec![
                marker,
                Span::styled(
                    step.line.clone(),
                    Style::default().fg(Color::Cyan).patch(selection_style),
                ),
            ]));
        }
        if let Intent::Edit { files } = &step.intent {
            for file in files {
                lines.push(Line::from(Span::styled(
                    format!("  {}", file_heading(file)),
                    Style::default().bold().patch(selection_style),
                )));
                for (anchor, rows) in file_hunks(&file.change) {
                    if let Some(anchor) = anchor {
                        lines.push(Line::from(Span::styled(
                            format!("  @@ {anchor}"),
                            dim.patch(selection_style),
                        )));
                    }
                    for row in rows {
                        if diff_rows == max_diff_rows {
                            lines.push(Line::from(Span::styled(
                                format!("  … {} more lines", total_diff_rows - max_diff_rows),
                                dim.italic().patch(selection_style),
                            )));
                        }
                        diff_rows += 1;
                        if diff_rows > max_diff_rows {
                            continue;
                        }
                        lines.push(diff_row(row, selection_style));
                    }
                }
            }
        }
    }
    lines
}

fn file_heading(file: &FileEdit) -> String {
    match &file.change {
        FileChange::Add { .. } => format!("+ {} (new file)", file.path),
        FileChange::Delete => format!("- {} (deleted)", file.path),
        FileChange::Update {
            moved_to: Some(to), ..
        } => format!("~ {} → {to}", file.path),
        FileChange::Update { .. } => format!("~ {}", file.path),
    }
}

/// A file change's hunks as `(anchor, lines)`; a new file is one hunk.
fn file_hunks(change: &FileChange) -> Vec<(Option<&str>, &[DiffLine])> {
    match change {
        FileChange::Add { lines } => vec![(None, lines.as_slice())],
        FileChange::Delete => Vec::new(),
        FileChange::Update { hunks, .. } => hunks
            .iter()
            .map(|hunk| (hunk.anchor.as_deref(), hunk.lines.as_slice()))
            .collect(),
    }
}

fn diff_row(row: &DiffLine, selection_style: Style) -> Line<'static> {
    let (sign, color) = match row.op {
        DiffOp::Context => (' ', Color::Gray),
        DiffOp::Added => ('+', Color::Green),
        DiffOp::Removed => ('-', Color::Red),
    };
    let number =
        |value: Option<u32>| value.map_or_else(|| "    ".to_string(), |n| format!("{n:>4}"));
    Line::from(vec![
        Span::styled(
            format!("  {} {} ", number(row.old), number(row.new)),
            Style::default().fg(Color::DarkGray).patch(selection_style),
        ),
        Span::styled(
            format!("{sign} {}", row.text),
            Style::default().fg(color).patch(selection_style),
        ),
    ])
}

/// Render full-screen tool detail view.
pub fn render_tool_fullscreen(
    frame: &mut Frame,
    area: Rect,
    display: &ToolDisplay,
    scroll: usize,
    bg: Option<Color>,
) {
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
            location,
            ..
        } => {
            let marker = format!(
                "{}{}",
                terminal_mode_marker(*mode),
                terminal_location_marker(location.as_deref())
            );
            if description.is_empty() {
                format!("{marker}$ {command}")
            } else {
                format!("{description} — {marker}$ {command}")
            }
        }
        ToolDisplay::Action { chip, summary, .. } => {
            let (label, rest) = semantic_headline(chip, summary);
            format!("{label} {rest}").trim_end().to_string()
        }
        ToolDisplay::Builtin { tool, detail, .. } => {
            let (label, rest) = semantic_headline(tool, detail);
            format!("{label} {rest}").trim_end().to_string()
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
            "Detail",
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
                format!("Command {}", i + 1),
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

    // Calculate safe scroll value (account for the title row and bottom padding: 2 lines)
    let inner_height = area.height.saturating_sub(2);
    let scroll_y = safe_scroll(scroll, lines.len(), inner_height);

    let paragraph = super::wrap::WrappedParagraph::new(lines)
        .block(super::band::panel(
            Some(Span::raw("Tool Details")),
            Color::Cyan,
            bg,
        ))
        .scroll(scroll_y);

    frame.render_widget(paragraph, area);
}
#[cfg(test)]
mod tests {
    use super::{Card, RowChangeKind, RowView, SubagentState, card_lines};
    use super::{
        ChatElement, ChatStream, InputContext, ToolCallImage, ToolDisplay, ToolStatus,
        background_summary, build_plain_wrapped_lines_unified, format_cost_usd, format_number,
        format_status_usage, index_bar, render_input_line, render_tool_inline,
        terminal_mode_marker,
    };
    use crate::tui::queue::Attachment;
    use crate::tui::state::CompactionMeter;
    use ratatui::style::Style;

    #[test]
    fn usage_shows_the_wall_clock_split() {
        let report = crate::agent::SessionUsageReport {
            session_id: "s".to_string(),
            api_ms: 4_000,
            wall_ms: 61_000,
            split: crate::agent::WallClockSplit {
                api_ms: 4_000,
                tools_ms: 8_000,
                user_wait_ms: 20_000,
                idle_ms: 29_000,
            },
            total_cost_usd: None,
            models: Vec::new(),
            cache_miss_causes: Vec::new(),
            recent_cache_misses: Vec::new(),
        };
        let rendered = super::usage_report_lines(&report)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            rendered,
            "Wall clock 1m 1s\n  API        0m 4s\n  Waiting    0m 20s\n  Tools      0m 8s\n  Idle       0m 29s"
        );
    }

    #[test]
    fn usage_shows_cache_misses_with_their_causes() {
        use crate::agent::cache_miss::CacheMissCause;
        let miss =
            |cause, missed_tokens, gap_ms, kind: Option<&str>, index| crate::agent::CacheMissRow {
                timestamp: "not a timestamp".to_string(),
                model: "opus".to_string(),
                cause,
                missed_tokens,
                gap_ms,
                diverged_kind: kind.map(str::to_string),
                diverged_index: index,
            };
        let report = crate::agent::SessionUsageReport {
            session_id: "s".to_string(),
            api_ms: 0,
            wall_ms: 0,
            split: crate::agent::WallClockSplit {
                api_ms: 0,
                tools_ms: 0,
                user_wait_ms: 0,
                idle_ms: 0,
            },
            total_cost_usd: None,
            models: vec![crate::agent::ModelUsageRow {
                model: "opus".to_string(),
                requests: 3,
                prompt_tokens: 0,
                completion_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                api_ms: 0,
                cache_hit_rate: 0.0,
                cache_misses: 2,
                cache_miss_tokens: 30_000,
                cost_usd: None,
                keep_warm_requests: 1,
            }],
            cache_miss_causes: vec![
                crate::agent::CacheMissCauseRow {
                    cause: CacheMissCause::Expired,
                    misses: 1,
                    missed_tokens: 20_000,
                },
                crate::agent::CacheMissCauseRow {
                    cause: CacheMissCause::ToolsChanged,
                    misses: 1,
                    missed_tokens: 10_000,
                },
            ],
            recent_cache_misses: vec![
                miss(
                    CacheMissCause::ToolsChanged,
                    10_000,
                    30_000,
                    Some("tools"),
                    None,
                ),
                miss(CacheMissCause::Expired, 20_000, 3_660_000, None, None),
                miss(
                    CacheMissCause::Compaction,
                    5_000,
                    1_000,
                    Some("user"),
                    Some(1),
                ),
            ],
        };
        let rendered = super::usage_report_lines(&report)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert!(
            rendered
                .iter()
                .any(|line| line.ends_with("3 requests (1 keep-warm)")),
            "{rendered:?}"
        );
        let misses = rendered
            .iter()
            .skip_while(|line| !line.starts_with("    cache misses"))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            misses,
            "    cache misses 2  30.0k tokens not read from cache\n\
             \n\
             Cache misses 2  30.0k tokens not read from cache\n  \
             cache lifetime expired  1  20.0k tokens\n  \
             tool set changed  1  10.0k tokens\n\
             Recent cache misses\n  \
             not a timestamp  opus  tool set changed  10.0k tokens  changed at the tool definitions  0m 30s after the previous request\n  \
             not a timestamp  opus  cache lifetime expired  20.0k tokens  1h 1m after the previous request\n  \
             not a timestamp  opus  compaction  5.0k tokens  changed at message 1 (user)  0m 1s after the previous request"
        );
    }

    #[test]
    fn cost_formatting_keeps_sub_cent_precision() {
        assert_eq!(format_cost_usd(0.0), "$0.00");
        assert_eq!(format_cost_usd(1.234), "$1.23");
        assert_eq!(format_cost_usd(0.01), "$0.01");
        assert_eq!(format_cost_usd(0.0099), "$0.0099");
        assert_eq!(format_cost_usd(0.00337), "$0.0034");
        assert_eq!(format_cost_usd(0.000_004_2), "$0.0000042");
    }
    use crate::tui::ui::streaming_markdown::IMAGE_HEIGHT;
    use aither_sandbox::SandboxMode;
    use ratatui::style::Color;
    use ratatui::text::Line;
    use std::path::PathBuf;

    #[test]
    fn status_usage_is_used_over_autocompact_window() {
        let window = 200_000_u64;
        let policy = aither_agent::ContextPolicy::default();
        let compact = policy.pause_point(u32::try_from(window).unwrap());
        let used = compact / 4;
        let line = format_status_usage(u64::try_from(used).unwrap(), Some(window), &policy);
        #[allow(clippy::cast_precision_loss)]
        let percent = used as f64 / compact as f64 * 100.0;
        assert_eq!(
            line.usage,
            format!(
                "{} / {} ({percent:.0}%)",
                format_number(used),
                format_number(compact)
            )
        );
        assert!(line.warn.is_none());
    }

    #[test]
    fn status_usage_warns_within_ten_percent_of_autocompact() {
        let window = 200_000_u64;
        let policy = aither_agent::ContextPolicy::default();
        let compact = policy.pause_point(u32::try_from(window).unwrap());
        let used = compact - compact / 20;
        let line = format_status_usage(u64::try_from(used).unwrap(), Some(window), &policy);
        assert_eq!(line.warn.as_deref(), Some("5% to auto-compact"));
    }

    #[test]
    fn status_usage_omits_warn_at_ten_percent_remaining() {
        let window = 200_000_u64;
        let policy = aither_agent::ContextPolicy::default();
        let compact = policy.pause_point(u32::try_from(window).unwrap());
        let used = compact - compact / 10;
        let line = format_status_usage(u64::try_from(used).unwrap(), Some(window), &policy);
        assert!(line.warn.is_none());
    }

    #[test]
    fn status_usage_warns_at_zero_headroom() {
        let window = 200_000_u64;
        let policy = aither_agent::ContextPolicy::default();
        let compact = policy.pause_point(u32::try_from(window).unwrap());
        let line = format_status_usage(u64::try_from(compact).unwrap(), Some(window), &policy);
        assert_eq!(line.warn.as_deref(), Some("0% to auto-compact"));
    }

    #[test]
    fn status_usage_falls_back_to_used_without_a_window() {
        let line = format_status_usage(33_400, None, &aither_agent::ContextPolicy::default());
        assert_eq!(line.usage, "33.4K");
        assert!(line.warn.is_none());
    }

    fn coverage(with_vector: u64, total: u64) -> crate::client::IndexCoverage {
        crate::client::IndexCoverage {
            phase: repo_search_client::protocol::WirePhase::FirstIndex,
            with_vector,
            total,
        }
    }

    fn bar_text(coverage: Option<&crate::client::IndexCoverage>, failed: bool) -> Option<String> {
        index_bar(coverage, failed, None).map(|spans| {
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
    }

    #[test]
    fn index_bar_shows_fill_and_percent_while_incomplete() {
        let six = coverage(6, 10);
        let zero = coverage(0, 5);
        assert_eq!(
            bar_text(Some(&six), false).as_deref(),
            Some("index ▕████░░░░▏ 60%")
        );
        assert_eq!(
            bar_text(Some(&zero), false).as_deref(),
            Some("index ▕░░░░░░░░▏ 0%")
        );
    }

    #[test]
    fn index_bar_shows_checkmark_at_full_coverage() {
        let full = coverage(10, 10);
        let over = coverage(12, 10);
        assert_eq!(bar_text(Some(&full), false).as_deref(), Some("index ✓"));
        assert_eq!(bar_text(Some(&over), false).as_deref(), Some("index ✓"));
    }

    #[test]
    fn index_bar_is_absent_with_nothing_to_index() {
        let empty = coverage(0, 0);
        assert!(index_bar(Some(&empty), false, None).is_none());
        assert!(index_bar(None, false, None).is_none());
    }

    #[test]
    fn index_bar_spins_until_the_first_snapshot() {
        let loading = index_bar(None, false, Some(1)).expect("loading is visible");
        assert_eq!(
            loading
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            format!("index {}", super::SPINNER_FRAMES[1])
        );
        let failed = index_bar(None, true, Some(1)).expect("a failure is visible");
        assert_eq!(failed[0].content.as_ref(), "index ✗");
    }

    #[test]
    fn index_bar_shows_a_red_cross_when_search_is_down() {
        let full = coverage(10, 10);
        let spans = index_bar(Some(&full), true, None).expect("a failed search is visible");
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "index ✗"
        );
        assert!(spans.iter().all(|span| span.style.fg == Some(Color::Red)));
        assert_eq!(bar_text(None, true).as_deref(), Some("index ✗"));
    }

    #[test]
    fn background_summary_counts_shells_and_subagents() {
        assert_eq!(background_summary(0, 0), None);
        assert_eq!(background_summary(1, 0).as_deref(), Some("1 shell ↓"));
        assert_eq!(background_summary(2, 0).as_deref(), Some("2 shells ↓"));
        assert_eq!(background_summary(0, 1).as_deref(), Some("1 subagent ↓"));
        assert_eq!(
            background_summary(2, 1).as_deref(),
            Some("2 shells, 1 subagent ↓")
        );
    }

    #[test]
    fn live_output_appends_to_running_terminal_call() {
        let mut stream = ChatStream::new();
        stream.push_tool(
            "call-1".to_string(),
            ToolDisplay::Terminal {
                description: String::new(),
                command: "echo hi".to_string(),
                mode: SandboxMode::Sandboxed,
                location: None,
                reading: None,
                card: None,
                status: ToolStatus::Running,
                output: None,
                diffs: Vec::new(),
                pinned: None,
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

    /// A delta-materialized call is replaced in place by its canonical
    /// display — no duplicate element — and a finished call keeps its
    /// terminal display when a trailing event arrives.
    #[test]
    fn upsert_tool_display_refreshes_then_ignores_late_events() {
        fn terminal(command: &str) -> ToolDisplay {
            ToolDisplay::Terminal {
                description: String::new(),
                command: command.to_string(),
                mode: SandboxMode::Sandboxed,
                location: None,
                reading: None,
                card: None,
                status: ToolStatus::Running,
                output: None,
                diffs: Vec::new(),
                pinned: None,
            }
        }

        let mut stream = ChatStream::new();
        stream.upsert_tool_display("call-1".to_string(), terminal("ech"));
        stream.upsert_tool_display("call-1".to_string(), terminal("echo hi"));
        assert_eq!(stream.elements.len(), 1);
        let Some(ChatElement::ToolCall {
            display: ToolDisplay::Terminal { command, .. },
            ..
        }) = stream.elements.first()
        else {
            panic!("tool call missing");
        };
        assert_eq!(command, "echo hi");

        // Once finished, a stale delta/start cannot reopen the card.
        stream.update_tool_with_output("call-1", ToolStatus::Done, Some("done"));
        stream.upsert_tool_display("call-1".to_string(), terminal("ech"));
        let Some(ChatElement::ToolCall {
            display: ToolDisplay::Terminal {
                command, status, ..
            },
            ..
        }) = stream.elements.first()
        else {
            panic!("tool call missing");
        };
        assert_eq!(command, "echo hi");
        assert!(matches!(status, ToolStatus::Done));
    }

    /// A retried response leaves nothing of its broken attempt: its thinking,
    /// text and streamed tool call go, while the finished response before it
    /// stays.
    #[test]
    fn a_retracted_response_keeps_only_the_finished_ones() {
        fn terminal(command: &str) -> ToolDisplay {
            ToolDisplay::Terminal {
                description: String::new(),
                command: command.to_string(),
                mode: SandboxMode::Sandboxed,
                location: None,
                reading: None,
                card: None,
                status: ToolStatus::Running,
                output: None,
                diffs: Vec::new(),
                pinned: None,
            }
        }

        let mut stream = ChatStream::new();
        stream.push_user("go", Vec::new(), 0);
        stream.push_text("first reply");
        stream.upsert_tool_display("call-1".to_string(), terminal("ls"));
        stream.update_tool_with_output("call-1", ToolStatus::Done, Some("a"));
        stream.seal_response();

        stream.push_thinking("考え中 생각 中");
        stream.push_text("half an ans");
        stream.upsert_tool_display("call-2".to_string(), terminal("ec"));
        stream.retract_response();

        let kinds: Vec<_> = stream
            .elements()
            .iter()
            .map(|element| match element {
                ChatElement::UserMessage { .. } => "user",
                ChatElement::AssistantText { .. } => "text",
                ChatElement::ToolCall { id, .. } => id.as_str(),
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["user", "text", "call-1"]);
        assert_eq!(stream.elements().len(), stream.revisions.len());

        // The replacement streams as if the attempt had never been.
        stream.push_text("whole answer");
        assert!(matches!(
            stream.elements().last(),
            Some(ChatElement::AssistantText { content, .. }) if content == "whole answer"
        ));
    }

    /// The user's expansion pin and any attached diffs survive a display
    /// replacement.
    #[test]
    fn upsert_tool_display_carries_pin_and_diffs() {
        let mut stream = ChatStream::new();
        stream.upsert_tool_display(
            "call-1".to_string(),
            ToolDisplay::Terminal {
                description: String::new(),
                command: "ech".to_string(),
                mode: SandboxMode::Sandboxed,
                location: None,
                reading: None,
                card: None,
                status: ToolStatus::Running,
                output: None,
                diffs: Vec::new(),
                pinned: None,
            },
        );
        stream.toggle_expanded("call-1");
        stream.update_tool_diff(
            "call-1",
            vec![crate::file_tracker::RootDiff {
                root: std::path::PathBuf::from("/repo"),
                label: None,
                changes: vec![crate::file_tracker::FileStat {
                    path: std::path::PathBuf::from("a.txt"),
                    additions: 2,
                    deletions: 0,
                }],
                additions: 2,
                deletions: 0,
            }],
        );

        stream.upsert_tool_display(
            "call-1".to_string(),
            ToolDisplay::Terminal {
                description: "greet".to_string(),
                command: "echo hi".to_string(),
                mode: SandboxMode::Sandboxed,
                location: None,
                reading: None,
                card: None,
                status: ToolStatus::Running,
                output: None,
                diffs: Vec::new(),
                pinned: None,
            },
        );

        let Some(ChatElement::ToolCall {
            display:
                ToolDisplay::Terminal {
                    pinned,
                    diffs,
                    description,
                    ..
                },
            ..
        }) = stream.elements.first()
        else {
            panic!("tool call missing");
        };
        assert_eq!(*pinned, Some(false));
        assert_eq!(diffs.len(), 1);
        assert_eq!(description, "greet");
    }

    #[test]
    fn terminal_mode_marker_covers_every_backend() {
        assert_eq!(terminal_mode_marker(SandboxMode::Sandboxed), "");
        // Unsafe carries no text marker — the red status icon marks it.
        assert_eq!(terminal_mode_marker(SandboxMode::Unsandboxed), "");
    }

    #[test]
    fn inline_terminal_renders_ssh_location_marker() {
        for (location, description, expected) in [
            (Some("build-box"), "", "▸ ✓ [ssh build-box] $ uname"),
            (None, "", "▸ ✓ $ uname"),
            (Some("build-box"), "kernel", "▸ ✓ [ssh build-box] kernel"),
            (None, "kernel", "▸ ✓ kernel"),
        ] {
            let display = ToolDisplay::Terminal {
                description: description.to_string(),
                command: "uname".to_string(),
                mode: SandboxMode::Sandboxed,
                location: location.map(str::to_string),
                reading: None,
                card: None,
                status: ToolStatus::Done,
                output: None,
                diffs: Vec::new(),
                pinned: None,
            };
            let mut lines: Vec<Line> = Vec::new();
            let mut positions = Vec::new();
            render_tool_inline(&mut lines, &display, false, true, &mut positions);
            let rendered: String = lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert_eq!(rendered, expected);
        }
    }

    #[test]
    fn inline_terminal_renders_mode_marker() {
        for (mode, marker, icon_fg) in [
            (SandboxMode::Sandboxed, "", Color::Green),
            // Unsafe: no text marker — the status icon turns red instead.
            (SandboxMode::Unsandboxed, "", Color::Red),
        ] {
            let display = ToolDisplay::Terminal {
                description: String::new(),
                command: "echo hi".to_string(),
                mode,
                location: None,
                reading: None,
                card: None,
                status: ToolStatus::Done,
                output: None,
                diffs: Vec::new(),
                pinned: None,
            };
            let mut lines: Vec<Line> = Vec::new();
            let mut positions = Vec::new();
            render_tool_inline(&mut lines, &display, false, true, &mut positions);
            let rendered: String = lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert_eq!(rendered, format!("▸ ✓ {marker}$ echo hi"));
            assert_eq!(lines[0].spans[1].style.fg, Some(icon_fg));
        }
    }

    /// A call that finished — the automatic rule renders it collapsed.
    fn terminal_display(command: &str) -> ToolDisplay {
        ToolDisplay::Terminal {
            description: String::new(),
            command: command.to_string(),
            mode: SandboxMode::Sandboxed,
            location: None,
            reading: None,
            card: None,
            status: ToolStatus::Done,
            output: None,
            diffs: Vec::new(),
            pinned: None,
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
                mode: SandboxMode::Sandboxed,
                location: None,
                reading: None,
                card: None,
                status: ToolStatus::Done,
                output: Some(
                    super::TerminalCallInfo::new("terminal".to_string()).with_output(
                        "tick 1 at 21:20:24\ntick 2 at 21:20:25\ntick 3 at 21:20:26".to_string(),
                    ),
                ),
                diffs: Vec::new(),
                pinned: None,
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

    /// Render `chat` the way the TUI does (input + status tail) into
    /// `terminal`, reusing `layout`; returns the rendered frame buffer and
    /// asserts the backend (what a real terminal shows after diffing)
    /// matches it.
    fn draw_unified(
        terminal: &mut ratatui::Terminal<crate::tui::ui::test_screen::ScreenBackend>,
        chat: &ChatStream,
        layout: &mut super::ChatLayout,
        scroll: usize,
    ) -> ratatui::buffer::Buffer {
        use super::{StatusContext, UnifiedRenderContext, render_unified};
        let mut truth = None;
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_unified(
                    frame,
                    area,
                    chat,
                    layout,
                    scroll,
                    UnifiedRenderContext {
                        selected_tool: None,
                        content_height_out: None,
                        view_height_out: None,
                        input_ctx: Some(InputContext {
                            input: "draft",
                            cursor: 5,
                            attachments: &[],
                            suggestion: None,
                            greeting: None,
                        }),
                        input_caret_active: true,
                        queue_ctx: None,
                        status_ctx: Some(StatusContext {
                            model: "model",
                            usage: None,
                            compaction: CompactionMeter::Idle,
                            context_policy: aither_agent::ContextPolicy::default(),
                            queue_count: 0,
                            open_questions: 0,
                            yolo: false,
                            shells: 0,
                            subagents: 0,
                            decode_rate: None,
                            status_message: None,
                            cwd: "~",
                            activity: None,
                            spinner_frame: 0,
                            index_coverage: None,
                            index_failed: false,
                            index_loading: false,
                        }),
                        images_supported: true,
                        hide_todos: false,
                        image_positions_out: None,
                        spinner_frame: 0,
                        band_bg: None,
                    },
                );
                truth = Some(frame.buffer_mut().clone());
            })
            .unwrap();
        let truth = truth.expect("frame drawn");
        assert_eq!(
            terminal.backend().screen(),
            crate::tui::ui::test_screen::buffer_screen(&truth),
            "the terminal shows something other than the rendered frame"
        );
        assert_eq!(
            terminal.backend().backgrounds(),
            crate::tui::ui::test_screen::buffer_backgrounds(&truth),
            "the terminal paints backgrounds other than the rendered frame's"
        );
        truth
    }

    const STREAMED_REPLY: &str = "Here is the plan:\n\n## Steps\n\n1. **Read** the `config.rs` file and check the provider list carefully before editing anything\n2. 修改终端渲染逻辑，确保宽字符换行正确，并且不会出现重复的文本行\n3. Run the tests\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\nDone — that's all for now, with a long closing sentence that wraps across several rows of the view.";

    /// Streaming a reply in small chunks (with a frame after each) ends on
    /// exactly the screen a one-shot render of the finished reply shows:
    /// no row of an earlier, shorter wrap survives.
    #[test]
    fn streaming_in_chunks_matches_one_shot_render() {
        use crate::tui::ui::test_screen::ScreenBackend as TestBackend;
        use ratatui::Terminal;

        let mut streamed = ChatStream::new();
        streamed.push_user("plan it", Vec::new(), 0);
        let mut terminal = Terminal::new(TestBackend::new(48, 30)).unwrap();
        let mut layout = super::ChatLayout::default();
        let mut rest = STREAMED_REPLY;
        while !rest.is_empty() {
            // Chunk on char boundaries, 7 chars at a time.
            let split = rest.char_indices().nth(7).map_or(rest.len(), |(i, _)| i);
            let (chunk, tail) = rest.split_at(split);
            streamed.push_text(chunk);
            draw_unified(&mut terminal, &streamed, &mut layout, usize::MAX);
            rest = tail;
        }
        streamed.end_text();
        let streamed_screen = draw_unified(&mut terminal, &streamed, &mut layout, usize::MAX);

        let mut whole = ChatStream::new();
        whole.push_user("plan it", Vec::new(), 0);
        whole.push_text(STREAMED_REPLY);
        whole.end_text();
        let mut fresh = Terminal::new(TestBackend::new(48, 30)).unwrap();
        let mut fresh_layout = super::ChatLayout::default();
        let whole_screen = draw_unified(&mut fresh, &whole, &mut fresh_layout, usize::MAX);

        assert_eq!(streamed_screen, whole_screen);
    }

    /// Cells of `buf` with a background, as `(x, y)`.
    fn cells_with_background(buf: &ratatui::buffer::Buffer) -> Vec<(u16, u16)> {
        let area = buf.area;
        let mut found = Vec::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if buf[(x, y)].bg != ratatui::style::Color::Reset {
                    found.push((x, y));
                }
            }
        }
        found
    }

    const CODE_REPLY: &str = "## 改动说明\n\n- 新增 `ContextBudget` 并在每次 `message_delta` 到达时调用 `calibrate`，路径是 `/Users/lexoliu/Coding/aither`，这一行足够长以便换行。\n- 另一项 `calibrate` 之后接中文，再接 English words 和 `a b` 以及更多的内容让它再换一行\n\n结尾段落包含 `message_delta`。";

    /// A reply of CJK and Latin prose with wrapped inline code, streamed
    /// in, resized and scrolled: every frame's screen, backgrounds
    /// included, is what the frame rendered, and no cell carries a
    /// background.
    #[test]
    fn inline_code_reply_leaves_no_stray_cells_across_frames() {
        use crate::tui::ui::test_screen::ScreenBackend as TestBackend;
        use ratatui::Terminal;

        for width in [30u16, 41, 48, 57, 80] {
            let mut chat = ChatStream::new();
            chat.push_user("说明", Vec::new(), 0);
            let mut terminal = Terminal::new(TestBackend::new(width, 14)).unwrap();
            let mut layout = super::ChatLayout::default();
            let mut rest = CODE_REPLY;
            while !rest.is_empty() {
                let split = rest.char_indices().nth(5).map_or(rest.len(), |(i, _)| i);
                let (chunk, tail) = rest.split_at(split);
                chat.push_text(chunk);
                let frame = draw_unified(&mut terminal, &chat, &mut layout, usize::MAX);
                let stray = cells_with_background(&frame);
                assert!(stray.is_empty(), "w={width} after {chunk:?}: {stray:?}");
                rest = tail;
            }
            chat.end_text();
            for scroll in [0, 3, 1, usize::MAX, 2] {
                let frame = draw_unified(&mut terminal, &chat, &mut layout, scroll);
                assert!(
                    cells_with_background(&frame).is_empty(),
                    "w={width} scroll={scroll}"
                );
            }
            for resized in [width + 7, width.saturating_sub(9).max(12), width] {
                terminal.backend_mut().resize(resized, 14);
                terminal
                    .resize(ratatui::layout::Rect::new(0, 0, resized, 14))
                    .unwrap();
                let frame = draw_unified(&mut terminal, &chat, &mut layout, usize::MAX);
                assert!(
                    cells_with_background(&frame).is_empty(),
                    "w={width} resized={resized}"
                );
            }
        }
    }

    /// A three-column table of CJK cells and a long inline code span,
    /// wider than the chat column, then a fenced block and a paragraph.
    const TABLE_REPLY: &str = "对比如下：\n\n| 项目 | 说明 | 命令 |\n|---|---|---|\n| 构建 | 编译整个工作区并运行所有单元测试，确认没有回归 | `cargo test --workspace --all-features --locked` |\n| 格式化 | 检查代码风格 | `cargo fmt --all -- --check` |\n\n```sh\ncargo test --workspace\n```\n\n以上命令都需要在仓库根目录运行，结果会写入目标目录。";

    /// Display columns of the table border glyphs on a screen row.
    fn screen_border_columns(row: &str) -> Vec<usize> {
        let mut columns = Vec::new();
        let mut x = 0;
        for ch in row.chars() {
            if matches!(ch, '│' | '├' | '┼' | '┤') {
                columns.push(x);
            }
            x += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        }
        columns
    }

    /// A table wider than the chat column is laid out inside it: streamed
    /// or whole, every table row on screen has its borders at the
    /// header's columns, one rule is drawn, and no cell text spills onto
    /// a row outside the table.
    /// A withdrawn turn takes its user bubble and everything the turn
    /// showed after it; earlier prompts stay.
    #[test]
    fn withdrawing_a_prompt_removes_its_bubble_and_markers() {
        let mut chat = ChatStream::new();
        chat.push_user("earlier", Vec::new(), 0);
        chat.push_text("answer");
        chat.end_text();
        chat.push_user("你好 こんにちは 안녕하세요", Vec::new(), 2);
        chat.push_text("\n\n[Interrupted]");
        chat.end_text();

        assert!(chat.withdraw_last_prompt("never sent").is_none());
        let attachments = chat.withdraw_last_prompt("你好 こんにちは 안녕하세요");
        assert_eq!(attachments.map(|a| a.len()), Some(0));
        let targets = chat.rewind_targets();
        assert_eq!(targets.len(), 1, "only the earlier prompt remains");
        assert_eq!(targets[0].content, "earlier");
        assert_eq!(chat.elements.len(), 2, "the earlier prompt and its answer");
    }

    #[test]
    fn wide_table_reply_stays_aligned_on_screen() {
        use crate::tui::ui::test_screen::ScreenBackend as TestBackend;
        use ratatui::Terminal;

        let width = 48;
        let mut chat = ChatStream::new();
        chat.push_user("对比", Vec::new(), 0);
        let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
        let mut layout = super::ChatLayout::default();
        let mut rest = TABLE_REPLY;
        while !rest.is_empty() {
            let split = rest.char_indices().nth(9).map_or(rest.len(), |(i, _)| i);
            let (chunk, tail) = rest.split_at(split);
            chat.push_text(chunk);
            draw_unified(&mut terminal, &chat, &mut layout, usize::MAX);
            rest = tail;
        }
        chat.end_text();
        let screen = draw_unified(&mut terminal, &chat, &mut layout, usize::MAX);
        // Drop the horizontal padding.
        let rows: Vec<String> = crate::tui::ui::test_screen::buffer_screen(&screen)
            .into_iter()
            .map(|row| row.chars().skip(2).collect())
            .collect();
        let first = rows
            .iter()
            .position(|r| r.starts_with('│'))
            .expect("table on screen");
        let last = rows
            .iter()
            .rposition(|r| r.starts_with('│'))
            .expect("table on screen");
        let header = screen_border_columns(&rows[first]);
        assert_eq!(header.len(), 4, "{rows:#?}");
        for row in &rows[first..=last] {
            assert_eq!(screen_border_columns(row), header, "{rows:#?}");
        }
        assert_eq!(
            rows.iter().filter(|r| r.starts_with('├')).count(),
            1,
            "{rows:#?}"
        );
        // The rows around the table are the text before it and a blank.
        assert_eq!(rows[first - 2], "对比如下：");
        assert_eq!(rows[last + 1], "");
    }

    /// A reply holding a table wider than the chat column and a fenced
    /// block, streamed with a frame per chunk and followed by more frames,
    /// leaves no stray cell: no frame puts a wide glyph half outside the
    /// column (which the diff would follow by skipping the next cell), the
    /// terminal shows each frame exactly (`draw_unified` checks), and the
    /// final screen equals a fresh render of the final state.
    #[test]
    fn wide_table_reply_leaves_no_stray_cells() {
        use crate::tui::ui::test_screen::ScreenBackend as TestBackend;
        use ratatui::Terminal;

        let height = 24;
        let draw = |terminal: &mut Terminal<TestBackend>,
                    chat: &ChatStream,
                    layout: &mut super::ChatLayout,
                    scroll: usize| {
            let frame = draw_unified(terminal, chat, layout, scroll);
            let last = frame.area.right() - 1;
            for y in frame.area.top()..frame.area.bottom() {
                let symbol = frame[(last, y)].symbol();
                assert!(
                    unicode_width::UnicodeWidthStr::width(symbol) < 2,
                    "wide glyph {symbol:?} overflows the last column on row {y}"
                );
            }
        };
        for width in (20u16..=56).step_by(4) {
            for chunk in [1usize, 6] {
                let mut chat = ChatStream::new();
                chat.push_user("对比", Vec::new(), 0);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut layout = super::ChatLayout::default();
                let mut rest = TABLE_REPLY;
                while !rest.is_empty() {
                    let split = rest
                        .char_indices()
                        .nth(chunk)
                        .map_or(rest.len(), |(i, _)| i);
                    let (text, tail) = rest.split_at(split);
                    chat.push_text(text);
                    draw(&mut terminal, &chat, &mut layout, usize::MAX);
                    rest = tail;
                }
                chat.end_text();
                chat.push_user("再说一遍", Vec::new(), 1);
                chat.push_text("好的。\n\n第二段说明文字，稍微长一些，会在窄窗口里换行。");
                chat.end_text();
                for scroll in [0, 3, 11, usize::MAX] {
                    draw(&mut terminal, &chat, &mut layout, scroll);
                }
                let shown = terminal.backend().screen();

                let mut fresh = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut fresh_layout = super::ChatLayout::default();
                draw_unified(&mut fresh, &chat, &mut fresh_layout, usize::MAX);
                assert_eq!(
                    shown,
                    fresh.backend().screen(),
                    "width={width} chunk={chunk}"
                );
            }
        }
    }

    /// A width change re-wraps every element: the narrower screen equals a
    /// fresh render at that width, with no row left from the wider wrap.
    #[test]
    fn resize_rewraps_without_duplicates() {
        use crate::tui::ui::test_screen::ScreenBackend as TestBackend;
        use ratatui::{Terminal, layout::Rect};

        let mut chat = ChatStream::new();
        chat.push_user("plan it", Vec::new(), 0);
        chat.push_text(STREAMED_REPLY);
        chat.end_text();

        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        let mut layout = super::ChatLayout::default();
        draw_unified(&mut terminal, &chat, &mut layout, usize::MAX);
        terminal.backend_mut().resize(40, 30);
        terminal.resize(Rect::new(0, 0, 40, 30)).unwrap();
        let resized = draw_unified(&mut terminal, &chat, &mut layout, usize::MAX);

        let mut fresh = Terminal::new(TestBackend::new(40, 30)).unwrap();
        let mut fresh_layout = super::ChatLayout::default();
        let expected = draw_unified(&mut fresh, &chat, &mut fresh_layout, usize::MAX);
        assert_eq!(resized, expected);
    }

    /// Scrolling re-lays-out nothing: only the visible window is rendered
    /// from the cache. A streamed chunk re-lays-out only the tail element.
    #[test]
    fn scroll_and_stream_relayout_only_what_changed() {
        use crate::tui::ui::test_screen::ScreenBackend as TestBackend;
        use ratatui::Terminal;

        let mut chat = ChatStream::new();
        for index in 0..40 {
            chat.push_user(&format!("question {index}"), Vec::new(), index);
            chat.push_text(STREAMED_REPLY);
            chat.end_text();
        }
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let mut layout = super::ChatLayout::default();
        draw_unified(&mut terminal, &chat, &mut layout, usize::MAX);
        assert_eq!(layout.rebuilt(), 80);

        for scroll in [0, 7, 300, 301, 302, 900] {
            draw_unified(&mut terminal, &chat, &mut layout, scroll);
            assert_eq!(layout.rebuilt(), 0, "scroll={scroll} re-laid-out elements");
        }

        chat.push_text("more");
        draw_unified(&mut terminal, &chat, &mut layout, usize::MAX);
        assert_eq!(layout.rebuilt(), 1);
        chat.push_text(" text");
        draw_unified(&mut terminal, &chat, &mut layout, usize::MAX);
        assert_eq!(layout.rebuilt(), 1);
    }

    /// Every scroll offset of a long transcript shows exactly the rows a
    /// whole-document render shows at that offset.
    #[test]
    fn windowed_render_matches_full_document() {
        use crate::tui::ui::test_screen::ScreenBackend as TestBackend;
        use ratatui::{
            Terminal,
            buffer::Buffer,
            layout::Rect,
            widgets::{Paragraph, Widget},
        };

        let mut chat = ChatStream::new();
        for index in 0..6 {
            chat.push_user(&format!("question {index}"), Vec::new(), index);
            chat.push_text(STREAMED_REPLY);
            chat.end_text();
        }
        let (width, height) = (44u16, 12u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut layout = super::ChatLayout::default();
        draw_unified(&mut terminal, &chat, &mut layout, 0);

        // The plain mirror is the whole document, row for row.
        let rows = layout.plain_len();
        let doc: Vec<String> = (0..rows)
            .map(|row| layout.plain_line(row).unwrap().to_string())
            .collect();
        for scroll in [0, 1, 5, 33, 64, rows - usize::from(height)] {
            let screen = draw_unified(&mut terminal, &chat, &mut layout, scroll);
            for y in 0..height {
                let mut text = String::new();
                // The chat sits inside the horizontal padding.
                let pad = super::band::padding(width);
                let mut x = pad;
                while x < width - pad {
                    let symbol = screen[(x, y)].symbol();
                    text.push_str(symbol);
                    x += u16::try_from(unicode_width::UnicodeWidthStr::width(symbol).max(1))
                        .unwrap();
                }
                assert_eq!(
                    text.trim_end(),
                    doc[scroll + usize::from(y)],
                    "scroll={scroll} row={y}"
                );
            }
        }

        // And the mirror itself matches the whole document wrapped at once.
        let inner = width - 2 * super::band::padding(width);
        let mut lines = Vec::new();
        for (index, elem) in chat.active_elements().iter().enumerate() {
            let _ = index;
            lines.extend(
                super::element_lines(
                    elem,
                    super::ElementState::of(elem, None, false, 0),
                    true,
                    "http://localhost:8787/tunnel",
                    inner,
                )
                .lines,
            );
        }
        let element_rows = layout.element_rows();
        let area = Rect::new(0, 0, inner, u16::try_from(element_rows).unwrap());
        let mut buf = Buffer::empty(area);
        Paragraph::new(crate::tui::ui::wrap::wrap_lines(&lines, inner)).render(area, &mut buf);
        for (row, expected) in doc.iter().enumerate().take(element_rows) {
            let y = u16::try_from(row).unwrap();
            let mut text = String::new();
            let mut x = 0;
            while x < inner {
                let symbol = buf[(x, y)].symbol();
                text.push_str(symbol);
                x += u16::try_from(unicode_width::UnicodeWidthStr::width(symbol).max(1)).unwrap();
            }
            assert_eq!(&text.trim_end().to_string(), expected, "row={row}");
        }
    }

    /// An image at the very end of a reply keeps its reserved rows: the
    /// overlay block must not reach into the next element.
    #[test]
    fn trailing_image_keeps_its_reserved_rows() {
        let mut chat = ChatStream::new();
        chat.push_text("Here it is:\n\n![chart](https://example.com/chart.png)\n");
        chat.end_text();
        chat.push_user("thanks", Vec::new(), 1);

        let mut layout = super::ChatLayout::default();
        layout.sync(&chat, 60, true, false, None, 0);
        let images = layout.images_in(0, 1000);
        assert_eq!(images.len(), 1);
        let block_end = images[0].row + IMAGE_HEIGHT;
        let user_row = (0..layout.element_rows())
            .find(|&row| layout.plain_line(row) == Some("> thanks"))
            .expect("user message row");
        assert!(
            block_end <= user_row,
            "image block rows {}..{block_end} overlap the next message at {user_row}",
            images[0].row
        );
        for row in images[0].row..block_end {
            assert_eq!(
                layout.plain_line(row),
                Some(""),
                "reserved row {row} not blank"
            );
        }
    }

    /// A running terminal call carrying the reading of `script`.
    fn read_call(script: &str) -> ToolDisplay {
        ToolDisplay::Terminal {
            description: "work".to_string(),
            command: script.to_string(),
            mode: SandboxMode::Sandboxed,
            location: None,
            reading: Some(crate::command_reading::CommandReading::of(script)),
            card: None,
            status: ToolStatus::Running,
            output: None,
            diffs: Vec::new(),
            pinned: None,
        }
    }

    fn call_rows(display: &ToolDisplay) -> Vec<String> {
        let mut lines: Vec<Line> = Vec::new();
        let mut positions = Vec::new();
        render_tool_inline(&mut lines, display, false, true, &mut positions);
        rendered_text(&lines)
    }

    #[test]
    fn an_edit_renders_as_a_numbered_diff_before_it_runs() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir(dir.path().join("src")).expect("src");
        std::fs::write(
            dir.path().join("src/a.rs"),
            "fn main() {\n    keep\n    old line\n}\n",
        )
        .expect("file");
        let script = "apply_patch <<'P'\n*** Begin Patch\n*** Update File: src/a.rs\n@@ fn main() {\n     keep\n-    old line\n+    new line\n*** Add File: b.txt\n+one\n*** End Patch\nP\n";
        let mut display = read_call(script);
        if let ToolDisplay::Terminal { reading, .. } = &mut display {
            *reading = Some(crate::command_reading::CommandReading::of_in(
                script,
                dir.path(),
            ));
        }
        let rows = call_rows(&display);
        assert_eq!(rows[0], "▾ ⠋ work");
        assert_eq!(rows[1], "Edit 2 files");
        assert_eq!(rows[2], "  ~ src/a.rs");
        assert_eq!(rows[3], "  @@ fn main() {");
        assert_eq!(rows[4], "     2    2       keep");
        assert_eq!(rows[5], "     3      -     old line");
        assert_eq!(rows[6], "          3 +     new line");
        assert_eq!(rows[7], "  + b.txt (new file)");
        assert_eq!(rows[8], "          1 + one");
        assert!(
            rows.iter().all(|row| !row.contains("$ apply_patch")),
            "the raw script is not the body of a recognised call"
        );
    }

    #[test]
    fn a_read_and_a_search_render_as_one_line_each() {
        assert_eq!(
            call_rows(&read_call("cat src/lib.rs"))[1],
            "Read src/lib.rs"
        );
        assert_eq!(
            call_rows(&read_call("rg -n \"fn main\" src"))[1],
            "Search for \"fn main\" in src"
        );
    }

    #[test]
    fn a_pipeline_shows_its_parts_and_an_unknown_part_as_its_command() {
        let rows = call_rows(&read_call("rg -n foo src | wc -l && cargo test"));
        assert_eq!(rows[1], "Search for \"foo\" in src");
        assert_eq!(rows[2], "| $ wc -l");
        assert_eq!(rows[3], "&& Run cargo test");
    }

    fn with_card(script: &str, card: Option<Card>) -> ToolDisplay {
        let mut display = read_call(script);
        if let ToolDisplay::Terminal { card: slot, .. } = &mut display {
            *slot = card.map(Box::new);
        }
        display
    }

    fn row(section: &str, title: &str) -> RowView {
        RowView {
            section: section.to_owned(),
            title: title.to_owned(),
            detail: String::new(),
        }
    }

    #[test]
    fn a_tasks_call_renders_the_row_it_changed() {
        let card = Card::Tasks {
            changes: vec![crate::command_reading::RowChange {
                change: RowChangeKind::Started,
                before: Some(row("Queued", "write docs")),
                after: Some(row("Doing", "write docs")),
            }],
        };
        let rows = call_rows(&with_card("tasks start 2", Some(card)));
        assert_eq!(rows[1], "  tasks started");
        assert_eq!(rows[2], "  - write docs (Queued)");
        assert_eq!(rows[3], "  + write docs (Doing)");
    }

    fn subagent(state: SubagentState, result: Option<&str>) -> Card {
        let started_at = jiff::Timestamp::from_second(1_000).expect("time");
        Card::Subagent {
            agent: "explorer".to_owned(),
            title: "Map the parser".to_owned(),
            state,
            started_at,
            finished_at: matches!(state, SubagentState::Done)
                .then(|| jiff::Timestamp::from_second(1_063).expect("time")),
            result: result.map(str::to_owned),
        }
    }

    #[test]
    fn a_finished_subagent_shows_its_summary_and_expands_to_the_result() {
        let card = subagent(
            SubagentState::Done,
            Some("Found two parsers.\nThe old one is dead."),
        );
        let folded = call_rows(&finished(with_card(
            "subagent explorer brief.md",
            Some(card.clone()),
        )));
        assert_eq!(folded[1], "  explorer · Map the parser · done in 1m 03s");
        assert_eq!(folded[2], "  Found two parsers.…");
        let open: Vec<String> = rendered_text(&card_lines(&card, Style::default(), true));
        assert_eq!(open[1], "  Found two parsers.");
        assert_eq!(open[2], "  The old one is dead.");
    }

    #[test]
    fn an_interrupted_subagent_shows_that_it_never_returned() {
        let rows = rendered_text(&card_lines(
            &subagent(SubagentState::Interrupted, None),
            Style::default(),
            false,
        ));
        assert_eq!(rows, ["  explorer · Map the parser · interrupted"]);
    }

    #[test]
    fn a_running_subagent_shows_that_it_runs() {
        let card = subagent(SubagentState::Running, None);
        let rows = rendered_text(&card_lines(&card, Style::default(), false));
        assert!(
            rows[0].starts_with("  explorer · Map the parser · running "),
            "{rows:?}"
        );
        assert_eq!(rows.len(), 1);
    }

    fn question(answers: Option<Vec<crate::command_reading::AnswerCard>>) -> Card {
        Card::AskUser {
            request_id: "r1".to_owned(),
            questions: vec![crate::command_reading::AskedCard {
                section: "db".to_owned(),
                question: "Which database?".to_owned(),
                options: vec!["PostgreSQL".to_owned(), "SQLite".to_owned()],
                multi_select: false,
                secret: false,
            }],
            answers,
        }
    }

    #[test]
    fn an_ask_user_call_shows_the_question_and_then_the_answer() {
        let waiting = call_rows(&with_card(
            "ask_user \"Which database?\"",
            Some(question(None)),
        ));
        assert_eq!(waiting[1], "  ? Which database?");
        assert_eq!(waiting[2], "    · PostgreSQL");
        assert_eq!(waiting[3], "    · SQLite");
        assert_eq!(waiting[4], "  waiting for an answer");
        let answered = call_rows(&with_card(
            "ask_user \"Which database?\"",
            Some(question(Some(vec![crate::command_reading::AnswerCard {
                section: "db".to_owned(),
                text: "SQLite".to_owned(),
            }]))),
        ));
        assert_eq!(answered[4], "  ✓ SQLite");
    }

    #[test]
    fn a_call_without_a_card_renders_as_the_ordinary_command_row() {
        let rows = call_rows(&with_card("tasks start 9", None));
        assert_eq!(rows[1], "Update tasks (start)", "{rows:?}");
        let unknown = call_rows(&with_card("tasks frobnicate", None));
        assert!(
            unknown.iter().any(|row| row.contains("$ tasks frobnicate")),
            "{unknown:?}"
        );
        assert!(rows.iter().all(|row| !row.contains("tasks started")));
    }

    fn finished(mut display: ToolDisplay) -> ToolDisplay {
        if let ToolDisplay::Terminal { status, .. } = &mut display {
            *status = ToolStatus::Done;
        }
        display
    }

    #[test]
    fn a_finished_edit_keeps_a_capped_diff_preview_when_collapsed() {
        let body = "+line\n".repeat(12);
        let script = format!(
            "apply_patch <<'P'\n*** Begin Patch\n*** Add File: big.txt\n{body}*** End Patch\nP\n"
        );
        let rows = call_rows(&finished(read_call(&script)));
        assert!(rows[0].ends_with("work"), "{rows:?}");
        assert_eq!(rows[1], "  + big.txt (new file)");
        let diff = rows.iter().filter(|row| row.contains("+ line")).count();
        assert_eq!(diff, super::DIFF_PREVIEW_LINES);
        assert_eq!(rows.last().unwrap(), "  … 4 more lines");
    }

    #[test]
    fn a_finished_read_folds_to_its_headline() {
        let rows = call_rows(&finished(read_call("cat src/lib.rs")));
        assert_eq!(rows.len(), 1, "{rows:?}");
    }

    #[test]
    fn the_daemons_reading_attaches_to_the_call_it_names() {
        let mut stream = ChatStream::new();
        let mut display = read_call("cat a.rs");
        if let ToolDisplay::Terminal { reading, .. } = &mut display {
            *reading = None;
        }
        stream.push_tool("c1".to_string(), display);
        stream.set_tool_reading("c1", crate::command_reading::CommandReading::of("cat a.rs"));
        let Some(ChatElement::ToolCall {
            display: ToolDisplay::Terminal { reading, .. },
            ..
        }) = stream.elements.first()
        else {
            panic!("terminal call expected");
        };
        assert!(reading.is_some());
    }

    #[test]
    fn a_call_with_no_recognised_part_looks_as_it_did() {
        let rows = call_rows(&read_call("echo hi | wc -c"));
        assert_eq!(rows[1], "$ echo hi | wc -c");
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
        };
        assert!(!bare.expandable());
        let builtin = ToolDisplay::Builtin {
            tool: "web_search".to_string(),
            detail: "q".to_string(),
            status: ToolStatus::Done,
            output: None,
            pinned: None,
        };
        assert!(!builtin.expandable());
    }

    #[test]
    fn hosted_web_search_headlines_as_searched() {
        let display = ToolDisplay::Builtin {
            tool: "web_search".to_string(),
            detail: "may agent".to_string(),
            status: ToolStatus::Done,
            output: None,
            pinned: None,
        };
        let text = {
            let mut lines: Vec<Line> = Vec::new();
            let mut positions = Vec::new();
            render_tool_inline(&mut lines, &display, false, true, &mut positions);
            rendered_text(&lines)
        };
        assert!(
            text.iter()
                .any(|line| line.contains("Searched \"may agent\"")),
            "{text:?}"
        );

        let gemini = ToolDisplay::Builtin {
            tool: "google_search".to_string(),
            detail: "rust async".to_string(),
            status: ToolStatus::Running,
            output: None,
            pinned: None,
        };
        let text = {
            let mut lines: Vec<Line> = Vec::new();
            let mut positions = Vec::new();
            render_tool_inline(&mut lines, &gemini, false, true, &mut positions);
            rendered_text(&lines)
        };
        assert!(text[0].contains("Searched \"rust async\""), "{text:?}");
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
        assert_eq!(text, vec!["▸ ✓ $ one".to_string()]);

        // A click pins it open, showing the whole command.
        let ToolDisplay::Terminal { pinned, .. } = &mut display else {
            unreachable!()
        };
        *pinned = Some(true);
        let text = {
            let mut lines: Vec<Line> = Vec::new();
            let mut positions = Vec::new();
            render_tool_inline(&mut lines, &display, false, true, &mut positions);
            rendered_text(&lines)
        };
        assert_eq!(text.len(), 6); // headline + 5 command lines
        assert_eq!(text[5], "│ five");

        // A second click pins it shut.
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
        assert_eq!(text, vec!["▸ ✓ $ one".to_string()]);
    }

    #[test]
    fn headline_shows_diff_badge() {
        let mut display = terminal_display("echo hi");
        {
            let ToolDisplay::Terminal { diffs, .. } = &mut display else {
                unreachable!()
            };
            *diffs = vec![crate::file_tracker::RootDiff {
                root: std::path::PathBuf::from("/tmp/root"),
                label: None,
                changes: vec![crate::file_tracker::FileStat {
                    path: std::path::PathBuf::from("a.txt"),
                    additions: 12,
                    deletions: 0,
                }],
                additions: 12,
                deletions: 0,
            }];
        }

        // Collapsed: the headline carries the badge; no patch lines
        // render anywhere.
        let mut lines: Vec<Line> = Vec::new();
        let mut positions = Vec::new();
        render_tool_inline(&mut lines, &display, false, true, &mut positions);
        let text = rendered_text(&lines);
        assert_eq!(text, vec!["▸ ✓ $ echo hi  ± 1 file (+12 −0)".to_string()]);

        // Expanded: command body, still no diff block.
        let ToolDisplay::Terminal { pinned, .. } = &mut display else {
            unreachable!()
        };
        *pinned = Some(true);
        let mut lines: Vec<Line> = Vec::new();
        let mut positions = Vec::new();
        render_tool_inline(&mut lines, &display, false, true, &mut positions);
        let text = rendered_text(&lines);
        assert_eq!(text[0], "▾ ✓ $ echo hi  ± 1 file (+12 −0)");
        assert!(text.iter().all(|line| !line.contains("+line")));
    }

    /// The headline leads with the call's status: spinner while running,
    /// tick once done, cross on failure.
    #[test]
    fn headline_icon_follows_status() {
        use ratatui::style::Color;

        for (status, icon, color) in [
            (ToolStatus::Running, '⠋', Color::DarkGray),
            (ToolStatus::Done, '✓', Color::Green),
            (ToolStatus::Failed("exit 2".to_string()), '✗', Color::Red),
        ] {
            let mut display = terminal_display("echo hi");
            let ToolDisplay::Terminal {
                status: slot,
                pinned,
                ..
            } = &mut display
            else {
                unreachable!()
            };
            *slot = status;
            *pinned = Some(false); // collapsed, so only the headline renders
            let mut lines: Vec<Line> = Vec::new();
            let mut positions = Vec::new();
            render_tool_inline(&mut lines, &display, false, true, &mut positions);
            assert_eq!(lines.len(), 1);
            let icon_span = &lines[0].spans[1];
            assert_eq!(icon_span.content, format!("{icon} "));
            assert_eq!(icon_span.style.fg, Some(color));
        }
    }

    /// A command's `$` body is highlighted: at least one span carries a
    /// colored foreground rather than the plain default.
    #[test]
    fn expanded_command_is_highlighted() {
        let mut display = terminal_display("echo hi | grep h");
        let ToolDisplay::Terminal { pinned, .. } = &mut display else {
            unreachable!()
        };
        *pinned = Some(true);
        let mut lines: Vec<Line> = Vec::new();
        let mut positions = Vec::new();
        render_tool_inline(&mut lines, &display, false, true, &mut positions);
        let colored = lines[1]
            .spans
            .iter()
            .any(|span| matches!(span.style.fg, Some(ratatui::style::Color::Rgb(..))));
        assert!(colored, "command line has no syntax colors: {:?}", lines[1]);
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
        assert_eq!(supported.len(), 3 + IMAGE_HEIGHT); // padding + "> look" + header + blanks + trailing ""

        let unsupported =
            build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert_eq!(unsupported.len(), 4); // padding + "> look" + "[image]" + trailing ""
        assert_eq!(unsupported[2], "[image]");
    }

    #[test]
    fn input_preview_rows_appear_above_prompt() {
        let attachments = vec![image_attachment()];
        let ctx = InputContext {
            input: "send it",
            cursor: 7,
            attachments: &attachments,
            suggestion: None,
            greeting: None,
        };
        let mut lines: Vec<Line> = Vec::new();
        let mut positions = Vec::new();
        render_input_line(&mut lines, &ctx, 80, true, Some(&mut positions));

        assert_eq!(lines.len(), IMAGE_HEIGHT + 1);
        assert!(
            lines[..IMAGE_HEIGHT]
                .iter()
                .all(|line| line.spans.iter().all(|span| span.content.is_empty()))
        );
        assert_eq!(positions.len(), 1);
        let prompt = lines.last().expect("input line");
        let text: String = prompt
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "[image] send it");
    }

    #[test]
    fn input_without_image_support_shows_marker_only() {
        let attachments = vec![image_attachment()];
        let ctx = InputContext {
            input: "send it",
            cursor: 7,
            attachments: &attachments,
            suggestion: None,
            greeting: None,
        };
        let mut lines: Vec<Line> = Vec::new();
        render_input_line(&mut lines, &ctx, 80, false, None);

        assert_eq!(lines.len(), 1);
        let rendered: String = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(rendered.contains("[image]"));
    }

    #[test]
    fn suggestion_is_dimmed_ghost_text_after_the_empty_prompt() {
        let ctx = InputContext {
            input: "",
            cursor: 0,
            attachments: &[],
            suggestion: Some("run the tests"),
            greeting: None,
        };
        let mut lines: Vec<Line> = Vec::new();
        render_input_line(&mut lines, &ctx, 80, false, None);
        let ghost = lines
            .last()
            .and_then(|line| line.spans.last())
            .expect("ghost span");
        assert!(ghost.content.starts_with("run the tests"));
        assert_eq!(ghost.style.fg, Some(Color::DarkGray), "dimmed");

        let hidden = InputContext {
            input: "h",
            cursor: 1,
            suggestion: None,
            greeting: None,
            ..ctx
        };
        let mut lines: Vec<Line> = Vec::new();
        render_input_line(&mut lines, &hidden, 80, false, None);
        let rendered: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect();
        assert!(!rendered.contains("run the tests"));
    }

    #[test]
    fn bash_input_shows_mode_hint_and_styled_bang() {
        let ctx = InputContext {
            input: "!ls -la",
            cursor: 7,
            attachments: &[],
            suggestion: None,
            greeting: None,
        };
        let mut lines: Vec<Line> = Vec::new();
        let index = render_input_line(&mut lines, &ctx, 80, false, None);

        assert_eq!(index, 1);
        let hint: String = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(hint.starts_with(" (bash mode) ───"));
        let prompt = &lines[1];
        assert_eq!(prompt.spans[0].content, "! ");
        assert_eq!(prompt.spans[0].style.fg, Some(Color::Yellow));
        assert_eq!(prompt.spans[0].style.bg, None);
        assert_eq!(prompt.spans[1].content, "ls -la");
    }

    #[test]
    fn secret_input_masks_value() {
        let ctx = InputContext {
            input: "/secret KEY hunter2",
            cursor: 19,
            attachments: &[],
            suggestion: None,
            greeting: None,
        };
        let mut lines: Vec<Line> = Vec::new();
        render_input_line(&mut lines, &ctx, 80, false, None);

        let rendered: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect();
        assert!(rendered.contains("/secret KEY "));
        assert!(!rendered.contains("hunter2"));
        assert!(rendered.contains("•••••••"));
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
    fn last_lines_skips_a_trailing_newline() {
        assert_eq!(super::last_lines("a\nb\nc\n", 2), "b\nc");
        assert_eq!(super::last_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(super::last_lines("only", 5), "only");
        assert_eq!(super::last_lines("", 3), "");
    }

    #[test]
    fn live_thinking_window_shows_last_five_lines_then_reopens_full() {
        let mut chat = ChatStream::new();
        chat.push_thinking("l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8");
        let live = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        let body: Vec<&str> = live
            .iter()
            .skip(1)
            .filter(|l| !l.is_empty())
            .map(String::as_str)
            .collect();
        assert_eq!(body, ["l4", "l5", "l6", "l7", "l8"]);
        assert!(live[0].contains("Thinking"));
        assert!(!live.iter().any(|l| l.contains("l1")));

        chat.collapse_thinking();
        chat.toggle_expanded("thinking-0");
        let reopened = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        let full: Vec<&str> = reopened
            .iter()
            .skip(1)
            .filter(|l| !l.is_empty())
            .map(String::as_str)
            .collect();
        assert_eq!(full, ["l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8"]);
    }

    #[test]
    fn a_warning_is_a_warning_icon_with_yellow_text() {
        let mut chat = ChatStream::new();
        chat.push_warning("PR #3: CI / test ran 52m 10s, past the 40m 00s limit.");
        let elem = &chat.elements[0];
        let lines = super::element_lines(
            elem,
            super::ElementState::of(elem, None, false, 0),
            false,
            "",
            80,
        )
        .lines;
        let spans = &lines[0].spans;
        assert_eq!(spans[0].content, "⚠ ");
        assert!(spans.iter().all(|s| s.style.fg == Some(Color::Yellow)));
        assert!(spans[1].content.contains("past the 40m 00s limit"));
    }

    #[test]
    fn a_compaction_recovery_is_a_notice_with_the_cleanup_counts() {
        let mut chat = ChatStream::new();
        chat.push_compaction_recovery(&aither_agent::AgentError::llm_local("no handoff"), 3, 2);
        let [super::ChatElement::Notice { message }] = chat.elements() else {
            panic!("one notice expected: {:?}", chat.elements());
        };
        assert!(message.contains("Compaction failed"), "{message}");
        assert!(message.contains("no handoff"), "{message}");
        assert!(
            message.contains("3 alerts removed, 2 long outputs moved to files"),
            "{message}"
        );
        assert!(message.ends_with("Compacting again."), "{message}");
    }

    #[test]
    fn empty_zero_second_thought_is_dropped() {
        let mut chat = ChatStream::new();
        chat.finish_thinking(0.0);
        assert!(chat.elements().is_empty());

        chat.start_thinking();
        chat.collapse_thinking();
        assert!(chat.elements().is_empty());
    }

    #[test]
    fn premature_reasoning_end_does_not_leave_empty_thought_before_burst() {
        let mut chat = ChatStream::new();
        // Daemon closed a freshly re-armed phase at 0s (TurnComplete after
        // tools), then the next burst streamed real reasoning.
        chat.finish_thinking(0.0);
        chat.push_thinking("step one");
        chat.finish_thinking(9.0);
        let lines = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        let headers: Vec<&str> = lines
            .iter()
            .filter(|l| l.contains("Thought") || l.contains("Thinking"))
            .map(String::as_str)
            .collect();
        assert_eq!(headers, ["◐ Thought for 9s ▸"]);
        assert_eq!(
            chat.elements()
                .iter()
                .filter(|e| matches!(e, ChatElement::Thinking { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn a_translated_reply_shows_the_translation_under_a_marker_and_reveals_the_original() {
        let mut chat = ChatStream::new();
        chat.push_text("All done, the migration passes.");
        chat.set_translation("全部完成，迁移已通过。".to_string(), "cmn");
        chat.end_text();

        let mut regions = Vec::new();
        let shown = build_plain_wrapped_lines_unified(
            &chat,
            80,
            None,
            None,
            None,
            Some(&mut regions),
            false,
        );
        assert_eq!(shown[0], "⇄ Translated to Chinese ▸ show original");
        assert!(shown.iter().any(|l| l.contains("全部完成")));
        assert!(!shown.iter().any(|l| l.contains("migration passes")));
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].element_id, "translation-0");

        chat.toggle_expanded("translation-0");
        let original = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert!(original[0].starts_with("⇄ Original reply"));
        assert!(original.iter().any(|l| l.contains("migration passes")));
        assert!(!original.iter().any(|l| l.contains("全部完成")));

        chat.toggle_expanded("translation-0");
        let again = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert_eq!(again, shown);
    }

    #[test]
    fn an_untranslated_reply_has_no_marker_or_click_region() {
        let mut chat = ChatStream::new();
        chat.push_text("hello");
        chat.end_text();
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
        assert_eq!(lines[0], "hello");
        assert!(regions.is_empty());
    }

    #[test]
    fn duration_only_header_stays_when_the_next_burst_starts() {
        let mut chat = ChatStream::new();
        chat.finish_thinking(4.0);
        chat.push_thinking("next");
        let lines = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert!(lines[0].contains("Thought for 4s"));
        assert!(!lines[0].contains('▸'));
        assert!(lines.iter().any(|l| l.contains("next")));
    }

    #[test]
    fn thinking_streams_then_collapses_and_reopens() {
        let mut chat = ChatStream::new();
        chat.push_thinking("step one\nstep two");
        chat.update_thinking(3.0);
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
        assert!(collapsed[0].contains("Thought for 3s"));
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].element_id, "thinking-0");

        // Clicking the header reopens the content.
        chat.toggle_expanded("thinking-0");
        let reopened = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert_eq!(reopened.len(), 4);
        assert_eq!(reopened[0], "◐ Thought for 3s ▾");
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
        assert!(lines[0].contains("Thought for 0ms")); // first burst collapsed
        assert!(lines.iter().any(|l| l.contains("second")));
    }

    #[test]
    fn empty_thinking_block_is_not_collapsible() {
        let mut chat = ChatStream::new();
        // A reasoning phase that produced no readable text: only a duration.
        chat.finish_thinking(4.0);
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
        assert_eq!(lines.len(), 2); // header + trailing blank
        assert!(lines[0].contains("Thought for 4s"));
        assert!(!lines[0].contains('▸'));
        assert!(regions.is_empty());

        // Toggling by id is a no-op on an empty block.
        chat.toggle_expanded("thinking-0");
        let lines = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn finish_thinking_collapses_the_phase() {
        let mut chat = ChatStream::new();
        chat.push_thinking("chunk");
        chat.finish_thinking(6.0);
        // The phase ended: the block collapses to its header at once, it does
        // not wait for the next element to arrive.
        let lines = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert!(lines[0].contains("Thought for 6s ▸"));

        chat.collapse_thinking();
        let collapsed = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert!(collapsed[0].contains("Thought for 6s ▸"));
    }

    #[test]
    fn thought_duration_label_matches_the_display_rules() {
        assert_eq!(super::thought_duration_label(0.0), "0ms");
        assert_eq!(super::thought_duration_label(0.047), "47ms");
        assert_eq!(super::thought_duration_label(0.1), "0.1s");
        assert_eq!(super::thought_duration_label(0.5), "0.5s");
        assert_eq!(super::thought_duration_label(12.0), "12s");
        assert_eq!(super::thought_duration_label(64.0), "1m 4s");
        assert_eq!(super::thought_duration_label(60.0), "1m");
        assert_eq!(super::thought_duration_label(3720.0), "1h 2m");
        assert_eq!(super::thought_duration_label(3600.0), "1h");
    }

    #[test]
    fn subsecond_thinking_with_content_keeps_a_duration_header() {
        let mut chat = ChatStream::new();
        chat.push_thinking("step");
        chat.finish_thinking(0.5);
        let lines = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert_eq!(lines[0], "◐ Thought for 0.5s ▸");
    }

    #[test]
    fn finish_thinking_stamps_a_collapsed_block() {
        let mut chat = ChatStream::new();
        chat.push_thinking("step");
        chat.collapse_thinking();
        chat.finish_thinking(0.047);
        let lines = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert_eq!(lines[0], "◐ Thought for 47ms ▸");
        assert_eq!(
            chat.elements()
                .iter()
                .filter(|e| matches!(e, ChatElement::Thinking { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn finish_running_calls_marks_orphans_interrupted() {
        let mut chat = ChatStream::new();
        chat.push_tool(
            "call-1".to_string(),
            ToolDisplay::Builtin {
                tool: "search".to_string(),
                detail: String::new(),
                status: ToolStatus::Running,
                output: None,
                pinned: None,
            },
        );
        chat.finish_running_calls();
        let Some(ChatElement::ToolCall { display, .. }) = chat.elements.last() else {
            panic!("the call stays in the stream");
        };
        assert!(matches!(display.status(), ToolStatus::Failed(_)));
    }

    /// The rows of a tasks.md document feed the todo block — the same path
    /// `refresh_todos` uses after tool calls and on session load.
    #[test]
    fn set_todos_renders_parsed_session_rows() {
        let rows = crate::task_rows::task_rows(
            "## Doing\n- write report\n\n## Queued\n- ship it BLOCKING q1\n",
        );
        let mut chat = ChatStream::new();
        chat.push_text("earlier content");
        chat.end_text();
        chat.set_todos(rows.clone());

        let shown = chat
            .elements()
            .iter()
            .find_map(|element| match element {
                ChatElement::TodoBlock { rows } => Some(rows),
                _ => None,
            })
            .expect("a non-empty document should produce a todo block");
        assert_eq!(shown, &rows);

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

    /// A finished turn folds its work — thinking, calls and the messages
    /// between them — behind one "Worked for" row that clicks open and
    /// shut again.
    #[test]
    fn a_failed_turns_error_stays_outside_the_folded_work() {
        let mut chat = ChatStream::new();
        chat.push_user("hi", Vec::new(), 0);
        chat.push_thinking("draft");
        chat.push_error("permission_denied: This model is no longer available");
        chat.fold_turn_work(0);

        let lines = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert!(
            lines.iter().any(|line| line.contains("permission_denied")),
            "the error shows with the work collapsed: {lines:?}"
        );
        assert!(matches!(
            chat.elements.last(),
            Some(ChatElement::Error { .. })
        ));

        // A turn that only failed has no work to fold.
        let mut chat = ChatStream::new();
        chat.push_user("hi", Vec::new(), 0);
        chat.push_error("boom");
        chat.fold_turn_work(0);
        assert!(
            !chat
                .elements
                .iter()
                .any(|element| matches!(element, ChatElement::WorkedFor { .. }))
        );
    }

    /// A turn whose reply is its only output keeps the reply visible and
    /// gets no folded row: there is no work before it.
    #[test]
    fn a_reply_only_turn_keeps_its_reply_outside_the_fold() {
        let mut chat = ChatStream::new();
        chat.push_user("你知道吗 知っていますか 알아요?", Vec::new(), 0);
        chat.push_text("知道。はい。네.");
        chat.end_text();
        chat.fold_turn_work(7);

        assert!(
            !chat
                .elements
                .iter()
                .any(|element| matches!(element, ChatElement::WorkedFor { .. })),
            "{:#?}",
            chat.elements
        );
        assert!(matches!(
            chat.elements.last(),
            Some(ChatElement::AssistantText { content, .. }) if content == "知道。はい。네."
        ));
    }

    fn work_turn(chat: &mut ChatStream, label: &str) {
        chat.push_thinking(&format!("{label} thought"));
        chat.push_tool(
            format!("tool-{label}"),
            ToolDisplay::Terminal {
                description: String::new(),
                command: format!("echo {label}"),
                mode: SandboxMode::Sandboxed,
                location: None,
                status: ToolStatus::Done,
                output: None,
                diffs: Vec::new(),
                pinned: None,
                reading: None,
                card: None,
            },
        );
    }

    fn worked_rows(chat: &ChatStream) -> Vec<(u64, usize)> {
        let mut adjacent = false;
        let mut previous_was_row = false;
        let mut rows = Vec::new();
        for element in &chat.elements {
            if let ChatElement::WorkedFor {
                seconds, elements, ..
            } = element
            {
                adjacent |= previous_was_row;
                previous_was_row = true;
                rows.push((*seconds, elements.len()));
            } else {
                previous_was_row = false;
            }
        }
        assert!(!adjacent, "two Worked for rows are adjacent: {rows:?}");
        rows
    }

    /// Turns that end without a reply of their own (interrupted, resumed
    /// by a background command, ended on a question) fold into one row
    /// that the next result follows.
    #[test]
    fn resultless_turns_fold_into_one_row_before_the_next_result() {
        // An interrupted turn, a background-resumed turn (no user message)
        // and a turn ending on a question, then a normal turn.
        let mut chat = ChatStream::new();
        chat.push_user("実行して 运行 실행", Vec::new(), 0);
        work_turn(&mut chat, "a");
        chat.fold_turn_work(1800);
        work_turn(&mut chat, "b");
        chat.fold_turn_work(250);
        work_turn(&mut chat, "c");
        chat.fold_turn_work(5);
        work_turn(&mut chat, "d");
        chat.push_text("done 完成 완료");
        chat.end_text();
        chat.fold_turn_work(10);

        assert_eq!(worked_rows(&chat), vec![(1800 + 250 + 5 + 10, 8)]);
        assert!(matches!(
            chat.elements.last(),
            Some(ChatElement::AssistantText { .. })
        ));

        chat.toggle_expanded("work-0");
        let open = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        for label in ["a", "b", "c", "d"] {
            assert!(
                open.iter().any(|l| l.contains(&format!("{label} thought"))
                    || l.contains(&format!("echo {label}"))),
                "{label} missing: {open:?}"
            );
        }
    }

    /// An interrupted run ends on its marker; the earlier resultless
    /// turn folds with the interrupted turn's work ahead of it.
    #[test]
    fn interrupted_turn_after_resultless_turn_shares_one_row() {
        let mut chat = ChatStream::new();
        chat.push_user("hi", Vec::new(), 0);
        work_turn(&mut chat, "a");
        chat.fold_turn_work(1821);
        work_turn(&mut chat, "b");
        chat.push_text("\n\n[Interrupted]");
        chat.end_text();
        chat.fold_turn_work(250);
        assert_eq!(worked_rows(&chat), vec![(2071, 4)]);
    }

    /// A turn with its own user message keeps its own row.
    #[test]
    fn separate_user_turns_keep_separate_rows() {
        let mut chat = ChatStream::new();
        chat.push_user("one", Vec::new(), 0);
        work_turn(&mut chat, "a");
        chat.fold_turn_work(3);
        chat.push_user("two", Vec::new(), 1);
        work_turn(&mut chat, "b");
        chat.fold_turn_work(4);
        assert_eq!(worked_rows(&chat), vec![(3, 2), (4, 2)]);
    }

    #[test]
    fn worked_for_folds_turn_and_toggles() {
        let mut chat = ChatStream::new();
        chat.push_user("hi", Vec::new(), 0);
        chat.push_thinking("draft reply");
        chat.push_tool(
            "t1".to_string(),
            ToolDisplay::Terminal {
                description: String::new(),
                command: "echo hi".to_string(),
                mode: SandboxMode::Sandboxed,
                location: None,
                reading: None,
                card: None,
                status: ToolStatus::Done,
                output: None,
                diffs: Vec::new(),
                pinned: None,
            },
        );
        chat.push_text("done");
        chat.fold_turn_work(52);

        // user message + folded work + the reply, which stays visible.
        assert!(matches!(
            chat.elements.last(),
            Some(ChatElement::AssistantText { .. })
        ));
        let Some(ChatElement::WorkedFor { elements, .. }) = chat.elements.get(1) else {
            panic!("the turn's work folds into one row");
        };
        assert_eq!(elements.len(), 2);

        let collapsed = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert!(collapsed.iter().any(|line| line.contains("Worked for 52s")));
        assert!(!collapsed.iter().any(|line| line.contains("draft reply")));

        chat.toggle_expanded("work-0");
        let open = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert!(open.iter().any(|line| line.contains("draft reply")));

        chat.toggle_expanded("work-0");
        let closed = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert!(!closed.iter().any(|line| line.contains("draft reply")));
    }

    /// Inside an expanded "Worked for" group a thought or command expands
    /// and collapses on its own; the group stays expanded throughout.
    #[test]
    fn worked_for_members_toggle_independently() {
        let mut chat = ChatStream::new();
        chat.push_thinking("inner thought body");
        chat.collapse_thinking();
        chat.push_tool(
            "t1".to_string(),
            ToolDisplay::Terminal {
                description: "run inner".to_string(),
                command: "echo inner".to_string(),
                mode: SandboxMode::Sandboxed,
                location: None,
                reading: None,
                card: None,
                status: ToolStatus::Done,
                output: None,
                diffs: Vec::new(),
                pinned: None,
            },
        );
        chat.push_text("done");
        chat.fold_turn_work(3);
        chat.toggle_expanded("work-0");

        // Expanded group: both members render collapsed — the thought body
        // and the command's body rows stay hidden behind their headlines.
        let grouped = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert!(grouped.iter().any(|l| l.contains("Worked for 3s")));
        assert!(grouped.iter().any(|l| l.contains("Thought for")));
        assert!(grouped.iter().any(|l| l.contains("run inner")));
        assert!(!grouped.iter().any(|l| l.contains("inner thought body")));
        assert!(!grouped.iter().any(|l| l.contains("$ echo inner")));

        // Expanding the nested thought shows its body while the group and
        // the collapsed command stay as they were.
        chat.toggle_expanded("thinking-0");
        let thought_open =
            build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert!(thought_open.iter().any(|l| l.contains("Worked for 3s ▾")));
        assert!(
            thought_open
                .iter()
                .any(|l| l.contains("inner thought body"))
        );
        assert!(!thought_open.iter().any(|l| l.contains("$ echo inner")));
        chat.toggle_expanded("thinking-0");

        // Expanding the nested command shows its body rows; the thought
        // stays collapsed.
        chat.toggle_expanded("t1");
        let cmd_open = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert!(cmd_open.iter().any(|l| l.contains("Worked for 3s ▾")));
        assert!(cmd_open.iter().any(|l| l.contains("$ echo inner")));
        assert!(!cmd_open.iter().any(|l| l.contains("inner thought body")));
        chat.toggle_expanded("t1");

        // A click on a member's rows resolves to the member's region, not
        // the group's — and collapsing the group folds everything away.
        let mut regions = Vec::new();
        let _ = build_plain_wrapped_lines_unified(
            &chat,
            80,
            None,
            None,
            None,
            Some(&mut regions),
            false,
        );
        let member = regions
            .iter()
            .filter(|r| r.element_id == "thinking-0" || r.element_id == "t1")
            .count();
        assert_eq!(member, 2);
        let group = regions
            .iter()
            .find(|r| r.element_id == "work-0")
            .expect("the group keeps its own click region");
        for member_id in ["thinking-0", "t1"] {
            let member = regions.iter().find(|r| r.element_id == member_id).unwrap();
            assert!(member.start >= group.start && member.end <= group.end);
        }

        chat.toggle_expanded("work-0");
        let closed = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        assert!(closed.iter().any(|l| l.contains("Worked for 3s")));
        assert!(!closed.iter().any(|l| l.contains("run inner")));
    }

    /// An expanded thinking block stays clickable — a click folds it back.
    #[test]
    fn expanded_thinking_renders_markdown_not_raw_syntax() {
        let mut chat = ChatStream::new();
        chat.push_thinking(
            "先分析 the request:\n\n- 第一步 *check* config\n- 次のステップ **verify** 확인\n\n```rust\nlet x = 1;\n```",
        );
        chat.collapse_thinking();
        chat.toggle_expanded("thinking-0");
        let out = build_plain_wrapped_lines_unified(&chat, 80, None, None, None, None, false);
        let text = out.join("\n");
        assert!(text.contains("第一步"), "{text}");
        assert!(text.contains("次のステップ"), "{text}");
        assert!(text.contains("확인"), "{text}");
        assert!(text.contains("let x = 1;"), "{text}");
        assert!(!text.contains("```"), "{text}");
        assert!(!text.contains("**"), "{text}");
        assert!(!text.contains("*check*"), "{text}");

        let elem = &chat.elements[0];
        let lines = super::element_lines(
            elem,
            super::ElementState::of(elem, None, false, 0),
            false,
            "",
            80,
        )
        .lines;
        let verify = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("verify"))
            .expect("verify span");
        assert!(
            verify
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert!(
            verify
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::ITALIC)
        );
        assert_eq!(verify.style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn expanded_thinking_has_click_region() {
        let mut chat = ChatStream::new();
        chat.push_thinking("step one");
        let mut regions = Vec::new();
        let _ = build_plain_wrapped_lines_unified(
            &chat,
            80,
            None,
            None,
            None,
            Some(&mut regions),
            false,
        );
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].element_id, "thinking-0");
    }

    /// Rows of the stream and how many of them click open.
    fn rows_and_regions(chat: &ChatStream) -> (Vec<String>, usize) {
        let mut regions = Vec::new();
        let lines = build_plain_wrapped_lines_unified(
            chat,
            200,
            None,
            None,
            None,
            Some(&mut regions),
            false,
        );
        (
            lines.into_iter().filter(|l| !l.is_empty()).collect(),
            regions.len(),
        )
    }

    /// The `OpenAI` Responses API announces a hosted search before it knows
    /// the query and repeats it, quoted, in the result.
    #[test]
    fn a_hosted_search_announced_without_its_query_is_one_row() {
        let mut chat = ChatStream::new();
        chat.push_builtin("ws_1".into(), "web_search".into(), String::new());
        chat.finish_builtin("ws_1", "web_search", "\"Rust 异步 非同期 비동기\"".into());
        chat.push_builtin("wf_1".into(), "web_fetch".into(), String::new());
        chat.finish_builtin("wf_1", "web_fetch", "\"https://example.com/a\"".into());
        // A result with no matching start is appended as a finished row.
        chat.finish_builtin("ws_2", "web_search", "\"late query\"".into());

        let (rows, regions) = rows_and_regions(&chat);
        assert_eq!(
            rows,
            [
                "  ✓ Searched \"Rust 异步 非同期 비동기\"",
                "  ✓ Fetched https://example.com/a",
                "  ✓ Searched \"late query\"",
            ]
        );
        assert_eq!(regions, 0);
    }

    /// The IPC search and fetch rows carry arguments and results too, but
    /// are one row all the same.
    #[test]
    fn an_inline_search_with_a_result_is_one_row() {
        let mut chat = ChatStream::new();
        chat.push_tool(
            "s1".to_string(),
            ToolDisplay::Action {
                chip: "websearch".to_string(),
                task_id: None,
                summary: "may agent".to_string(),
                detail: vec!["{\"query\":\"may agent\"}".to_string()],
                status: ToolStatus::Running,
                output: None,
                pinned: None,
            },
        );
        chat.update_tool_with_output("s1", ToolStatus::Done, Some("result one\nresult two"));
        let (rows, regions) = rows_and_regions(&chat);
        assert_eq!(rows, ["  ✓ Searched \"may agent\""]);
        assert_eq!(regions, 0);
    }

    /// The input band as drawn on a screen: frame buffer, cursor and the
    /// first and last band row.
    struct BandFrame {
        buf: ratatui::buffer::Buffer,
        cursor: ratatui::layout::Position,
    }

    const BAND_BG: Color = Color::Rgb(30, 30, 30);

    fn draw_band(
        chat: &ChatStream,
        width: u16,
        height: u16,
        input: &str,
        queued: &[String],
    ) -> BandFrame {
        draw_band_at(chat, width, height, input, queued, usize::MAX)
    }

    /// `draw_band` at an explicit document-row scroll instead of the
    /// follow-bottom sentinel.
    fn draw_band_at(
        chat: &ChatStream,
        width: u16,
        height: u16,
        input: &str,
        queued: &[String],
        scroll: usize,
    ) -> BandFrame {
        use super::{StatusContext, UnifiedRenderContext, render_unified};
        use ratatui::{Terminal, backend::TestBackend};

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut layout = super::ChatLayout::default();
        let mut buf = None;
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_unified(
                    frame,
                    area,
                    chat,
                    &mut layout,
                    scroll,
                    UnifiedRenderContext {
                        selected_tool: None,
                        content_height_out: None,
                        view_height_out: None,
                        input_ctx: Some(InputContext {
                            input,
                            cursor: input.len(),
                            attachments: &[],
                            suggestion: None,
                            greeting: None,
                        }),
                        input_caret_active: true,
                        queue_ctx: (!queued.is_empty())
                            .then_some(super::QueueContext { messages: queued }),
                        status_ctx: Some(StatusContext {
                            model: "model",
                            usage: Some((4_800, Some(120_000))),
                            compaction: CompactionMeter::Idle,
                            context_policy: aither_agent::ContextPolicy::default(),
                            queue_count: 0,
                            open_questions: 0,
                            yolo: false,
                            shells: 0,
                            subagents: 0,
                            decode_rate: None,
                            status_message: None,
                            cwd: "~/may",
                            activity: None,
                            spinner_frame: 0,
                            index_coverage: None,
                            index_failed: false,
                            index_loading: false,
                        }),
                        images_supported: false,
                        hide_todos: false,
                        image_positions_out: None,
                        spinner_frame: 0,
                        band_bg: Some(BAND_BG),
                    },
                );
                buf = Some(frame.buffer_mut().clone());
            })
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        BandFrame {
            buf: buf.unwrap(),
            cursor,
        }
    }

    fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    fn row_has_band(buf: &ratatui::buffer::Buffer, y: u16) -> bool {
        (0..buf.area.width).all(|x| buf[(x, y)].bg == BAND_BG)
    }

    fn row_has_no_band(buf: &ratatui::buffer::Buffer, y: u16) -> bool {
        (0..buf.area.width).all(|x| buf[(x, y)].bg != BAND_BG)
    }

    fn sample_chat() -> ChatStream {
        let mut chat = ChatStream::new();
        chat.push_user("hello", Vec::new(), 0);
        chat.push_text("Hi there, this is the last chat row.");
        chat.end_text();
        chat
    }

    #[test]
    fn band_rows_come_in_order_with_a_plain_margin_above() {
        let frame = draw_band(&sample_chat(), 80, 20, "draft", &[]);
        let buf = &frame.buf;
        // Bottom up: pad, status, blank, input, pad, margin, chat.
        let last = 19;
        assert!(row_has_band(buf, last), "bottom padding row");
        assert_eq!(row_text(buf, last).trim(), "");
        let status = row_text(buf, last - 1);
        assert!(row_has_band(buf, last - 1));
        assert!(status.starts_with("  ~/may"), "{status:?}");
        assert!(status.trim_end().ends_with("(5%)"), "{status:?}");
        assert!(row_has_band(buf, last - 2) && row_text(buf, last - 2).trim().is_empty());
        assert!(row_has_band(buf, last - 3));
        assert_eq!(row_text(buf, last - 3).trim_end(), "  draft");
        assert!(row_has_band(buf, last - 4) && row_text(buf, last - 4).trim().is_empty());
        // One plain margin row, then chat text with no band.
        assert!(row_has_no_band(buf, last - 5));
        assert_eq!(row_text(buf, last - 5).trim(), "");
        // The reply's own trailing blank row, then its last text row.
        assert!(row_has_no_band(buf, last - 6));
        assert!(row_has_no_band(buf, last - 7));
        assert!(row_text(buf, last - 7).contains("last chat row"));
    }

    #[test]
    fn band_keeps_two_columns_of_padding_on_both_sides() {
        let frame = draw_band(&sample_chat(), 80, 20, "draft", &[]);
        let buf = &frame.buf;
        for y in 14..=19 {
            let text = row_text(buf, y);
            assert!(text.starts_with("  "), "left padding of row {y}");
            assert!(text.ends_with("  "), "right padding of row {y}");
        }
        // The context size ends two columns before the right edge.
        let status = row_text(buf, 18);
        assert_eq!(status.trim_end().chars().count(), 78, "{status:?}");
    }

    #[test]
    fn chat_text_lines_up_with_the_input() {
        let frame = draw_band(&sample_chat(), 80, 20, "draft", &[]);
        let chat_row = row_text(&frame.buf, 12);
        assert_eq!(chat_row.find('H'), Some(2), "{chat_row:?}");
        let input_row = row_text(&frame.buf, 16);
        assert_eq!(input_row.find('d'), Some(2));
    }

    #[test]
    fn user_message_rows_fill_the_band_background() {
        let frame = draw_band(&sample_chat(), 80, 20, "draft", &[]);
        let user = (0..20)
            .find(|&y| row_text(&frame.buf, y).contains("> hello"))
            .expect("user message row");
        assert!(row_has_band(&frame.buf, user));
        // The assistant reply below keeps the terminal background.
        let reply = (0..20)
            .find(|&y| row_text(&frame.buf, y).contains("last chat row"))
            .expect("reply row");
        assert!(row_has_no_band(&frame.buf, reply));
    }

    /// A blank row directly above the message is its top padding: it
    /// draws the band too, so the block breathes like the input band.
    /// A non-blank row above is never shaded.
    #[test]
    fn a_blank_row_above_a_user_message_is_its_padding() {
        let mut chat = ChatStream::new();
        chat.push_text("first reply");
        chat.end_text();
        chat.push_user("hello", Vec::new(), 1);
        chat.push_text("next reply");
        chat.end_text();
        let frame = draw_band(&chat, 80, 20, "draft", &[]);
        let user = (0..20)
            .find(|&y| row_text(&frame.buf, y).contains("> hello"))
            .expect("user message row");
        assert_eq!(row_text(&frame.buf, user - 1).trim(), "");
        assert!(row_has_band(&frame.buf, user - 1), "padding row above");
        assert!(row_has_band(&frame.buf, user + 1), "padding row below");
        let reply = (0..20)
            .find(|&y| row_text(&frame.buf, y).contains("first reply"))
            .expect("reply row");
        assert!(row_has_no_band(&frame.buf, reply));
    }

    /// Every context that can precede a user message leaves it one shaded
    /// row above and one below (#911): assistant prose with its own
    /// paragraphs, and a folded "Worked for" group.
    #[test]
    fn a_user_message_after_assistant_prose_is_padded_on_both_sides() {
        let mut chat = ChatStream::new();
        chat.push_user("first", Vec::new(), 0);
        chat.push_thinking("plan");
        chat.push_text(
            "swe-2 的 reasoning 是这样的。\n\nSecond paragraph of the reply with `code`.",
        );
        chat.fold_turn_work(1821);
        chat.push_user("swe-2 应该有 reasoning 的", Vec::new(), 1);
        chat.push_thinking("plan");
        chat.push_text("ok");
        chat.fold_turn_work(1821);
        let frame = draw_band(&chat, 80, 30, "draft", &[]);
        let user = (0..30)
            .find(|&y| row_text(&frame.buf, y).contains("> swe-2"))
            .expect("user message row");
        assert_eq!(row_text(&frame.buf, user - 1).trim(), "");
        assert!(row_has_band(&frame.buf, user - 1), "padding row above");
        assert!(row_has_band(&frame.buf, user + 1), "padding row below");
        assert!(row_has_no_band(&frame.buf, user - 2));
    }

    /// The padding rows belong to the message, so a row that is not blank
    /// above it (an error line ends without a separator) is no exception.
    #[test]
    fn a_user_message_after_a_non_blank_row_is_still_padded() {
        let mut chat = ChatStream::new();
        chat.push_user("first", Vec::new(), 0);
        chat.push_error("boom");
        chat.push_user("second", Vec::new(), 1);
        let frame = draw_band(&chat, 80, 30, "draft", &[]);
        let user = (0..30)
            .find(|&y| row_text(&frame.buf, y).contains("> second"))
            .expect("user message row");
        assert!(row_has_band(&frame.buf, user - 1), "padding row above");
        assert!(row_has_band(&frame.buf, user + 1), "padding row below");
        assert!(
            row_text(&frame.buf, user - 2).contains("boom")
                || row_text(&frame.buf, user - 3).contains("boom")
        );
    }

    /// Scrolled up, the band's tail sits below the window; painting its
    /// unclamped rows indexed outside the buffer and panicked (#871).
    #[test]
    fn a_scrolled_band_below_the_viewport_stays_inside_the_buffer() {
        let mut chat = ChatStream::new();
        for i in 0..20usize {
            chat.push_user(&format!("message {i}"), Vec::new(), i);
            chat.push_text("reply");
            chat.end_text();
        }
        let frame = draw_band_at(&chat, 80, 20, "draft", &[], 5);
        // The band is out of view; a visible user message still draws it.
        let user = (0..20)
            .find(|&y| row_text(&frame.buf, y).contains("> message"))
            .expect("a user message row in view");
        assert!(row_has_band(&frame.buf, user));
    }

    #[test]
    fn chat_wraps_inside_the_margins() {
        let mut chat = ChatStream::new();
        chat.push_text(&"word ".repeat(40));
        chat.end_text();
        let frame = draw_band(&chat, 40, 20, "", &[]);
        for y in 0..13 {
            let text = row_text(&frame.buf, y);
            assert!(
                text.starts_with("  ") && text.ends_with("  "),
                "row {y}: {text:?}"
            );
        }
    }

    #[test]
    fn caret_sits_after_the_typed_text() {
        let frame = draw_band(&sample_chat(), 80, 20, "draft", &[]);
        // "draft" starts at column 2 (band padding); the caret follows the last char.
        assert_eq!((frame.cursor.x, frame.cursor.y), (2 + 5, 16), "caret");
    }

    #[test]
    fn caret_follows_wrapped_input() {
        let text = "x".repeat(50);
        let frame = draw_band(&sample_chat(), 40, 20, &text, &[]);
        // Content is 36 wide: 36 x fill the first row, 14 wrap to the next.
        assert_eq!((frame.cursor.x, frame.cursor.y), (2 + 14, 16));
        assert!(row_has_band(&frame.buf, 16));
    }

    #[test]
    fn queue_lines_stay_above_the_band() {
        let queued = vec!["later".to_string()];
        let frame = draw_band(&sample_chat(), 80, 24, "", &queued);
        let buf = &frame.buf;
        let queue_row = (0..24)
            .find(|&y| row_text(buf, y).contains("Queued"))
            .expect("queue header");
        assert!(row_has_no_band(buf, queue_row));
        // Header, one message, margin row, then the band.
        assert!(row_has_no_band(buf, queue_row + 2));
        assert!(row_has_band(buf, queue_row + 3));
    }

    #[test]
    fn narrow_width_drops_padding_but_keeps_the_band() {
        let frame = draw_band(&sample_chat(), 8, 20, "ab", &[]);
        let buf = &frame.buf;
        assert!(row_has_band(buf, 19), "bottom padding row");
        let input = (0..20)
            .find(|&y| row_text(buf, y).starts_with("ab"))
            .expect("input row at column 0");
        assert!(row_has_band(buf, input));
        assert!(row_has_band(buf, input - 1), "top padding row");
        assert!(row_has_no_band(buf, input - 2), "margin row");
    }

    #[test]
    fn plain_rows_match_the_screen_structure() {
        use super::StatusContext;
        let status = StatusContext {
            model: "model",
            usage: None,
            compaction: CompactionMeter::Idle,
            context_policy: aither_agent::ContextPolicy::default(),
            queue_count: 0,
            open_questions: 0,
            yolo: false,
            shells: 0,
            subagents: 0,
            decode_rate: None,
            status_message: None,
            cwd: "~",
            activity: None,
            spinner_frame: 0,
            index_coverage: None,
            index_failed: false,
            index_loading: false,
        };
        let input = InputContext {
            input: "hi",
            cursor: 2,
            attachments: &[],
            suggestion: None,
            greeting: None,
        };
        let rows = build_plain_wrapped_lines_unified(
            &sample_chat(),
            80,
            None,
            Some(&input),
            Some(&status),
            None,
            false,
        );
        let n = rows.len();
        assert_eq!(&rows[n - 6..n - 3], ["", "", "hi"]);
        assert_eq!(rows[n - 3], "");
        assert!(rows[n - 2].starts_with("~ · model"));
        assert_eq!(rows[n - 1], "");
    }

    /// The status bar's right edge for `usage` and `compaction`.
    fn status_row(compaction: CompactionMeter) -> String {
        use super::StatusContext;
        let status = StatusContext {
            model: "model",
            usage: Some((80_800, Some(262_144))),
            compaction,
            context_policy: aither_agent::ContextPolicy::default(),
            queue_count: 0,
            open_questions: 0,
            yolo: false,
            shells: 0,
            subagents: 0,
            decode_rate: None,
            status_message: None,
            cwd: "~",
            activity: None,
            spinner_frame: 0,
            index_coverage: None,
            index_failed: false,
            index_loading: false,
        };
        let rows = build_plain_wrapped_lines_unified(
            &sample_chat(),
            80,
            None,
            None,
            Some(&status),
            None,
            false,
        );
        rows.iter()
            .find(|row| row.starts_with("~ · model"))
            .expect("status row")
            .clone()
    }

    #[test]
    fn the_context_meter_shows_the_compaction_in_place_of_the_usage() {
        let now = std::time::Instant::now();
        assert!(status_row(CompactionMeter::Idle).trim_end().ends_with("%)"));
        assert!(
            status_row(CompactionMeter::Compacting)
                .trim_end()
                .ends_with("Compacting")
        );
        let held = CompactionMeter::Compacted { until: now };
        assert!(status_row(held).trim_end().ends_with("Compacted"));
    }

    /// Render a sample screen and write every cell as JSON lines to
    /// `MAY_FRAME_DUMP` (symbol, fg, bg) — the input for the frame-to-PNG
    /// preview script.
    #[test]
    #[ignore = "writes the frame JSON for visual preview"]
    fn dump_frame_json() {
        use std::fmt::Write as _;
        let mut chat = ChatStream::new();
        chat.push_user("帮我做一个变更，合入dev，去掉`>`", Vec::new(), 1);
        chat.push_text("好的，我已经把输入框的 `>` 提示符去掉了：\n\n- 普通输入现在顶格显示\n- bash 模式仍然保留 `!` 高亮");
        chat.end_text();
        chat.push_user("第二条消息，看看背景填充效果", Vec::new(), 2);
        chat.push_text("这是第二条回复。");
        chat.end_text();
        let frame = draw_band(&chat, 80, 24, "draft message", &[]);
        let path = std::env::var("MAY_FRAME_DUMP").unwrap_or_else(|_| "frame.jsonl".into());
        let mut out = String::new();
        for y in 0..frame.buf.area.height {
            for x in 0..frame.buf.area.width {
                let cell = &frame.buf[(x, y)];
                let _ = writeln!(
                    out,
                    "{{\"x\":{x},\"y\":{y},\"s\":{:?},\"fg\":\"{:?}\",\"bg\":\"{:?}\"}}",
                    cell.symbol(),
                    cell.fg,
                    cell.bg
                );
            }
        }
        std::fs::write(&path, out).unwrap();
        eprintln!("wrote {path}");
    }

    #[test]
    #[ignore = "prints one frame for eyeballing"]
    fn dump_band_frame() {
        let frame = draw_band(&sample_chat(), 80, 14, "draft", &[]);
        for y in 0..14 {
            let line: String = (0..80)
                .map(|x| {
                    let cell = &frame.buf[(x, y)];
                    if cell.symbol() == " " && cell.bg == BAND_BG {
                        "░"
                    } else {
                        cell.symbol()
                    }
                })
                .collect();
            println!("{line}");
        }
    }

    fn compact_row(status: ToolStatus, result: Option<&str>) -> (Vec<String>, usize) {
        let mut chat = ChatStream::new();
        chat.push_tool(
            "c1".to_string(),
            ToolDisplay::Action {
                chip: "compact".to_string(),
                task_id: None,
                summary: String::new(),
                detail: Vec::new(),
                status: ToolStatus::Running,
                output: None,
                pinned: None,
            },
        );
        chat.update_tool_with_output("c1", status, result);
        rows_and_regions(&chat)
    }

    /// A compaction is one status row: a tick when it started, a cross
    /// with the refusal when the tool returned its typed error, and nothing
    /// to expand either way.
    #[test]
    fn a_compaction_is_a_one_line_status_row() {
        let (rows, regions) = compact_row(
            ToolStatus::Done,
            Some("Compaction started in the background."),
        );
        assert_eq!(rows, ["  ✓ Compaction"]);
        assert_eq!(regions, 0);

        let (rows, regions) = compact_row(
            ToolStatus::Failed("Nothing was started: the context is too small.".to_string()),
            None,
        );
        assert_eq!(
            rows,
            ["  ✗ Compaction — failed: Nothing was started: the context is too small."]
        );
        assert_eq!(regions, 0);
    }
}
