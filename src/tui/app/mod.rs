//! Application state and logic.

mod completion;
mod config;
mod model_picker;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use async_channel::Receiver;
use blocking::unblock;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use executor_core::spawn;

use crate::config::{ModelTier, ProviderType};
use crate::models::FetchedModel;
use crate::profile::{PinnedItem, ProfileInfo, ProfileSessions};
use crate::session::PendingUserMessageMode;
use crate::{ChatMessage, MayConfig, MessageRole, SessionId, SessionManager, SessionSummary};

use super::agent_event_adapter::{
    ToolEndKind, ToolStartKind, parse_tool_end, parse_tool_start, parse_tool_start_partial,
};
use super::clipboard::{
    ClipboardPasteResult, ImageManager, TextManager, download_image, is_image_url,
    pasted_image_path_candidate, read_clipboard, read_pasted_image_file, set_clipboard_html,
    set_clipboard_text,
};
use super::completion::{Completion, CompletionMode};
use super::error::{AppResult, TuiError};
use super::event::{AppEvent, EventSender};
use super::file_diff::FileDiffHandle;
use super::history::InputHistory;
use super::queue::{Attachment, MessageQueue, PendingMessage};
use super::slash::SlashCommand;
use super::ui::chat::{ChatStream, ToolDisplay, clamp_column_by_width};
use super::ui::file_picker::FilePicker;
use crate::pasted_text::LARGE_PASTE_THRESHOLD;

/// User-facing message shown when a tool response cannot be sent to the session.
pub const ERR_TOOL_RESPONSE_SEND: &str = include_str!("../../texts/tool_response_send_failed.txt");

/// Two Esc presses within this window open the rewind picker.
const DOUBLE_ESC_WINDOW: std::time::Duration = std::time::Duration::from_millis(1000);

/// Two clicks at the same spot within this window select the word under
/// the cursor.
const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

/// How long a transient status notice (e.g. "Copied selection") stays on
/// screen before it clears itself.
const STATUS_NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// Convert a character index to a byte index in a string.
pub(super) fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(i, _)| i)
}

/// Agent event from aither-agent.
pub use aither_agent::{AgentError, AgentEvent, TimedEvent};

use super::state::{
    ActiveTextInput, AppMode, AskUserState, AuthReturn, ChatgptAuthStatus, ConfigEditMode,
    ConfigState, CopilotAuthStatus, GrokAuthStatus, ImageViewState, McpAddMode,
    OpenrouterAuthStatus, PasteFilter, QuitConfirmState, ResumePickerState, SelectionPoint,
    SkillAddMode, SshAddMode, StatusNotice, TextSelection, detect_api_key_from_env,
};

/// Screen rect an inline image was painted into last frame — hit-tested on
/// clicks to open the zoom view.
#[derive(Debug)]
pub struct ImageRegion {
    /// Absolute screen area the image occupies.
    pub rect: ratatui::layout::Rect,
    /// Key of the decoded image in `image_cache`.
    pub key: String,
    /// Path or URL shown under the zoomed image.
    pub caption: String,
}
/// A suggested next prompt the daemon generated for a session.
#[derive(Debug)]
pub struct PromptSuggestion {
    /// Session the suggestion belongs to.
    pub session_id: String,
    /// The suggested prompt.
    pub text: String,
}

/// The suggested prompt while it belongs to the attached session and the
/// input is empty — the only state in which ghost text shows.
///
/// Field-level borrows keep it callable while another field is mutably
/// borrowed.
#[must_use]
pub fn visible_suggestion<'a>(
    prompt_suggestion: Option<&'a PromptSuggestion>,
    input: &str,
    current_session_id: Option<&str>,
) -> Option<&'a str> {
    let suggestion = prompt_suggestion?;
    (input.is_empty() && current_session_id == Some(suggestion.session_id.as_str()))
        .then_some(suggestion.text.as_str())
}

/// Application state (UI only - agent managed externally).
#[derive(Debug)]
pub struct App {
    /// Session manager
    pub session_manager: SessionManager,
    /// Configuration
    pub config: MayConfig,
    /// Config values the current agent worker was built from; a saved change
    /// to them replaces the worker.
    pub worker_settings: crate::agent::AgentLiveSettings,
    /// Current input text
    pub input: String,
    /// Input cursor position (character index, not byte index)
    pub cursor: usize,
    /// Cached character count for input (avoids O(n) .`chars().count()`)
    pub input_char_count: usize,
    /// The daemon's suggested next prompt, drawn as ghost text while the
    /// input is empty; `Tab` lifts it into the draft.
    pub prompt_suggestion: Option<PromptSuggestion>,
    /// Pending attachments
    pub attachments: Vec<Attachment>,
    /// Chat stream for display
    pub chat: ChatStream,
    /// Scroll offset in chat (`usize::MAX` = follow bottom)
    pub scroll: usize,
    /// Whether chat input is shown (hidden during full-screen scroll)
    pub input_visible: bool,
    /// Last known content height (set during rendering for scroll normalization)
    pub content_height: AtomicUsize,
    /// View height (set during rendering for scroll normalization)
    pub view_height: AtomicUsize,
    /// Current mode
    pub mode: AppMode,
    /// Message queue
    pub queue: MessageQueue,
    /// Status-line notice; a transient one clears once its deadline passes.
    pub status: Option<StatusNotice>,
    /// Whether agent is currently processing
    pub processing: bool,
    /// Hidden context attached to the next send after a rewind: tells the
    /// agent the conversation was cut back while the world — open and
    /// merged PRs — stands as recorded.
    pub rewind_notice: Option<crate::xml::XmlBlock>,
    /// The hidden context the latest send carried, kept client-side until
    /// the next send: a withdrawn turn deletes its rows, so the sender
    /// attaches it to the message it sends again.
    sent_hidden_context: Option<crate::xml::XmlBlock>,
    /// File picker state (when in `FilePicker` mode)
    pub file_picker: Option<FilePicker>,
    /// Image manager for clipboard images
    pub image_manager: Option<ImageManager>,
    /// Text manager for clipboard text
    pub text_manager: Option<TextManager>,
    /// Input history for up/down navigation
    pub history: InputHistory,
    /// Input completion state
    pub completion: Completion,
    /// Session list for selection (when in `SessionSelect` mode)
    pub session_list: Vec<SessionSummary>,
    /// Selected session index (in filtered list)
    pub session_selected: usize,
    /// Session search query
    pub session_search: String,
    /// Session list scroll offset
    pub session_scroll: usize,
    /// Currently selected tool ID (for `ToolDetail` mode)
    pub selected_tool: Option<String>,
    /// Tool selection index in tool list
    pub selected_tool_index: usize,
    /// Scroll offset in tool detail view
    pub tool_detail_scroll: usize,
    /// Rows the help sheet is scrolled.
    pub help_scroll: u16,
    /// Unified configuration state (for Config mode)
    pub config_state: ConfigState,
    /// Selected index in model select popup
    pub model_select_index: usize,
    /// Available models for model select (cached)
    pub model_select_list: Vec<String>,
    /// Search query in the model select popup
    pub model_select_query: String,
    /// Last fetched provider catalog; seeds the next `/model` open so the
    /// picker does not paint the configured-only list while a fetch lands
    pub model_catalog: Vec<FetchedModel>,
    /// Effort rungs offered by the `/effort` popup
    pub effort_ladder: Vec<&'static str>,
    /// Effort rungs each fetched model family serves, keyed by
    /// `(provider_id, family base)` — the `/effort` popup shows a family's
    /// own ladder when the catalog reports one.
    pub family_effort_rungs: crate::models::FamilyEffortRungs,
    /// When the current turn began; `None` while idle.
    pub turn_started_at: Option<std::time::Instant>,
    /// Selected index in the effort popup
    pub effort_index: usize,
    /// Session's stored reasoning effort as last reported by the daemon
    pub current_effort: Option<String>,
    /// Whether we need to run setup (config incomplete)
    pub needs_setup: bool,
    /// Current working directory (where May was launched)
    pub cwd: PathBuf,
    /// Whether quit confirmation is pending (Ctrl+C pressed once)
    pub quit_pending: bool,
    /// When the last Esc was handled in chat mode; a second Esc inside the
    /// window opens the rewind picker.
    pub last_esc_at: Option<std::time::Instant>,
    /// When thinking started (for thinking models)
    pub thinking_start: Option<std::time::Instant>,
    /// Copilot OAuth authentication status
    pub copilot_auth_status: Option<CopilotAuthStatus>,
    /// `ChatGPT` OAuth authentication status
    pub chatgpt_auth_status: Option<ChatgptAuthStatus>,
    /// xAI Grok OAuth authentication status
    pub grok_auth_status: Option<GrokAuthStatus>,
    /// `OpenRouter` OAuth authentication status
    pub openrouter_auth_status: Option<OpenrouterAuthStatus>,
    /// Selected row in the `/login` provider picker
    pub login_selected: usize,
    /// Screen an OAuth flow returns to when it closes
    pub auth_return: AuthReturn,
    /// Tier being edited in `TierModelPicker` mode
    pub tier_picker_tier: Option<ModelTier>,
    /// All available models for tier picker
    pub tier_picker_models: Vec<FetchedModel>,
    /// Filtered models based on search query
    pub tier_picker_filtered: Vec<usize>,
    /// Selected index in filtered list
    pub tier_picker_selected: usize,
    /// Search query in tier picker
    pub tier_picker_query: String,
    /// Whether models are being fetched
    pub tier_picker_loading: bool,
    /// Receiver for fetched models (oneshot)
    pub model_fetch_rx: Option<Receiver<Vec<FetchedModel>>>,
    /// Receiver for provider catalogs merged into the model-select popup
    pub model_select_rx: Option<Receiver<Vec<FetchedModel>>>,
    /// Per-session file-diff worker (rebuilt on session switch)
    pub file_diffs: Option<FileDiffHandle>,
    /// The daemon's answers to clone-root requests, waiting to join tracking
    clone_roots: (
        async_channel::Sender<Vec<crate::file_tracker::TrackedRootSpec>>,
        Receiver<Vec<crate::file_tracker::TrackedRootSpec>>,
    ),
    /// Per-root session file changes shown in the sidebar's Changes
    /// section; refreshed from the diff worker's status outcomes.
    pub file_status: Vec<crate::file_tracker::RootDiff>,
    /// Clone-based trust (`full` / `prs-only` / `local-branch`) hides the
    /// Changes section; PRs and issues carry the work instead.
    pub sidebar_file_changes: bool,
    /// Receiver for clipboard paste resolution (oneshot)
    pub clipboard_rx: Option<Receiver<Result<ClipboardPasteResult, TuiError>>>,
    /// Pending paste results waiting to be handled
    pub pending_paste_results: std::collections::VecDeque<PendingPaste>,
    /// Pending attachment jobs (clipboard read / download)
    pub pending_attachment_jobs: usize,
    /// Latest request's token usage; `prompt_tokens` is the status bar's
    /// context footprint since every request resends the whole context
    pub usage: Option<aither_core::llm::Usage>,
    /// Decode-speed measurement for the streaming response
    pub decode_meter: crate::decode_meter::DecodeMeter,
    /// New Year greeting and fireworks
    pub new_year: crate::tui::new_year::NewYearState,
    /// Spinner animation frame for the activity indicator above the status bar
    pub spinner_frame: usize,
    /// What the in-flight run is doing (thinking vs. producing output)
    pub activity_phase: crate::tui::ui::chat::ActivityPhase,
    /// Context window size for current model
    pub context_window: Option<u32>,
    /// Current model being used by the agent
    pub current_model: Option<String>,
    /// MCP server info for display (cached)
    pub mcp_servers: Vec<crate::mcp::McpServerInfo>,
    /// Selected MCP server index
    pub mcp_selected: usize,
    /// MCP add mode (entering name/command)
    pub mcp_add_mode: McpAddMode,
    /// MCP add name input
    pub mcp_add_name: String,
    /// MCP add command input
    pub mcp_add_command: String,
    /// Selected SSH server index
    pub ssh_selected: usize,
    /// SSH add mode (entering name/target)
    pub ssh_add_mode: SshAddMode,
    /// SSH add name input
    pub ssh_add_name: String,
    /// SSH add target input (`user@host`)
    pub ssh_add_target: String,
    /// Skill list for display
    pub skill_list: Vec<crate::skill::SkillInfo>,
    /// Selected skill index
    pub skill_selected: usize,
    /// Skill scroll offset for long lists
    pub skill_scroll: usize,
    /// Skill add mode (entering GitHub repo or searching)
    pub skill_add_mode: SkillAddMode,
    /// GitHub repo input for skill installation
    pub skill_github_input: String,
    /// Skill search query
    pub skill_search: String,

    // ── Profile state ─────────────────────────────────────────
    /// Available profiles (loaded at startup)
    pub profiles: Vec<ProfileInfo>,
    /// Currently active profile ID
    pub current_profile_id: Option<String>,
    /// Profile sessions for the current profile (pinned/regular/archived)
    pub profile_sessions: Option<ProfileSessions>,
    /// Selected profile index in `ProfileSelect` mode
    pub profile_selected: usize,
    /// Which sessions `SessionSelect` lists.
    pub session_view: super::state::SessionView,
    /// The task following the daemon's pushed session lists of a profile
    /// while `SessionSelect` is open.
    profile_follow: Option<(String, tokio::task::JoinHandle<()>)>,

    /// Event sender for spawning background tasks
    pub event_tx: EventSender,
    /// Terminal image rendering support (None if terminal doesn't support graphics)
    pub image_support: Option<super::ui::image::TerminalImageSupport>,
    /// Background of the input band, queried from the terminal at startup;
    /// `None` keeps the terminal's default background.
    pub band_bg: Option<ratatui::style::Color>,
    /// Image cache: URL -> fetched image state (for terminal image rendering)
    pub image_cache: ahash::AHashMap<String, super::ui::image::ImageState>,
    /// URLs currently being fetched (to avoid duplicate requests)
    pub image_fetching: ahash::AHashSet<String>,
    /// Image keys that failed to fetch or decode (avoids retry loops)
    pub image_failed: ahash::AHashSet<String>,
    /// Screen rects inline images were painted into last frame (click to zoom)
    pub image_regions: Vec<ImageRegion>,
    /// Image rect inside the zoom overlay — a click outside it closes the view
    pub image_view_area: Option<ratatui::layout::Rect>,
    /// Mouse/text selection in rendered chat content
    pub selection: Option<TextSelection>,
    /// Where the pointer last was inside the chat column (screen cell);
    /// the link under it is underlined and its target shown.
    pub hover: Option<(u16, u16)>,
    /// Opens link targets; replaced in tests.
    pub link_opener: std::sync::Arc<dyn super::ui::links::LinkOpener>,
    /// Laid-out chat transcript: the render cache, and the plain-text
    /// mirror (selection, copy, click hit-testing) of what was drawn.
    pub chat_layout: super::ui::chat_layout::ChatLayout,
    /// Timing breakdown of the last rendered frame (jank reporting).
    pub frame_stats: super::perf::FrameStats,
    /// When and where the last plain click finished; a second press at the
    /// same spot inside `DOUBLE_CLICK_WINDOW` selects the word under it.
    last_click: Option<(Instant, SelectionPoint)>,
    /// The session's background jobs and subagent runs in the daemon (set
    /// when the agent worker installs).
    pub background: Option<crate::client::background::RemoteBackground>,
    /// Daemon that answers `/btw` when the agent runs there.
    pub btw_daemon: Option<crate::client::DaemonClient>,
    /// Right sidebar: task tree, linked issues (background lists come
    /// from `jobs` / `subagents`).
    pub sidebar: super::sidebar::SidebarState,
    /// Screen rect the chat column occupied last frame (mouse mapping).
    pub chat_area: ratatui::layout::Rect,
    /// Latest job list shown in `AppMode::Jobs`
    pub jobs: Vec<aither_sandbox::JobInfo>,
    /// Selected row in `AppMode::Jobs`
    pub jobs_selected: usize,
    /// The session's subagent runs in flight, as the daemon last reported
    /// them.
    pub subagents: Vec<crate::agent::SubagentRunSnapshot>,
    /// Last live terminal-output read from the daemon.
    pub live_output_read_at: Option<std::time::Instant>,
    /// The lease of the run in flight, held from its start until the TUI has
    /// persisted its last message.
    pub run_guard: Option<crate::session::SessionRunGuard>,
    /// Running terminal calls bound to registry jobs for live output.
    pub live_reads: std::collections::HashMap<String, LiveRead>,
    /// Tool calls still streaming their arguments, keyed by call id: the
    /// cumulative JSON `ToolCallDelta` fragments assembled so far, so the
    /// call's element can render before the canonical `ToolCallStart`.
    pub pending_tool_calls: std::collections::HashMap<String, PendingToolCall>,
    /// Last time file-diff previews were requested for running calls.
    pub diff_preview_at: Option<std::time::Instant>,
    /// Loaded secret values for bash-mode masking and env injection.
    pub secrets: crate::secrets::Secrets,
    /// Questions the agent asked that nobody answered yet, for the status bar.
    pub open_question_count: usize,
    /// The session's blocking state as the daemon last reported it on the
    /// `session_runtime` stream.
    pub session_runtime: crate::client::SessionRuntimeSnapshot,
    /// Permission requests whose dialog was already opened, so a snapshot
    /// that still lists an answered request does not reopen it.
    shown_permission_requests: std::collections::HashSet<String>,
    /// Coverage of the session's worktree index, as the `session_index`
    /// stream last reported it; `None` until the first report or when the
    /// session is not in an indexed worktree.
    pub index_coverage: Option<crate::client::IndexCoverage>,
    /// Whether the user is asleep, as the daemon last reported on the
    /// `sleep` stream.
    pub asleep: bool,
    /// Health reminders the daemon had raised when last reported; `None`
    /// until the first report.
    pub sleep_reminders: Option<u64>,
    /// Set when the last `session_index` report said repo-search could not
    /// be reached. The status bar shows a red cross instead of coverage.
    pub index_failed: bool,
    /// Waiting for the daemon's first index snapshot; the status bar shows a
    /// spinner meanwhile.
    pub index_loading: bool,
    /// Which open question `/questions` shows next, so repeated use walks
    /// through all of them.
    pub question_cursor: usize,
    /// The open question being answered when a blocking prompt took over the
    /// screen; it comes back, draft intact, when the prompt is done.
    parked_question: Option<AskUserState>,
}

/// A paste queued for the main loop: either text applied through the
/// terminal-paste path or a result destined for an attachment.
#[derive(Debug)]
pub enum PendingPaste {
    /// Text applied like a terminal paste: into the focused input when it
    /// fits, or an attachment when it is large.
    Input(String),
    /// Clipboard read or large terminal paste that becomes an attachment
    /// (image or pasted text), or a clipboard failure to report.
    Attachment(Result<ClipboardPasteResult, TuiError>),
}

/// Live-output read state for a running terminal call bound to a
/// job-registry task.
#[derive(Debug)]
pub struct LiveRead {
    /// Registry task the call was bound to.
    pub task_id: String,
    /// Byte cursor into the task's output buffer.
    pub cursor: usize,
    /// Trailing bytes of an incomplete multi-byte UTF-8 sequence, held for
    /// the next chunk so the live preview never shows mojibake.
    pub pending: Vec<u8>,
}

/// A tool call whose arguments are still streaming in.
#[derive(Debug, Default)]
pub struct PendingToolCall {
    /// Tool name; empty until the first named fragment arrives.
    pub name: String,
    /// Cumulative arguments JSON — fragments concatenate into the call's
    /// full payload.
    pub arguments: String,
    /// The canonical `ToolCallStart` landed; later deltas are stale and
    /// must not regress the fully-parsed display.
    pub started: bool,
}

/// Length of `buf`'s longest prefix that ends on a UTF-8 boundary —
/// everything except a possibly incomplete trailing multi-byte sequence.
pub(super) fn utf8_complete_len(buf: &[u8]) -> usize {
    let mut valid = 0;
    loop {
        match std::str::from_utf8(&buf[valid..]) {
            Ok(_) => return buf.len(),
            Err(error) => {
                valid += error.valid_up_to();
                match error.error_len() {
                    Some(bad) => valid += bad,
                    None => return valid,
                }
            }
        }
    }
}

/// How often live terminal output is read from the daemon while calls run.
const LIVE_OUTPUT_READ_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Base URL `<tunnel id>` tags resolve against in the TUI: the daemon's
/// loopback listener.
fn tunnel_base(config: &MayConfig) -> String {
    format!(
        "http://localhost:{}/tunnel",
        config.daemon.port.unwrap_or(crate::daemon::DEFAULT_PORT)
    )
}

/// The conversation rows of one turn seen so far while history is rebuilt.
#[derive(Default)]
struct HistoryTurn {
    started: Option<jiff::Timestamp>,
    last: Option<jiff::Timestamp>,
    /// The turn's latest row is a reply: a run ends with one.
    replied: bool,
}

impl HistoryTurn {
    fn note(&mut self, msg: &ChatMessage) {
        if !matches!(
            msg.role,
            MessageRole::User | MessageRole::Assistant | MessageRole::Tool
        ) {
            return;
        }
        if let Some(at) = msg.timestamp {
            self.started.get_or_insert(at);
            self.last = Some(at);
        }
        self.replied = msg.role == MessageRole::Assistant;
    }

    /// Whole seconds from the turn's first row to its last.
    fn seconds(&self) -> u64 {
        match (self.started, self.last) {
            (Some(started), Some(last)) => {
                u64::try_from(last.duration_since(started).as_secs()).unwrap_or(0)
            }
            _ => 0,
        }
    }
}

impl App {
    /// Create a new application.
    /// `image_support` should be detected before entering raw mode using `TerminalImageSupport::detect()`.
    ///
    /// # Errors
    ///
    /// Returns [`TuiError`] if the May configuration cannot be loaded or the session
    /// manager cannot be initialized.
    pub async fn new(
        event_tx: EventSender,
        image_support: Option<super::ui::image::TerminalImageSupport>,
    ) -> AppResult<Self> {
        let mut config = MayConfig::load().await.map_err(TuiError::LoadConfig)?;
        let session_manager = SessionManager::new().await?;

        // Auto-detect API keys from environment variables
        let mut env_providers_added = Vec::new();
        for provider_type in ProviderType::ALL {
            if let Some((key, var_name)) = detect_api_key_from_env(*provider_type) {
                let provider_id = provider_type.short_name();
                // Only add if not already configured
                if !config.provider.contains_key(provider_id) {
                    let mut instance = crate::config::ProviderInstance::new(*provider_type);
                    instance.api_key = Some(key);
                    config.upsert_provider(provider_id, instance);
                    env_providers_added.push((provider_id.to_string(), var_name));
                }
            }
        }

        // If we added providers from env, populate models and save
        if !env_providers_added.is_empty() {
            // Auto-populate models if empty
            if config.models.is_empty() {
                let available = config.available_models();
                for model in available {
                    if let Some(tier) = model.tier {
                        config.models.add_model(tier, &model.id, &model.provider_id);
                    }
                }
            }
            if let Err(e) = config.clone().save().await {
                tracing::error!("Failed to save config after env detection: {}", e);
            }
        }

        // Build status message for env-detected providers
        let status = if env_providers_added.is_empty() {
            None
        } else {
            let names: Vec<_> = env_providers_added
                .iter()
                .map(|(id, _)| id.as_str())
                .collect();
            Some(StatusNotice::transient(
                format!("Auto-configured from env: {}", names.join(", ")),
                STATUS_NOTICE_TTL,
            ))
        };

        Ok(Self::with_parts(
            config,
            session_manager,
            event_tx,
            image_support,
            status,
        ))
    }

    /// Builds the application state around an already loaded configuration
    /// and session manager; the part of [`Self::new`] that touches neither
    /// the home directory nor the environment.
    fn with_parts(
        config: MayConfig,
        session_manager: SessionManager,
        event_tx: EventSender,
        image_support: Option<super::ui::image::TerminalImageSupport>,
        status: Option<StatusNotice>,
    ) -> Self {
        let needs_setup = !config.is_configured();
        let tunnel_base = tunnel_base(&config);
        // Always start in Chat mode - show inline message if not configured
        let mode = AppMode::Chat;

        // Initialize config state
        let mut config_state = ConfigState::default();
        config_state.load_providers(&config);

        // Profiles load once the daemon is attached (`load_profiles`).
        let profiles = Vec::new();
        let current_profile_id = None;

        // Load input history
        let history = InputHistory::new(session_manager.may_dir());

        // Initialize completion
        let completion = Completion::new();

        // Get current working directory
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        let secrets = crate::secrets::Secrets::load(
            &crate::secrets::SecretStore::new(session_manager.may_dir()),
            &config,
        );

        Self {
            session_manager,
            worker_settings: crate::agent::AgentLiveSettings::of(&config),
            config,
            input: String::new(),
            cursor: 0,
            input_char_count: 0,
            prompt_suggestion: None,
            attachments: Vec::new(),
            chat: ChatStream::new().with_tunnel_base(tunnel_base),
            scroll: usize::MAX, // Follow bottom by default
            input_visible: true,
            content_height: AtomicUsize::new(0),
            view_height: AtomicUsize::new(0),
            mode,
            rewind_notice: None,
            sent_hidden_context: None,
            queue: MessageQueue::new(),
            status,
            processing: false,
            file_picker: None,
            image_manager: None,
            text_manager: None,
            history,
            completion,
            session_list: Vec::new(),
            session_selected: 0,
            session_search: String::new(),
            session_scroll: 0,
            selected_tool: None,
            selected_tool_index: 0,
            tool_detail_scroll: 0,
            help_scroll: 0,
            config_state,
            model_select_index: 0,
            model_select_list: Vec::new(),
            model_select_query: String::new(),
            model_catalog: Vec::new(),
            effort_ladder: Vec::new(),
            family_effort_rungs: std::collections::HashMap::new(),
            turn_started_at: None,
            effort_index: 0,
            current_effort: None,
            needs_setup,
            cwd,
            quit_pending: false,
            last_esc_at: None,
            thinking_start: None,
            copilot_auth_status: None,
            chatgpt_auth_status: None,
            grok_auth_status: None,
            openrouter_auth_status: None,
            login_selected: 0,
            auth_return: AuthReturn::Config,
            tier_picker_tier: None,
            tier_picker_models: Vec::new(),
            tier_picker_filtered: Vec::new(),
            tier_picker_selected: 0,
            tier_picker_query: String::new(),
            tier_picker_loading: false,
            model_fetch_rx: None,
            model_select_rx: None,
            file_diffs: None,
            clone_roots: async_channel::unbounded(),
            file_status: Vec::new(),
            sidebar_file_changes: true,
            clipboard_rx: None,
            pending_paste_results: std::collections::VecDeque::new(),
            pending_attachment_jobs: 0,
            usage: None,
            decode_meter: crate::decode_meter::DecodeMeter::default(),
            new_year: crate::tui::new_year::NewYearState::default(),
            spinner_frame: 0,
            activity_phase: crate::tui::ui::chat::ActivityPhase::Thinking,
            context_window: None,
            current_model: None,
            mcp_servers: Vec::new(),
            mcp_selected: 0,
            mcp_add_mode: McpAddMode::None,
            mcp_add_name: String::new(),
            mcp_add_command: String::new(),
            ssh_selected: 0,
            ssh_add_mode: SshAddMode::None,
            ssh_add_name: String::new(),
            ssh_add_target: String::new(),
            skill_list: Vec::new(),
            skill_selected: 0,
            skill_scroll: 0,
            skill_add_mode: SkillAddMode::None,
            skill_github_input: String::new(),
            skill_search: String::new(),

            profiles,
            current_profile_id,
            profile_sessions: None,
            profile_selected: 0,
            session_view: super::state::SessionView::default(),
            profile_follow: None,

            event_tx,
            image_support,
            image_cache: ahash::AHashMap::new(),
            image_fetching: ahash::AHashSet::new(),
            image_failed: ahash::AHashSet::new(),
            image_regions: Vec::new(),
            image_view_area: None,
            selection: None,
            hover: None,
            link_opener: std::sync::Arc::new(super::ui::links::SystemOpener),
            chat_layout: super::ui::chat_layout::ChatLayout::default(),
            frame_stats: super::perf::FrameStats::default(),
            last_click: None,
            background: None,
            btw_daemon: None,
            sidebar: super::sidebar::SidebarState::default(),
            chat_area: ratatui::layout::Rect::default(),
            band_bg: None,
            jobs: Vec::new(),
            jobs_selected: 0,
            subagents: Vec::new(),
            live_output_read_at: None,
            run_guard: None,
            live_reads: std::collections::HashMap::new(),
            pending_tool_calls: std::collections::HashMap::new(),
            diff_preview_at: None,
            secrets,
            open_question_count: 0,
            session_runtime: crate::client::SessionRuntimeSnapshot::default(),
            shown_permission_requests: std::collections::HashSet::new(),
            index_coverage: None,
            asleep: false,
            sleep_reminders: None,
            index_failed: false,
            index_loading: false,
            question_cursor: 0,
            parked_question: None,
        }
    }

    fn persist_config_background(&self) {
        let tx = self.event_tx.clone();
        let config = self.config.clone();
        spawn(async move {
            let result = config.save().await.map_err(TuiError::from);
            let _ = tx.send(AppEvent::ConfigSaved(result)).await;
        })
        .detach();
    }

    /// Create or rebuild the file-diff worker for the current session.
    ///
    /// Clone-based trust tracks session clones only (sidebar hides Changes).
    /// Direct-checkout trust tracks this worktree and excludes nested git
    /// worktrees. Container-profile sessions are skipped: their writes land
    /// inside the container where host-side git cannot see them.
    fn ensure_file_diffs(&mut self) {
        // An ephemeral session has no dirs on disk yet; its filebase is
        // created when the session materializes.
        let Some(session) = self
            .session_manager
            .current()
            .filter(|session| session.persisted)
        else {
            self.file_diffs = None;
            self.file_status.clear();
            return;
        };
        let session_id = session.id.as_str().to_string();
        if self
            .file_diffs
            .as_ref()
            .is_some_and(|handle| handle.session_id() == session_id)
        {
            return;
        }

        let is_container = session
            .profile_id
            .as_deref()
            .or(self.current_profile_id.as_deref())
            .and_then(|pid| self.profiles.iter().find(|profile| profile.id == pid))
            .is_some_and(|profile| {
                matches!(
                    profile.environment.kind(),
                    crate::profile::ProfileEnvironment::Container { .. }
                )
            });
        if is_container {
            self.file_diffs = None;
            self.file_status.clear();
            return;
        }

        let session_paths = crate::session::SessionPaths::new(session.sandbox_dir.clone());
        let config =
            crate::workspace::WorkspaceConfig::load(&session_paths.workspace_config()).ok();
        let trust = config.as_ref().and_then(|config| {
            let worktree = crate::repo_search::worktree_root(&self.cwd)?;
            let key = crate::issue::repository_identity(&worktree).ok()?;
            crate::trust::trust_for(config, &key, &worktree)
        });
        let clones_work = trust.is_some_and(crate::trust::TrustLevel::clones_work);
        self.sidebar_file_changes = !clones_work;
        // With clones, the daemon's registry names the roots; they join
        // tracking when its answer arrives.
        let mut roots = Vec::new();
        if !clones_work {
            if let Some(config) = &config {
                if !config.has_write_access(&self.cwd) {
                    roots.push(crate::file_tracker::TrackedRootSpec::checkout(
                        self.cwd.clone(),
                    ));
                }
                roots.extend(
                    config
                        .workdirs
                        .iter()
                        .filter(|dir| dir.permission == crate::workspace::Permission::ReadWrite)
                        .map(|dir| {
                            crate::file_tracker::TrackedRootSpec::checkout(dir.path.clone())
                        }),
                );
            } else {
                roots.push(crate::file_tracker::TrackedRootSpec::checkout(
                    self.cwd.clone(),
                ));
            }
        }
        self.file_status.clear();
        self.file_diffs = FileDiffHandle::start(
            session_id.clone(),
            session_paths.root().join("filebase"),
            roots,
        );
        if let Some(handle) = &self.file_diffs {
            handle.request_status();
        }
        if clones_work {
            self.request_clone_roots(&session_id);
        }
    }

    /// Ask the daemon for the session's repo clones; its answer is applied by
    /// [`Self::poll_file_diffs`].
    fn request_clone_roots(&self, session_id: &str) {
        let Ok(client) = self.session_manager.daemon_handle() else {
            return;
        };
        let results = self.clone_roots.0.clone();
        let path = format!("/api/sessions/{session_id}/repo-clones");
        spawn(async move {
            match client.get::<Vec<crate::repo_clone::CloneView>>(&path).await {
                Ok(clones) => {
                    let specs = clones
                        .into_iter()
                        .map(|clone| {
                            let label = if clone.local {
                                crate::fs_utils::display_path(std::path::Path::new(&clone.source))
                            } else {
                                clone.source.clone()
                            };
                            crate::file_tracker::TrackedRootSpec::new(clone.path.into())
                                .label(format!("{label} (clone)"))
                        })
                        .collect();
                    let _ = results.send(specs).await;
                }
                Err(error) => tracing::warn!(%error, "failed to list session repo clones"),
            }
        })
        .detach();
    }

    /// Ask the diff worker to attribute pending file changes to
    /// `tool_call_id`.
    fn request_file_diff(&mut self, tool_call_id: &str) {
        self.ensure_file_diffs();
        if let Some(handle) = &self.file_diffs {
            handle.request_snapshot(tool_call_id);
        }
    }

    /// Record the current tree of every tracked root as the checkpoint
    /// for the prompt being dispatched — the state "before this prompt"
    /// that a conversation-and-code rewind restores.
    pub fn request_prompt_mark(&mut self, message_db_id: i64) {
        self.ensure_file_diffs();
        if let Some(handle) = &self.file_diffs {
            handle.request_mark(&format!("prompt:{message_db_id}"));
        }
    }

    /// Ask the diff worker to restore every tracked root to the tree
    /// recorded when the prompt's turn started — turn-granular rewind.
    pub fn request_prompt_rewind(&mut self, message_db_id: i64) {
        self.ensure_file_diffs();
        if let Some(handle) = &self.file_diffs {
            handle.request_rewind(&format!("prompt:{message_db_id}"));
        }
    }

    /// Open the rewind picker listing the session's user prompts.
    /// Returns `false` when there is nothing to rewind to.
    pub fn open_rewind_picker(&mut self) -> bool {
        let targets = self.chat.rewind_targets();
        if targets.is_empty() {
            return false;
        }
        let selected = targets.len();
        self.mode = AppMode::Rewind(crate::tui::state::RewindState {
            targets,
            // The trailing "(current)" entry is selected first.
            selected,
        });
        true
    }

    /// Begin tracking a workspace directory the user just granted access
    /// to. The baseline is taken at grant time, so only later writes are
    /// attributed to the agent.
    pub fn track_workspace_dir(&mut self, path: &std::path::Path) {
        self.ensure_file_diffs();
        if let Some(handle) = &self.file_diffs {
            handle.request_add_root(crate::file_tracker::TrackedRootSpec::new(
                path.to_path_buf(),
            ));
        }
    }

    /// Apply finished file diffs and rewind results to their tool-call
    /// blocks. Called once per event-loop iteration.
    pub fn poll_file_diffs(&mut self) {
        while let Ok(specs) = self.clone_roots.1.try_recv() {
            if let Some(handle) = &self.file_diffs {
                for spec in specs {
                    handle.request_add_root(spec);
                }
            }
        }
        let outcomes = match &self.file_diffs {
            Some(handle) => handle.drain(),
            None => return,
        };
        let mut refresh_status = false;
        for outcome in outcomes {
            match outcome {
                super::file_diff::Outcome::Diff {
                    tool_call_id,
                    diffs,
                    final_snapshot,
                } => {
                    // A preview only applies while its call still runs — a
                    // late preview must not clobber the final snapshot.
                    if final_snapshot || self.chat.is_tool_running(&tool_call_id) {
                        self.chat.update_tool_diff(&tool_call_id, diffs);
                    }
                    refresh_status = true;
                }
                super::file_diff::Outcome::Rewound { diffs, .. } => {
                    self.apply_rewind_result(&diffs);
                    refresh_status = true;
                }
                super::file_diff::Outcome::Status { diffs } => {
                    self.file_status = diffs;
                }
            }
        }
        if refresh_status && let Some(handle) = &self.file_diffs {
            // Repo clones created since the last refresh join tracking;
            // the worker dedupes roots it already covers.
            self.request_clone_roots(handle.session_id());
            handle.request_status();
        }
    }

    /// Show the result of a completed code rewind in the chat stream.
    fn apply_rewind_result(&mut self, diffs: &[crate::file_tracker::RootDiff]) {
        if diffs.is_empty() {
            self.chat.push_notice(
                "Rewind: files unchanged — no code checkpoint applies or files already match",
            );
            return;
        }
        let files: usize = diffs.iter().map(|d| d.changes.len()).sum();
        let additions: usize = diffs.iter().map(|d| d.additions).sum();
        let deletions: usize = diffs.iter().map(|d| d.deletions).sum();
        self.chat.push_notice(&format!(
            "⏪ restored {files} file{} (+{additions} −{deletions})",
            if files == 1 { "" } else { "s" },
        ));
    }

    /// Re-fetch the session's background jobs from the registry.
    pub fn refresh_jobs(&mut self) {
        let Some(background) = self.background.clone() else {
            return;
        };
        let tx = self.event_tx.clone();
        spawn(async move {
            match background.snapshot().await {
                Ok((jobs, subagents)) => {
                    let _ = tx
                        .send(AppEvent::BackgroundLoaded { jobs, subagents })
                        .await;
                }
                Err(error) => tracing::debug!(%error, "failed to load background jobs"),
            }
        })
        .detach();
    }

    /// Cancel one subagent run; the daemon matches it to the `subagent`
    /// command's own registry.
    pub fn cancel_subagent(&mut self, run: &crate::agent::SubagentRunSnapshot) {
        let Some(background) = self.background.clone() else {
            self.set_status(format!("No live registry entry for subagent {}", run.agent));
            return;
        };
        self.set_status(format!("Stopping subagent {}…", run.agent));
        let (run_id, agent) = (run.run_id, run.agent.clone());
        let tx = self.event_tx.clone();
        spawn(async move {
            let stopped = background.cancel(run_id).await.unwrap_or(0);
            if stopped == 0 {
                let _ = tx
                    .send(AppEvent::Status(format!(
                        "No live registry entry for subagent {agent}"
                    )))
                    .await;
            }
        })
        .detach();
    }

    /// Stops every subagent run of the session; `false` when none runs.
    pub fn cancel_all_subagents(&mut self) -> bool {
        let Some(background) = self.background.clone() else {
            return false;
        };
        let count = self.subagents.len();
        if count == 0 {
            return false;
        }
        self.set_status(format!(
            "Stopping {count} subagent{} — Esc again to abort turn",
            if count == 1 { "" } else { "s" }
        ));
        spawn(async move {
            if let Err(error) = background.cancel_all().await {
                tracing::warn!(%error, "failed to stop subagents");
            }
        })
        .detach();
        true
    }

    /// Kill a background job by pid, then refresh the panel.
    pub fn kill_job(&mut self, pid: u32) {
        let Some(background) = self.background.clone() else {
            return;
        };
        let tx = self.event_tx.clone();
        spawn(async move {
            let ok = background.kill(pid).await.unwrap_or(false);
            let _ = tx.send(AppEvent::JobKilled { pid, ok }).await;
            if let Ok((jobs, subagents)) = background.snapshot().await {
                let _ = tx
                    .send(AppEvent::BackgroundLoaded { jobs, subagents })
                    .await;
            }
        })
        .detach();
    }

    /// Open the `/btw` side-question overlay.
    pub fn open_btw(&mut self) {
        if self.btw_daemon.is_none() {
            self.chat
                .push_notice("No model available for /btw — start a session first");
            return;
        }
        self.mode = AppMode::Btw(super::state::BtwState::default());
    }

    /// Submit the question typed in the `/btw` overlay. Streams the answer
    /// back through [`AppEvent`]s; nothing touches the conversation history.
    pub fn submit_btw(&mut self) {
        let AppMode::Btw(state) = &self.mode else {
            return;
        };
        let question = state.input.trim().to_string();
        if question.is_empty() || state.loading {
            return;
        }
        let messages = self.btw_messages(&question);
        let tx = self.event_tx.clone();
        let (Some(client), Some(session)) = (
            self.btw_daemon.clone(),
            self.session_manager
                .current()
                .filter(|session| session.persisted),
        ) else {
            return;
        };
        let path = crate::client::session_path(session.id.as_str(), "/actions/btw");
        if let AppMode::Btw(state) = &mut self.mode {
            state.loading = true;
        }
        spawn(async move {
            let answer: Result<crate::daemon::BtwResponse, _> = client
                .post(&path, &crate::daemon::BtwRequest { messages })
                .await;
            match answer {
                Ok(answer) => {
                    if tx.send(AppEvent::BtwDelta(answer.answer)).await.is_ok() {
                        let _ = tx.send(AppEvent::BtwFinished).await;
                    }
                }
                Err(error) => {
                    let _ = tx.send(AppEvent::BtwFailed(error.to_string())).await;
                }
            }
        })
        .detach();
    }

    /// Open the `/progress` overlay and ask the daemon for the remaining
    /// work. The request runs on the side: nothing touches the conversation.
    pub fn open_progress(&mut self) {
        let (Some(client), Some(session)) = (
            self.btw_daemon.clone(),
            self.session_manager
                .current()
                .filter(|session| session.persisted),
        ) else {
            self.chat
                .push_notice("No model available for /progress — start a session first");
            return;
        };
        let path = crate::client::session_path(session.id.as_str(), "/actions/progress");
        self.mode = AppMode::Progress(super::state::ProgressState {
            loading: true,
            ..super::state::ProgressState::default()
        });
        let tx = self.event_tx.clone();
        spawn(async move {
            let report: Result<crate::daemon::ProgressReport, _> =
                client.post(&path, &serde_json::json!({})).await;
            let event = match report {
                Ok(report) => AppEvent::ProgressLoaded(report),
                Err(error) => AppEvent::ProgressFailed(error.to_string()),
            };
            let _ = tx.send(event).await;
        })
        .detach();
    }

    /// System prompt for `/btw`: answer once, no tools, no side effects.
    const BTW_SYSTEM: &'static str = "This is a side question from the user. \
        Answer it directly in a single response. You share the conversation \
        context but are a separate, lightweight instance — the main agent is \
        NOT interrupted and keeps working. You have NO tools available and \
        there will be no follow-up turns: never promise to take any action.";

    /// Chat elements the `/btw` request sees, oldest first. Bounded to the
    /// most recent entries so the overlay stays cheap on long sessions.
    const BTW_CONTEXT_ELEMENTS: usize = 24;

    /// Build the `/btw` request: system contract, recent chat as context,
    /// then the question itself.
    fn btw_messages(&self, question: &str) -> Vec<crate::daemon::BtwMessage> {
        use super::ui::chat::{ChatElement, ToolDisplay};

        let mut messages = vec![btw_message("system", Self::BTW_SYSTEM)];
        for element in self
            .chat
            .elements()
            .iter()
            .rev()
            .take(Self::BTW_CONTEXT_ELEMENTS)
        {
            let message = match element {
                ChatElement::UserMessage { content, .. } => Some(btw_message("user", content)),
                ChatElement::AssistantText { content, .. } => {
                    (!content.is_empty()).then(|| btw_message("assistant", content))
                }
                ChatElement::ToolCall { display, .. } => {
                    let summary = match display {
                        ToolDisplay::Terminal {
                            description,
                            command,
                            output,
                            ..
                        } => {
                            let lead = if description.is_empty() {
                                "ran terminal command".to_string()
                            } else {
                                format!("ran terminal command ({description})")
                            };
                            let mut text = format!("{lead}:\n{command}");
                            if let Some(info) = output
                                && !info.output.is_empty()
                            {
                                let tail: String = info.output.chars().take(600).collect();
                                text.push_str("\noutput:\n");
                                text.push_str(&tail);
                            }
                            text
                        }
                        ToolDisplay::Action {
                            chip,
                            summary,
                            output,
                            ..
                        } => {
                            let mut text = format!("called {chip}: {summary}");
                            if let Some(info) = output
                                && !info.output.is_empty()
                            {
                                let tail: String = info.output.chars().take(600).collect();
                                text.push_str("\noutput:\n");
                                text.push_str(&tail);
                            }
                            text
                        }
                        ToolDisplay::Builtin { tool, detail, .. } => {
                            format!("hosted tool {tool}: {detail}")
                        }
                    };
                    Some(btw_message("assistant", summary))
                }
                _ => None,
            };
            if let Some(message) = message {
                messages.push(message);
            }
        }
        // History was gathered newest-first; flip it back to chronological.
        let (system, history) = messages.split_first().expect("system message present");
        let mut ordered = vec![system.clone()];
        ordered.extend(history.iter().rev().cloned());
        ordered.push(btw_message("user", question));
        ordered
    }

    /// The subagent run `run_id`, while it runs.
    fn subagent(&self, run_id: u64) -> Option<crate::agent::SubagentRunSnapshot> {
        self.subagents
            .iter()
            .find(|run| run.run_id == run_id)
            .cloned()
    }

    /// Open the jobs/subagents panel.
    pub fn open_jobs(&mut self) {
        if self.background.is_none() {
            self.chat
                .push_error("No active session — jobs list unavailable");
            return;
        }
        self.mode = AppMode::Jobs;
        self.jobs_selected = 0;
        self.refresh_jobs();
    }

    /// Poll live output and file-diff previews for running terminal calls —
    /// the streaming half of tool-call rendering. Runs on each tick while
    /// the agent processes.
    pub fn poll_live_output(&mut self) {
        // Drop bindings whose call left the Running state (end, failure,
        // interrupt); the remaining reads still refresh on status.
        self.live_reads
            .retain(|call_id, _| self.chat.is_tool_running(call_id));

        if let Some(background) = self.background.clone() {
            // Bind each unbound running call to the newest unbound running
            // job executing the same script. Two concurrent calls with an
            // identical script bind in order of `started_at`.
            let mut bound: std::collections::HashSet<String> = self
                .live_reads
                .values()
                .map(|read| read.task_id.clone())
                .collect();
            for (call_id, command) in self.chat.running_terminal_calls() {
                if self.live_reads.contains_key(&call_id) {
                    continue;
                }
                let job = self
                    .jobs
                    .iter()
                    .filter(|job| {
                        matches!(job.status, aither_sandbox::JobStatus::Running)
                            && job.script.trim() == command.trim()
                            && !bound.contains(&job.task_id)
                    })
                    .max_by_key(|job| job.started_at);
                if let Some(job) = job {
                    bound.insert(job.task_id.clone());
                    self.live_reads.insert(
                        call_id,
                        LiveRead {
                            task_id: job.task_id.clone(),
                            cursor: 0,
                            pending: Vec::new(),
                        },
                    );
                }
            }

            // Reads cross the daemon's socket; a few a second keep the
            // output live.
            let read_due = self
                .live_output_read_at
                .is_none_or(|at| at.elapsed() >= LIVE_OUTPUT_READ_INTERVAL);
            if !self.live_reads.is_empty() && read_due {
                self.live_output_read_at = Some(std::time::Instant::now());
                let tx = self.event_tx.clone();
                let reads: Vec<(String, String, usize)> = self
                    .live_reads
                    .iter()
                    .map(|(call_id, read)| (call_id.clone(), read.task_id.clone(), read.cursor))
                    .collect();
                spawn(async move {
                    let mut deltas = Vec::with_capacity(reads.len());
                    for (call_id, task_id, cursor) in reads {
                        if let Ok(delta) = background.read(&task_id, cursor).await {
                            deltas.push(super::event::TerminalOutputDelta {
                                call_id,
                                cursor: delta.cursor,
                                bytes: delta.bytes,
                                status: delta.status.into(),
                            });
                        }
                    }
                    let _ = tx.send(AppEvent::TerminalOutputs(deltas)).await;
                })
                .detach();
            }
        }

        // Live file-diff previews at ~1s cadence while calls run.
        if self
            .diff_preview_at
            .is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(1))
        {
            self.ensure_file_diffs();
            if let Some(handle) = &self.file_diffs {
                for (call_id, _) in self.chat.running_terminal_calls() {
                    handle.request_preview(&call_id);
                }
            }
            self.diff_preview_at = Some(std::time::Instant::now());
        }
    }

    /// Open the detail view for one jobs-panel row.
    pub fn open_job_detail(&mut self, target: crate::tui::state::JobDetailTarget) {
        let mut state = crate::tui::state::JobDetailState::new(target);
        match target {
            crate::tui::state::JobDetailTarget::Job(pid) => {
                state.job = self.jobs.iter().find(|job| job.pid == pid).cloned();
                state.finished = state.job.is_none();
            }
            crate::tui::state::JobDetailTarget::Subagent(run_id) => {
                state.subagent = self.subagent(run_id);
                state.finished = state.subagent.is_none();
            }
        }
        self.mode = AppMode::JobDetail(state);
        self.refresh_job_detail();
    }

    /// Refresh the data behind an open `AppMode::JobDetail` view.
    ///
    /// `Job` targets fetch through the registry (async, lands via
    /// `AppEvent::JobDetailLoaded`); `Subagent` targets re-read the shared
    /// tracker synchronously.
    pub fn refresh_job_detail(&mut self) {
        let AppMode::JobDetail(state) = &mut self.mode else {
            return;
        };
        let target = state.target;
        match target {
            crate::tui::state::JobDetailTarget::Job(pid) => {
                let Some(background) = self.background.clone() else {
                    state.finished = true;
                    return;
                };
                let tx = self.event_tx.clone();
                spawn(async move {
                    let (job, output) = background.job(pid).await.unwrap_or((None, None));
                    let _ = tx.send(AppEvent::JobDetailLoaded { job, output }).await;
                })
                .detach();
            }
            crate::tui::state::JobDetailTarget::Subagent(run_id) => {
                match self
                    .subagents
                    .iter()
                    .find(|run| run.run_id == run_id)
                    .cloned()
                {
                    Some(snapshot) => {
                        state.subagent = Some(snapshot);
                        state.finished = false;
                    }
                    None => state.finished = true,
                }
            }
        }
        state.refreshed = Some(std::time::Instant::now());
    }

    /// Check if configuration is complete.
    pub fn is_configured(&self) -> bool {
        self.config.is_configured()
    }

    /// Request fetching an image from URL if not already cached or fetching.
    /// Returns true if the image is already cached.
    pub fn request_image(&mut self, url: &str) -> bool {
        // Already cached?
        if self.image_cache.contains_key(url) {
            return true;
        }
        // Already fetching or already failed?
        if self.image_fetching.contains(url) || self.image_failed.contains(url) {
            return false;
        }
        // Check if terminal supports images
        if self.image_support.is_none() {
            return false;
        }
        let targets = self.image_fetch_targets(url);
        self.spawn_image_fetch(url.to_string(), targets);
        false
    }

    /// Resolve an image reference to fetch candidates. Remote and absolute
    /// references map to themselves; a sandbox-relative path (for example
    /// `artifacts/x.png` from a tool result) is resolved against the session
    /// sandbox directory first, then the process working directory.
    fn image_fetch_targets(&self, url: &str) -> Vec<String> {
        if url.starts_with("http://")
            || url.starts_with("https://")
            || url.starts_with("file://")
            || std::path::Path::new(url).is_absolute()
        {
            return vec![url.to_string()];
        }
        let mut targets = Vec::with_capacity(2);
        if let Some(session) = self.session_manager.current() {
            targets.push(session.sandbox_dir.join(url).to_string_lossy().into_owned());
        }
        targets.push(url.to_string());
        targets
    }

    /// Spawn a background task trying each candidate target in order,
    /// decoding the fetched bytes into an [`ImageState`] off the render
    /// thread and reporting it under `key`.
    fn spawn_image_fetch(&mut self, key: String, targets: Vec<String>) {
        let Some(ref support) = self.image_support else {
            return;
        };
        self.image_fetching.insert(key.clone());
        let picker = support.picker().clone();
        let tx = self.event_tx.clone();
        tracing::debug!("Starting image fetch: {} (targets: {:?})", key, targets);
        spawn(async move {
            let mut data = None;
            for target in &targets {
                if let Some(bytes) = super::ui::image::fetch_image(target).await {
                    data = Some(bytes);
                    break;
                }
            }
            if let Some(ref bytes) = data {
                tracing::debug!("Image fetch succeeded: {} ({} bytes)", key, bytes.len());
            } else {
                tracing::debug!("Image fetch returned None: {}", key);
            }
            let state = match data {
                Some(bytes) => {
                    unblock(move || super::ui::image::decode_image_state(&picker, &bytes)).await
                }
                None => None,
            };
            if let Err(e) = tx.send(AppEvent::ImageDecoded { key, state }).await {
                tracing::warn!("Failed to send ImageDecoded event: {}", e);
            }
        })
        .detach();
    }

    /// Decode image bytes into an [`ImageState`] on a worker thread and
    /// deliver it through the event channel under `key`. No-op when the
    /// terminal lacks graphics support, the key is already cached or in
    /// flight, or a previous attempt failed.
    fn spawn_image_decode(&mut self, key: String, data: Vec<u8>) {
        let Some(ref support) = self.image_support else {
            return;
        };
        if self.image_cache.contains_key(&key)
            || self.image_failed.contains(&key)
            || !self.image_fetching.insert(key.clone())
        {
            return;
        }
        let picker = support.picker().clone();
        let tx = self.event_tx.clone();
        spawn(async move {
            let state = unblock(move || super::ui::image::decode_image_state(&picker, &data)).await;
            if let Err(e) = tx.send(AppEvent::ImageDecoded { key, state }).await {
                tracing::warn!("Failed to send ImageDecoded event: {}", e);
            }
        })
        .detach();
    }

    /// Handle a finished image fetch/decode: cache the renderable state or
    /// mark the key failed so it is not retried.
    pub(super) fn handle_image_decoded(
        &mut self,
        key: String,
        state: Option<super::ui::image::ImageState>,
    ) {
        self.image_fetching.remove(&key);
        if let Some(state) = state {
            tracing::debug!("Image decoded: {}", key);
            self.image_cache.insert(key, state);
        } else {
            tracing::debug!("Image fetch/decode failed for {}", key);
            self.image_failed.insert(key);
        }
    }

    /// Decode a tool-produced image into the cache (or queue its fetch) and
    /// return the [`ToolCallImage`] to attach to the call's display info.
    fn install_tool_image(
        &mut self,
        tool_call_id: &str,
        image: super::agent_event_adapter::ToolImage,
    ) -> Option<super::ui::chat::ToolCallImage> {
        use super::agent_event_adapter::ToolImage as Kind;
        self.image_support.as_ref()?;
        let key = format!("tool-image:{tool_call_id}");
        match image {
            Kind::Bytes(bytes) => {
                self.spawn_image_decode(key.clone(), bytes);
                Some(super::ui::chat::ToolCallImage {
                    key,
                    label: "image".to_string(),
                    path: String::new(),
                })
            }
            Kind::Stored { url, path } => {
                let label = std::path::Path::new(if path.is_empty() { &url } else { &path })
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("image")
                    .to_string();
                let caption_path = if path.is_empty() {
                    url.clone()
                } else {
                    path.clone()
                };
                let mut targets = if url.is_empty() {
                    Vec::new()
                } else {
                    self.image_fetch_targets(&url)
                };
                if !path.is_empty() {
                    targets.push(path);
                }
                if targets.is_empty() {
                    return None;
                }
                self.spawn_image_fetch(key.clone(), targets);
                Some(super::ui::chat::ToolCallImage {
                    key,
                    label,
                    path: caption_path,
                })
            }
        }
    }

    /// Resolve a background task id into a user-facing label: the job's
    /// script when the registry knows it, else the description (or command)
    /// of the call whose output introduced the id — the `terminal` result
    /// that named it. The id itself is never surfaced.
    fn resolve_task_label(&self, task_id: &str) -> Option<String> {
        use super::ui::chat::ChatElement;
        if task_id.is_empty() {
            return None;
        }
        if let Some(job) = self.jobs.iter().find(|job| job.task_id == task_id) {
            return job.script.lines().next().map(str::to_string);
        }
        self.chat.elements().iter().find_map(|element| {
            let ChatElement::ToolCall {
                display:
                    ToolDisplay::Terminal {
                        description,
                        command,
                        output,
                        ..
                    },
                ..
            } = element
            else {
                return None;
            };
            let info = output.as_ref()?;
            info.output.contains(task_id).then(|| {
                if description.is_empty() {
                    command.lines().next().unwrap_or_default().to_string()
                } else {
                    description.clone()
                }
            })
        })
    }

    /// Recover an image produced by the `imagine` command: it writes into
    /// the session artifacts dir and replies with a bare relative path.
    fn imagine_result_image(
        &self,
        tool_call_id: &str,
        output: &str,
    ) -> Option<super::agent_event_adapter::ToolImage> {
        let Some(ToolDisplay::Terminal { command, .. }) = self.chat.get_tool(tool_call_id) else {
            return None;
        };
        if command.split_whitespace().next() != Some("imagine") {
            return None;
        }
        let trimmed = output.trim();
        if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
            return None;
        }
        let is_image = mime_guess::from_path(trimmed)
            .first()
            .is_some_and(|mime| mime.type_() == mime::IMAGE);
        if !is_image {
            return None;
        }
        Some(super::agent_event_adapter::ToolImage::Stored {
            url: trimmed.to_string(),
            path: trimmed.to_string(),
        })
    }

    /// Check if terminal supports image rendering.
    pub const fn supports_images(&self) -> bool {
        self.image_support.is_some()
    }

    /// Get cached image state for a URL.
    pub fn get_cached_image(&self, url: &str) -> Option<&super::ui::image::ImageState> {
        self.image_cache.get(url)
    }

    /// Scan chat content for images and request fetches for any not yet cached.
    /// Also decodes in-hand attachment bytes — pending paste previews and
    /// user-message images — into the cache.
    pub fn scan_and_fetch_images(&mut self) {
        use super::ui::chat::ChatElement;

        // Markdown image references of the laid-out transcript (last
        // frame's layout; a new reference is fetched one frame later). The
        // layout already parsed them, so nothing is re-parsed here.
        let urls: Vec<String> = self
            .chat_layout
            .markdown_image_keys()
            .filter(|key| {
                !self.image_cache.contains_key(*key)
                    && !self.image_failed.contains(*key)
                    && !self.image_fetching.contains(*key)
            })
            .map(str::to_string)
            .collect();

        for url in urls {
            self.request_image(&url);
        }

        // Seed the cache directly from attachment bytes already in hand —
        // pending paste attachments and image attachments on user messages.
        let mut seeds: Vec<(String, Vec<u8>)> = Vec::new();
        for attachment in
            self.attachments
                .iter()
                .chain(
                    self.chat
                        .active_elements()
                        .iter()
                        .flat_map(|elem| match elem {
                            ChatElement::UserMessage { attachments, .. } => attachments.as_slice(),
                            _ => &[],
                        }),
                )
        {
            if let Attachment::Image { path, data } = attachment {
                let key = path.display().to_string();
                if !self.image_cache.contains_key(&key)
                    && !self.image_failed.contains(&key)
                    && !self.image_fetching.contains(&key)
                {
                    seeds.push((key, data.clone()));
                }
            }
        }
        for (key, data) in seeds {
            self.spawn_image_decode(key, data);
        }
    }

    /// Rebuilds the chat view from `messages` the way the live turns built
    /// it: tool calls replay from their stored lifecycle rows, and every
    /// finished turn folds into its "Worked for" row. The last turn folds
    /// only when its reply is stored; until then it may still be running.
    async fn render_history(&mut self, messages: &[ChatMessage]) {
        self.chat = ChatStream::new().with_tunnel_base(tunnel_base(&self.config));
        let mut turn = HistoryTurn::default();
        for (history_index, msg) in messages.iter().enumerate() {
            if msg.role == MessageRole::User {
                self.fold_history_turn(&turn);
                turn = HistoryTurn::default();
            }
            turn.note(msg);
            match msg.role {
                MessageRole::User => {
                    let daemon = self.session_manager.daemon_handle().ok();
                    let restored =
                        futures_util::future::join_all(msg.attachments.iter().map(|att| {
                            let daemon = daemon.clone();
                            async move {
                                super::runtime::tui_attachment_from_persisted(
                                    daemon.as_ref(),
                                    att.clone(),
                                )
                                .await
                                .map_err(|error| (att.name.clone(), error))
                            }
                        }))
                        .await;
                    let mut attachments = Vec::with_capacity(restored.len());
                    for result in restored {
                        match result {
                            Ok(attachment) => attachments.push(attachment),
                            Err((name, error)) => {
                                tracing::warn!("failed to restore attachment {name}: {error}");
                                attachments.push(Attachment::FileReference {
                                    path: PathBuf::from(&name),
                                });
                            }
                        }
                    }
                    self.chat
                        .push_user(&msg.content, attachments, history_index);
                }
                MessageRole::Assistant => {
                    self.chat.push_text(&msg.content);
                    self.chat.end_text();
                    if let Some(translation) = &msg.translation {
                        self.chat
                            .set_translation(translation.text.clone(), &translation.language);
                    }
                }
                MessageRole::Tool => {
                    match serde_json::from_str::<crate::session::ToolEventRecord>(&msg.content) {
                        Ok(crate::session::ToolEventRecord::Start {
                            id,
                            name,
                            arguments,
                        }) => {
                            // A stored call carries the arguments schema of
                            // the build that ran it, so it is read field by
                            // field rather than against today's schema.
                            self.show_tool_start(id, parse_tool_start_partial(&name, &arguments));
                        }
                        Ok(crate::session::ToolEventRecord::End { id, result, .. }) => {
                            self.show_tool_end(&id, &result);
                        }
                        Ok(crate::session::ToolEventRecord::Stopped { id, .. }) => {
                            self.show_tool_end(
                                &id,
                                &crate::session::ToolEventRecord::stopped_result(),
                            );
                        }
                        Err(error) => {
                            tracing::warn!(%error, row = ?msg.id, "unreadable tool row in history");
                        }
                    }
                }
                MessageRole::Recap => {
                    if let Ok(record) =
                        serde_json::from_str::<crate::daemon::RecapRecord>(&msg.content)
                    {
                        self.chat.push_recap(&record.summary, record.next_action);
                    }
                }
                _ => {}
            }
        }
        if turn.replied {
            self.fold_history_turn(&turn);
        }
    }

    /// Fold the finished turn `turn` of a history rebuild, as
    /// `finish_agent_task` folds a live one when its run ends.
    fn fold_history_turn(&mut self, turn: &HistoryTurn) {
        self.chat.finish_running_calls();
        self.chat.fold_turn_work(turn.seconds());
    }

    /// Re-reads the current session from the database and rebuilds the chat
    /// view, keeping what the user is typing: a turn another client or the
    /// daemon's queue ran wrote rows this view has not seen.
    pub async fn reload_chat(&mut self) {
        if let Err(error) = self.session_manager.refresh_history().await {
            tracing::warn!(%error, "failed to reload the session history");
            return;
        }
        let messages = self
            .session_manager
            .current()
            .map(|session| session.history.messages().to_vec())
            .unwrap_or_default();
        self.render_history(&messages).await;
        self.scroll = usize::MAX;
    }

    /// Initialize for a session (sets up image manager and status).
    ///
    /// Takes the session ID and a slice of messages to avoid borrow conflicts
    /// with the session manager.
    ///
    /// # Panics
    ///
    /// Panics if the session manager has no current session while a session is
    /// being initialized.
    pub async fn init_session(&mut self, session_id: &SessionId, messages: &[ChatMessage]) {
        // Reset per-session UI state so /new starts from a clean screen in inline viewport mode.
        self.input.clear();
        self.cursor = 0;
        self.input_char_count = 0;
        self.attachments.clear();
        self.completion.cancel();
        self.queue = MessageQueue::new();
        self.status = None;
        self.processing = false;
        self.selected_tool = None;
        self.selected_tool_index = 0;
        self.tool_detail_scroll = 0;
        self.quit_pending = false;
        self.thinking_start = None;
        self.content_height.store(0, Ordering::Relaxed);
        self.view_height.store(0, Ordering::Relaxed);
        self.selection = None;
        self.chat_layout = super::ui::chat_layout::ChatLayout::default();
        self.jobs.clear();
        self.jobs_selected = 0;
        self.session_runtime = crate::client::SessionRuntimeSnapshot::default();
        self.image_cache.clear();
        self.image_fetching.clear();
        self.image_failed.clear();
        self.background = None;
        self.subagents.clear();
        self.live_output_read_at = None;
        self.live_reads.clear();
        self.pending_tool_calls.clear();
        self.diff_preview_at = None;

        // Load chat history into display
        self.render_history(messages).await;

        let session_dir = self.session_manager.current().map_or_else(
            || {
                panic!(
                    "current session missing while initializing session {}",
                    session_id.as_str()
                )
            },
            |session| session.sandbox_dir.clone(),
        );
        self.image_manager = Some(ImageManager::new(&session_dir));
        self.text_manager = Some(TextManager::new(&session_dir));
        self.refresh_todos().await;

        // Reset usage tracking for new session
        self.usage = None;

        // The daemon sends the window with the session's next usage.
        self.context_window = None;
    }

    /// Handle an event. Returns Some(message) if user wants to send a message.
    ///
    /// # Errors
    ///
    /// Returns [`TuiError`] when a reducer action fails, such as copying the
    /// selection to the clipboard or running a session-management command.
    pub fn handle_event(&mut self, event: AppEvent) -> AppResult<Option<SendAction>> {
        super::reducer::reduce_event(self, event)
    }

    /// Show a prompt a run is blocked on. An open question being answered
    /// is parked with its draft, not discarded.
    pub fn show_blocking_prompt(&mut self, prompt: AppMode) {
        super::reducer::ask_user::show_blocking_prompt(
            &mut self.mode,
            &mut self.parked_question,
            prompt,
        );
    }

    /// The handler boundary: the outcome of a key, event or action handler.
    /// A failure of an ordinary interaction (a refused daemon request, a
    /// failed session operation) becomes a chat error and the TUI keeps
    /// running; `None` tells the caller the interaction produced nothing.
    /// Only failures that leave nothing to run (a terminal that cannot draw,
    /// no daemon at startup) are propagated by their callers instead.
    pub fn settle<T, E: std::fmt::Display>(&mut self, outcome: Result<T, E>) -> Option<T> {
        match outcome {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(%error, "interaction failed");
                self.chat.end_text();
                self.chat.push_error(&format!("{error:#}"));
                self.scroll = usize::MAX;
                None
            }
        }
    }

    /// Open the dialog for the permission request the session waits on, once
    /// per request: the runtime snapshot keeps listing a request until the
    /// daemon's next report, so an answered one must not reopen.
    pub fn open_waiting_permission(&mut self) {
        if !matches!(self.mode, AppMode::Chat) {
            return;
        }
        let Some(request) = &self.session_runtime.waiting_permission else {
            return;
        };
        if !self.shown_permission_requests.insert(request.id.clone()) {
            return;
        }
        self.mode = AppMode::PermissionConfirm(crate::tui::state::PermissionConfirmState {
            request_id: request.id.clone(),
            request: request.request.clone(),
        });
    }

    /// Close the permission dialog once its request is no longer the one the
    /// session waits on: another client answered it, or it was withdrawn.
    pub fn close_resolved_permission(&mut self) {
        if let AppMode::PermissionConfirm(state) = &self.mode
            && self
                .session_runtime
                .waiting_permission
                .as_ref()
                .is_none_or(|request| request.id != state.request_id)
        {
            self.mode = AppMode::Chat;
        }
    }

    /// `/permission` re-asks the launch directory's access decision — the
    /// repo-trust sheet inside a worktree, the write-grant outside one.
    /// The repo-trust sheet opens on the persisted level, so it shows what
    /// is in force; the answer overwrites it.
    fn open_permission_gate(&mut self) {
        let home = crate::session::MayHomePaths::new(self.session_manager.may_dir().to_path_buf());
        let config_path = self.session_manager.current_profile_id().map_or_else(
            || home.workspace_config(),
            |id| home.profile_home(id).workspace_config(),
        );
        if let Some(worktree) = crate::repo_search::worktree_root(&self.cwd) {
            let current = match crate::trust::persisted_level(&worktree, &config_path) {
                Ok(current) => current,
                Err(error) => {
                    self.chat.push_error(&format!(
                        "Cannot read repository trust from {}: {error}",
                        config_path.display()
                    ));
                    return;
                }
            };
            let kind = crate::trust::repo_kind(&worktree);
            let mut ask = AskUserState::for_open_question(crate::trust::question_for(
                &kind, &worktree, false,
            ));
            if let Some(cursor) = current.and_then(|level| {
                ask.questions[0]
                    .option
                    .iter()
                    .position(|option| crate::trust::option_level(option) == Some(level))
            }) {
                ask.question_states[0].cursor = cursor;
            }
            self.mode = AppMode::RepoTrust(crate::tui::state::RepoTrustState {
                ask,
                worktree,
                kind,
                config_path,
                redecide: true,
            });
        } else {
            self.mode = AppMode::LaunchGrant(crate::tui::state::LaunchGrantState {
                path: self.cwd.clone(),
                config_path,
                redecide: true,
                selected: 0,
            });
        }
    }

    /// Offer the open-question panel: it opens now when the chat is up,
    /// parks behind whatever screen is open otherwise. Used when a new open
    /// question arrives so the user sees it without the panel ripping a
    /// different screen away.
    pub fn offer_open_question(&mut self, state: AskUserState) {
        if matches!(self.mode, AppMode::Chat) {
            self.mode = AppMode::AskUser(state);
        } else {
            self.parked_question = Some(state);
        }
    }

    /// Bring back the parked open question once the screen is free.
    pub fn restore_parked_question(&mut self) {
        super::reducer::ask_user::restore_parked_question(
            &mut self.mode,
            &mut self.parked_question,
        );
    }

    /// Show a status notice that clears itself after [`STATUS_NOTICE_TTL`]
    /// — or the moment the user presses a key. Notices are transient by
    /// construction; nothing lingers on screen.
    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = Some(StatusNotice::transient(text, STATUS_NOTICE_TTL));
    }

    /// Show a status notice that stays until [`Self::clear_persistent_status`]
    /// or another status replaces it — for live state like a quota-reset wait.
    pub fn set_persistent_status(&mut self, text: impl Into<String>) {
        self.status = Some(StatusNotice::persistent(text));
    }

    /// Drop a persistent notice; transient notices are untouched.
    pub fn clear_persistent_status(&mut self) {
        if self
            .status
            .as_ref()
            .is_some_and(StatusNotice::is_persistent)
        {
            self.status = None;
        }
    }

    /// Whether an agent run is still in flight for the current session —
    /// a turn this client follows or one the daemon reports.
    #[must_use]
    pub const fn agent_busy(&self) -> bool {
        self.processing || !self.session_runtime.state.is_idle()
    }

    /// Answer a quit request: quits at once when no run is in flight, and
    /// asks first what happens to a busy session's agent. A quit request
    /// repeated while the dialog is up leaves the agent running.
    fn request_quit(&mut self) -> Option<SendAction> {
        if matches!(self.mode, AppMode::QuitConfirm(_)) || !self.agent_busy() {
            return Some(SendAction::Quit);
        }
        self.quit_pending = false;
        self.status = None;
        let return_to = std::mem::replace(&mut self.mode, AppMode::Chat);
        self.mode = AppMode::QuitConfirm(QuitConfirmState {
            return_to: Box::new(return_to),
        });
        None
    }

    /// Text for the status line; `None` once a transient notice's deadline
    /// passed, even before the next tick sweeps it.
    #[must_use]
    pub fn status_text(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|notice| !notice.expired(std::time::Instant::now()))
            .map(|notice| notice.text.as_str())
    }

    /// Get the currently active text input field, if any.
    fn active_text_input(&mut self) -> Option<ActiveTextInput<'_>> {
        // Check mode-specific inputs first
        match &self.mode {
            AppMode::Chat => {
                // Chat has special handling for large pastes (attachments)
                return Some(ActiveTextInput::Cursored {
                    input: &mut self.input,
                    cursor: &mut self.cursor,
                });
            }
            AppMode::Config => {
                // Handle all config edit modes via a single match
                match &mut self.config_state.edit_mode {
                    ConfigEditMode::CustomProvider {
                        focused_field,
                        name,
                        base_url,
                        api_key,
                        cursor,
                        ..
                    } => {
                        let input = match *focused_field {
                            0 => name,
                            1 => base_url,
                            _ => api_key,
                        };
                        return Some(ActiveTextInput::Cursored { input, cursor });
                    }
                    ConfigEditMode::Text { input, cursor } => {
                        return Some(ActiveTextInput::Cursored { input, cursor });
                    }
                    _ => {}
                }
            }
            AppMode::OpenrouterAuth => {
                if let Some(OpenrouterAuthStatus::ManualEntry { input, .. }) =
                    &mut self.openrouter_auth_status
                {
                    return Some(ActiveTextInput::Simple {
                        input,
                        filter: PasteFilter::Any,
                    });
                }
            }
            AppMode::SessionSelect => {
                return Some(ActiveTextInput::Simple {
                    input: &mut self.session_search,
                    filter: PasteFilter::Any,
                });
            }
            AppMode::ResumePicker(_) => {
                if let AppMode::ResumePicker(state) = &mut self.mode {
                    return Some(ActiveTextInput::Simple {
                        input: &mut state.search,
                        filter: PasteFilter::Any,
                    });
                }
            }
            AppMode::TierModelPicker => {
                return Some(ActiveTextInput::Simple {
                    input: &mut self.tier_picker_query,
                    filter: PasteFilter::Any,
                });
            }
            AppMode::McpManage => match self.mcp_add_mode {
                McpAddMode::Name => {
                    return Some(ActiveTextInput::Simple {
                        input: &mut self.mcp_add_name,
                        filter: PasteFilter::Identifier,
                    });
                }
                McpAddMode::Command => {
                    return Some(ActiveTextInput::Simple {
                        input: &mut self.mcp_add_command,
                        filter: PasteFilter::Any,
                    });
                }
                McpAddMode::None => {}
            },
            AppMode::SshManage => match self.ssh_add_mode {
                SshAddMode::Name => {
                    return Some(ActiveTextInput::Simple {
                        input: &mut self.ssh_add_name,
                        filter: PasteFilter::Identifier,
                    });
                }
                SshAddMode::Target => {
                    return Some(ActiveTextInput::Simple {
                        input: &mut self.ssh_add_target,
                        filter: PasteFilter::Any,
                    });
                }
                SshAddMode::None => {}
            },
            AppMode::SkillManage => {
                match self.skill_add_mode {
                    SkillAddMode::GitHubRepo => {
                        return Some(ActiveTextInput::Simple {
                            input: &mut self.skill_github_input,
                            filter: PasteFilter::GitHubRepo,
                        });
                    }
                    SkillAddMode::None => {
                        // Skill search is always active in SkillManage
                        return Some(ActiveTextInput::Simple {
                            input: &mut self.skill_search,
                            filter: PasteFilter::Any,
                        });
                    }
                }
            }
            AppMode::AskUser(_) => {
                if let AppMode::AskUser(state) = &mut self.mode {
                    let secret = state.current_secret().is_some();
                    if let Some(qs) = state.current_state_mut()
                        && (qs.custom_mode || secret)
                    {
                        if secret {
                            qs.declined = false;
                        }
                        return Some(ActiveTextInput::Simple {
                            input: &mut qs.custom_text,
                            filter: PasteFilter::Any,
                        });
                    }
                }
            }
            // FilePicker is handled separately (has its own insert_str method)
            _ => {}
        }
        None
    }

    /// Terminal paste (bracketed paste). When the payload resolves to a
    /// filesystem path — how terminals that materialize pasted images as
    /// files deliver them (iTerm2, cmux-style temp-file bridges, dragged
    /// files, Finder copies) — the path is read on a worker and an image
    /// attaches; anything else pastes as text unchanged.
    pub(super) fn handle_paste_event(&mut self, text: String) -> Option<SendAction> {
        if matches!(self.mode, AppMode::Chat)
            && self.clipboard_rx.is_none()
            && pasted_image_path_candidate(&text).is_some()
        {
            self.spawn_image_path_attach(text);
            return None;
        }
        self.handle_bracketed_paste(text)
    }

    /// Read a pasted file reference on a worker thread: an image attaches,
    /// anything else pastes as the original text through the channel.
    fn spawn_image_path_attach(&mut self, pasted: String) {
        let Some(path) = pasted_image_path_candidate(&pasted) else {
            return;
        };
        let (tx, rx) = async_channel::bounded(1);
        self.clipboard_rx = Some(rx);
        self.pending_attachment_jobs = self.pending_attachment_jobs.saturating_add(1);
        spawn(async move {
            let result = match unblock(move || read_pasted_image_file(&path)).await {
                Ok(Some(image)) => Ok(ClipboardPasteResult::image(image)),
                Ok(None) => Ok(ClipboardPasteResult::text(pasted)),
                Err(e) => Err(TuiError::from(e)),
            };
            let _ = tx.send(result).await;
        })
        .detach();
    }

    /// Route pasted text — a terminal paste event or a Ctrl+V clipboard
    /// read — into the focused input, or an attachment when it is large.
    pub(super) fn handle_bracketed_paste(&mut self, text: String) -> Option<SendAction> {
        // Special handling for Chat mode: large pastes become attachments
        if matches!(self.mode, AppMode::Chat)
            && (text.contains('\n') || text.len() > LARGE_PASTE_THRESHOLD)
        {
            self.set_status("Saving pasted text...");
            self.enqueue_paste_result(Ok(ClipboardPasteResult::text(text)));
            return None;
        }

        // Use first line only for non-chat inputs (URLs, API keys, etc. are single-line)
        let text = if matches!(self.mode, AppMode::Chat) {
            text
        } else {
            text.lines().next().unwrap_or("").to_string()
        };

        if text.is_empty() {
            return None;
        }

        // Handle FilePicker separately (has its own insert_str method)
        if matches!(self.mode, AppMode::FilePicker) {
            if let Some(picker) = &mut self.file_picker {
                picker.insert_str(&text);
            }
            return None;
        }

        // Get active text input and paste into it
        match self.active_text_input() {
            Some(ActiveTextInput::Cursored { input, cursor }) => {
                let byte_idx = char_to_byte_index(input, *cursor);
                input.insert_str(byte_idx, &text);
                *cursor += text.chars().count();

                // Update input_char_count if this is the main chat input
                if matches!(self.mode, AppMode::Chat) {
                    self.input_char_count = self.input.chars().count();
                    self.update_completion_state();
                }
            }
            Some(ActiveTextInput::Simple { input, filter }) => {
                let filtered = filter.filter(&text);
                input.push_str(&filtered);
            }
            None => {}
        }

        None
    }

    /// Handle a key event.
    pub(super) fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> AppResult<Option<SendAction>> {
        // Any key ends the fireworks and is spent on that.
        if self.new_year.animating() {
            self.new_year.stop();
            return Ok(None);
        }
        // Global shortcuts
        if modifiers.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('c') => {
                    if self.copy_selection_to_clipboard()? {
                        self.quit_pending = false;
                        self.set_status("Copied selection");
                        return Ok(None);
                    }

                    // Double-press Ctrl+C to quit
                    if self.quit_pending {
                        return Ok(self.request_quit());
                    }
                    // First Ctrl+C: clear input and show exit hint
                    self.quit_pending = true;
                    self.input.clear();
                    self.cursor = 0;
                    self.input_char_count = 0;
                    self.attachments.clear();
                    self.completion.cancel();
                    self.set_status("^C Press Ctrl+C again to exit");
                    return Ok(None);
                }
                KeyCode::Char('q') => return Ok(self.request_quit()),
                // Ctrl+V pastes the system clipboard through the same path
                // as a terminal paste; bound globally so every text input
                // can use it.
                KeyCode::Char('v' | 'V') => {
                    self.handle_paste();
                    return Ok(None);
                }
                KeyCode::Left => return Ok(Some(SendAction::OpenQuestions)),
                // Ctrl+F plays the New Year fireworks while the greeting shows.
                KeyCode::Char('f')
                    if matches!(self.mode, AppMode::Chat) && self.new_year.greeting() =>
                {
                    self.new_year.start();
                    return Ok(None);
                }
                // Ctrl+B shows/hides the right sidebar (wide terminals only).
                KeyCode::Char('b') => {
                    self.sidebar.enabled = !self.sidebar.enabled;
                    return Ok(None);
                }
                KeyCode::Char('n') => {
                    self.quit_pending = false;
                    self.status = None;
                    return Ok(Some(SendAction::NewSession));
                }
                KeyCode::Char('p') => {
                    self.quit_pending = false;
                    self.status = None;
                    if self.profiles.len() > 1 {
                        // Open profile select popup
                        self.profile_selected = self
                            .profiles
                            .iter()
                            .position(|p| Some(p.id.as_str()) == self.current_profile_id.as_deref())
                            .unwrap_or(0);
                        self.mode = AppMode::ProfileSelect;
                    } else {
                        // Only one profile, open session list directly
                        self.session_view = super::state::SessionView::Recent;
                        self.load_session_list();
                    }
                    return Ok(None);
                }
                _ => {}
            }
        }

        // Cmd+V (super+V): terminals forwarding super-modified keys via the
        // kitty keyboard protocol deliver it here; the same paste path as
        // Ctrl+V.
        if modifiers.contains(KeyModifiers::SUPER) && matches!(code, KeyCode::Char('v' | 'V')) {
            self.handle_paste();
            return Ok(None);
        }

        // Cmd+Enter (Ctrl where Cmd cannot reach the app) opens the link
        // under the pointer, as the same-modified click does.
        if matches!(self.mode, AppMode::Chat)
            && super::ui::links::dispatch_enter(
                self.link_opener.as_ref(),
                code,
                modifiers,
                self.hovered_link().as_ref(),
            )
        {
            return Ok(None);
        }

        // Reset quit confirmation on any other input
        if self.quit_pending {
            self.quit_pending = false;
            self.status = None;
        }

        // The per-mode handlers see the key code alone, so a modified
        // character with no binding above must not reach them as text.
        if matches!(code, KeyCode::Char(_))
            && modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return Ok(None);
        }

        match &self.mode {
            AppMode::Chat => Ok(self.handle_chat_key(code)),
            AppMode::SessionSelect => Ok(super::reducer::navigation::handle_session_select_key(
                self, code,
            )),
            AppMode::FilePicker => Ok(super::reducer::navigation::handle_file_picker_key(
                self, code,
            )),
            AppMode::Help => Ok(super::reducer::navigation::handle_help_key(self, code)),
            AppMode::ToolDetail => Ok(super::reducer::navigation::handle_tool_detail_key(
                self, code,
            )),
            AppMode::Config => Ok(self.handle_config_key(code, modifiers)),
            AppMode::ModelSelect => Ok(self.handle_model_select_key(code)),
            AppMode::EffortSelect => Ok(self.handle_effort_select_key(code)),
            AppMode::TierModelPicker => Ok(self.handle_tier_model_picker_key(code)),
            AppMode::CopilotAuth => Ok(super::reducer::manage::handle_copilot_auth_key(self, code)),
            AppMode::ChatgptAuth => Ok(super::reducer::manage::handle_chatgpt_auth_key(self, code)),
            AppMode::Login => Ok(super::reducer::manage::handle_login_key(self, code)),
            AppMode::Connect(_) => {
                super::connect::handle_connect_key(self, code);
                Ok(None)
            }
            AppMode::GrokAuth => Ok(super::reducer::manage::handle_grok_auth_key(self, code)),
            AppMode::OpenrouterAuth => Ok(super::reducer::manage::handle_openrouter_auth_key(
                self, code,
            )),
            AppMode::PermissionConfirm(_) => Ok(
                super::reducer::manage::handle_permission_confirm_key(self, code),
            ),
            AppMode::WorkspaceConfirm(_) => Ok(
                super::reducer::manage::handle_workspace_confirm_key(self, code),
            ),
            AppMode::LaunchGrant(_) => {
                Ok(super::reducer::manage::handle_launch_grant_key(self, code))
            }
            AppMode::RepoTrust(_) => {
                Ok(super::reducer::ask_user::handle_repo_trust_key(self, code))
            }
            AppMode::McpManage => Ok(super::reducer::manage::handle_mcp_key(self, code)),
            AppMode::SshManage => Ok(super::reducer::manage::handle_ssh_key(self, code)),
            AppMode::AskUser(_) => Ok(super::reducer::ask_user::handle_ask_user_key(self, code)),
            AppMode::SkillManage => Ok(super::reducer::manage::handle_skill_key(self, code)),
            AppMode::ProfileSelect => Ok(super::reducer::navigation::handle_profile_select_key(
                self, code,
            )),
            AppMode::Rewind(_) => Ok(super::reducer::navigation::handle_rewind_key(self, code)),
            AppMode::ResumePicker(_) => Ok(super::reducer::navigation::handle_resume_picker_key(
                self, code,
            )),
            AppMode::Jobs => Ok(super::reducer::navigation::handle_jobs_key(self, code)),
            AppMode::JobDetail(_) => Ok(super::reducer::navigation::handle_job_detail_key(
                self, code,
            )),
            AppMode::Btw(_) => Ok(super::reducer::navigation::handle_btw_key(self, code)),
            AppMode::Progress(_) => Ok(super::reducer::navigation::handle_progress_key(self, code)),
            AppMode::ImageView(_) => Ok(super::reducer::navigation::handle_image_view_key(
                self, code,
            )),
            AppMode::QuitConfirm(_) => {
                Ok(super::reducer::manage::handle_quit_confirm_key(self, code))
            }
        }
    }

    /// Get filtered skill list based on search query.
    pub fn filtered_skills(&self) -> Vec<&crate::skill::SkillInfo> {
        if self.skill_search.is_empty() {
            self.skill_list.iter().collect()
        } else {
            let query = self.skill_search.to_lowercase();
            self.skill_list
                .iter()
                .filter(|s| {
                    s.name.to_lowercase().contains(&query)
                        || s.description.to_lowercase().contains(&query)
                })
                .collect()
        }
    }

    /// Get filtered sessions based on search query.
    /// Searches in both title and content preview.
    pub fn filtered_sessions(&self) -> Vec<(usize, &SessionSummary)> {
        if self.session_search.is_empty() {
            self.session_list.iter().enumerate().collect()
        } else {
            let query = self.session_search.to_lowercase();
            self.session_list
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    // Search in title
                    s.title
                        .as_ref()
                        .is_some_and(|t| t.to_lowercase().contains(&query))
                        // Search in content preview
                        || s.content_preview.to_lowercase().contains(&query)
                })
                .collect()
        }
    }

    /// Open the Claude Code resume picker and list transcripts off the event loop.
    fn open_resume_picker(&mut self) {
        self.completion.cancel();
        self.mode = AppMode::ResumePicker(ResumePickerState::loading());
        let tx = self.event_tx.clone();
        spawn(async move {
            let sessions = unblock(crate::session::import::list_claude_sessions).await;
            let _ = tx.send(AppEvent::ResumeSessionsLoaded(sessions)).await;
        })
        .detach();
    }

    /// Trigger async loading of session list for the session select popup.
    /// Uses profile sessions when a profile is active, falls back to flat list.
    pub fn load_session_list(&mut self) {
        let tx = self.event_tx.clone();
        let Some(client) = self.btw_daemon.clone() else {
            return;
        };
        let profile_id = self.current_profile_id.clone();

        spawn(async move {
            if let Some(pid) = profile_id {
                let result = client
                    .get::<crate::profile::ProfileSessions>(&format!(
                        "/api/profiles/{pid}/sessions"
                    ))
                    .await;
                let _ = tx
                    .send(AppEvent::ProfileSessionsLoaded(
                        result.map_err(TuiError::from),
                    ))
                    .await;
            } else {
                let result = client
                    .get::<Vec<crate::client::WireSession>>("/api/sessions")
                    .await
                    .map(|sessions| {
                        sessions
                            .into_iter()
                            .map(|session| crate::session::SessionSummary {
                                id: crate::session::SessionId::from_string(session.id),
                                title: Some(session.title),
                                model: Some(session.model),
                                profile_id: session.profile_id,
                                pinned: false,
                                archived: false,
                                folder_id: None,
                                pin_sort_order: None,
                                message_count: 0,
                                created_at: None,
                                updated_at: None,
                                content_preview: String::new(),
                                reasoning_effort: session.reasoning_effort,
                                repo: session.repo,
                                runtime: crate::session::SessionRuntimeSummary::default(),
                                working: false,
                            })
                            .collect()
                    });
                let _ = tx
                    .send(AppEvent::SessionsLoaded(result.map_err(TuiError::from)))
                    .await;
            }
        })
        .detach();
        self.set_status("Loading sessions...");
    }

    /// Build a flat session list from profile sessions for navigation.
    /// The recent view lists pinned items first, then regular sessions; the
    /// archive view lists the archived ones.
    pub fn build_session_list_from_profile(&mut self) {
        let Some(ref ps) = self.profile_sessions else {
            self.session_list.clear();
            return;
        };

        if self.session_view == super::state::SessionView::Archive {
            self.session_list.clone_from(&ps.archived);
            return;
        }

        let mut list = Vec::new();

        // Pinned items
        for item in &ps.pinned {
            match item {
                PinnedItem::Session(s) => list.push((**s).clone()),
                PinnedItem::Folder { sessions, .. } => {
                    list.extend(sessions.iter().cloned());
                }
            }
        }

        // Regular sessions, grouped by project
        list.extend(crate::session::group_sessions(
            ps.regular.clone(),
            |session| session.repo.as_deref(),
        ));

        self.session_list = list;
    }

    /// The provider type serving the session's current model; picks the
    /// effort ladder's vocabulary.
    fn current_provider_type(&self) -> Option<crate::config::ProviderType> {
        let model = self
            .current_model
            .clone()
            .or_else(|| self.config.last_model.as_ref().map(ToString::to_string))?;
        let selector = model.parse::<crate::ModelSelector>().ok()?;
        self.config
            .provider
            .get(selector.provider_id())
            .map(|instance| instance.resolve_type(selector.provider_id()))
    }

    /// Fetch the session's stored reasoning effort so the popup highlights
    /// the level other clients may have set.
    fn fetch_current_effort(&self) {
        let (Some(client), Some(session_id)) = (
            self.btw_daemon.clone(),
            self.session_manager
                .current()
                .filter(|session| session.persisted)
                .map(|s| s.id.clone()),
        ) else {
            return;
        };
        let tx = self.event_tx.clone();
        spawn(async move {
            if let Ok(session) = client
                .get::<crate::client::WireSession>(&format!(
                    "/api/sessions/{}",
                    session_id.as_str()
                ))
                .await
            {
                let _ = tx
                    .send(AppEvent::EffortLoaded(session.reasoning_effort))
                    .await;
            }
        })
        .detach();
    }

    /// Get the current profile name for display.
    pub fn current_profile_name(&self) -> Option<&str> {
        let pid = self.current_profile_id.as_deref()?;
        self.profiles
            .iter()
            .find(|p| p.id == pid)
            .map(|p| p.name.as_str())
    }

    /// Follows the daemon's pushed session lists of the current profile
    /// while `SessionSelect` is open, and stops once it closes.
    pub fn sync_profile_follow(&mut self) {
        let wanted = match (&self.mode, &self.current_profile_id, &self.btw_daemon) {
            (AppMode::SessionSelect, Some(profile_id), Some(_)) => Some(profile_id.clone()),
            _ => None,
        };
        if self.profile_follow.as_ref().map(|(id, _)| id) == wanted.as_ref() {
            return;
        }
        if let Some((_, task)) = self.profile_follow.take() {
            task.abort();
        }
        if let (Some(profile_id), Some(client)) = (wanted, self.btw_daemon.clone()) {
            let task = tokio::spawn(super::remote::follow_profile_sessions(
                client,
                profile_id.clone(),
                self.event_tx.clone(),
            ));
            self.profile_follow = Some((profile_id, task));
        }
    }

    /// Reload profile sessions asynchronously (used after pin/archive mutations).
    pub fn refresh_profile_sessions(&mut self) {
        let Some(pid) = self.current_profile_id.clone() else {
            return;
        };
        let tx = self.event_tx.clone();
        let Some(client) = self.btw_daemon.clone() else {
            return;
        };

        spawn(async move {
            let result = client
                .get::<crate::profile::ProfileSessions>(&format!("/api/profiles/{pid}/sessions"))
                .await;
            let _ = tx
                .send(AppEvent::ProfileSessionsLoaded(
                    result.map_err(TuiError::from),
                ))
                .await;
        })
        .detach();
    }

    /// Start the Copilot OAuth device flow.
    ///
    /// This sets the mode to `CopilotAuth` and status to `RequestingCode`.
    /// The caller should then spawn an async task to perform the OAuth flow.
    pub fn start_copilot_auth(&mut self) {
        self.mode = AppMode::CopilotAuth;
        self.copilot_auth_status = Some(CopilotAuthStatus::RequestingCode);
    }

    /// Update the Copilot auth status with the device code info.
    pub fn set_copilot_device_code(&mut self, user_code: String, verification_uri: String) {
        self.copilot_auth_status = Some(CopilotAuthStatus::WaitingForUser {
            user_code,
            verification_uri,
        });
    }

    /// Mark Copilot auth as successful and return to config.
    pub fn copilot_auth_success(&mut self) {
        self.copilot_auth_status = Some(CopilotAuthStatus::Success);
    }

    /// Mark Copilot auth as failed.
    pub fn copilot_auth_failed(&mut self, error: String) {
        self.copilot_auth_status = Some(CopilotAuthStatus::Failed(error));
    }

    /// Start the `ChatGPT` OAuth flow (initial "working" state).
    pub fn start_chatgpt_auth(&mut self) {
        self.mode = AppMode::ChatgptAuth;
        self.chatgpt_auth_status = Some(ChatgptAuthStatus::Working);
    }

    /// Show the browser loopback authorization URL.
    pub fn set_chatgpt_auth_url(&mut self, url: String) {
        self.chatgpt_auth_status = Some(ChatgptAuthStatus::WaitingForCallback { url });
    }

    /// Switch the `ChatGPT` auth screen to the device-flow variant.
    pub fn set_chatgpt_device_code(&mut self, user_code: String, verification_uri: String) {
        self.chatgpt_auth_status = Some(ChatgptAuthStatus::WaitingForUser {
            user_code,
            verification_uri,
        });
    }

    /// Mark `ChatGPT` auth as successful.
    pub fn chatgpt_auth_success(&mut self) {
        self.chatgpt_auth_status = Some(ChatgptAuthStatus::Success);
    }

    /// Mark `ChatGPT` auth as failed.
    pub fn chatgpt_auth_failed(&mut self, error: String) {
        self.chatgpt_auth_status = Some(ChatgptAuthStatus::Failed(error));
    }

    /// The screen an OAuth flow returns to when it closes.
    #[must_use]
    pub const fn auth_return_mode(&self) -> AppMode {
        match self.auth_return {
            AuthReturn::Config => AppMode::Config,
            AuthReturn::Login => AppMode::Login,
        }
    }

    /// Start the xAI Grok OAuth flow (requesting a device code).
    pub fn start_grok_auth(&mut self) {
        self.mode = AppMode::GrokAuth;
        self.grok_auth_status = Some(GrokAuthStatus::RequestingCode);
    }

    /// Show the device-flow verification URL and user code.
    pub fn set_grok_device_code(&mut self, user_code: String, verification_uri: String) {
        self.grok_auth_status = Some(GrokAuthStatus::WaitingForUser {
            user_code,
            verification_uri,
        });
    }

    /// Mark xAI Grok auth as successful.
    pub fn grok_auth_success(&mut self) {
        self.grok_auth_status = Some(GrokAuthStatus::Success);
    }

    /// Mark xAI Grok auth as failed.
    pub fn grok_auth_failed(&mut self, error: String) {
        self.grok_auth_status = Some(GrokAuthStatus::Failed(error));
    }

    /// Start the `OpenRouter` OAuth flow (initial "working" state).
    pub fn start_openrouter_auth(&mut self) {
        self.mode = AppMode::OpenrouterAuth;
        self.openrouter_auth_status = Some(OpenrouterAuthStatus::Working);
    }

    /// Show the browser authorization URL while the loopback listener waits.
    pub fn set_openrouter_auth_url(&mut self, url: String) {
        self.openrouter_auth_status = Some(OpenrouterAuthStatus::WaitingForCallback { url });
    }

    /// Switch the `OpenRouter` screen to manual code/URL entry.
    pub fn set_openrouter_manual_entry(&mut self, url: String) {
        self.openrouter_auth_status = Some(OpenrouterAuthStatus::ManualEntry {
            url,
            input: String::new(),
        });
    }

    /// Mark `OpenRouter` auth as successful.
    pub fn openrouter_auth_success(&mut self) {
        self.openrouter_auth_status = Some(OpenrouterAuthStatus::Success);
    }

    /// Mark `OpenRouter` auth as failed.
    pub fn openrouter_auth_failed(&mut self, error: String) {
        self.openrouter_auth_status = Some(OpenrouterAuthStatus::Failed(error));
    }

    /// Whether the `/login` provider `id` already has working credentials.
    #[must_use]
    pub fn login_signed_in(&self, id: &str) -> bool {
        match id {
            "chatgpt" => crate::chatgpt::has_credentials(),
            "grok" => crate::grok::has_credentials(),
            "openrouter" => self
                .config
                .provider
                .get("openrouter")
                .is_some_and(|instance| instance.api_key.is_some()),
            _ => false,
        }
    }

    /// Handle key in chat mode (unified input + navigation).
    fn handle_chat_key(&mut self, code: KeyCode) -> Option<SendAction> {
        match code {
            KeyCode::Enter => {
                // Accept a palette item when one is selected. An empty
                // palette (slash command plus arguments) must fall through
                // so `/resume-claude <id>` submits.
                if self.completion.has_matches() {
                    let mode = self.completion.mode;
                    if let Some(item) = self.completion.complete() {
                        let cmd = item.insert.clone();
                        self.completion.cancel();

                        // For slash commands, execute directly without showing in input
                        if mode == CompletionMode::Slash {
                            self.input.clear();
                            self.cursor = 0;
                            self.input_char_count = 0;
                            let command = SlashCommand::from_name(&cmd)
                                .expect("completion lists only table commands");
                            return self.execute_slash_command(command);
                        }

                        // For file completion, insert into input
                        self.accept_completion(&cmd);
                    }
                    return None;
                }
                if self.completion.is_active() {
                    self.completion.cancel();
                }
                if self.has_pending_attachments() {
                    self.set_status("Waiting for attachments to finish...");
                    return None;
                }
                // `!` prefix: bash mode — run the command locally and send
                // its masked transcript to the agent.
                if self.input.starts_with('!') {
                    return self.submit_bash();
                }
                // `/secret KEY VALUE` stores to the keychain; it must never
                // enter input history or the persisted transcript.
                if self.input.trim() == "/secret" || self.input.trim_start().starts_with("/secret ")
                {
                    return self.submit_secret();
                }
                // Send as message (slash commands only work from menu selection)
                if !self.input.is_empty() || !self.attachments.is_empty() {
                    return self.prepare_message(PendingUserMessageMode::Steer);
                }
            }
            KeyCode::Backspace => {
                self.history.reset_navigation();
                if self.cursor > 0 {
                    // Delete character before cursor
                    self.cursor -= 1;
                    let byte_idx = char_to_byte_index(&self.input, self.cursor);
                    self.input.remove(byte_idx);
                    self.input_char_count -= 1;
                    self.update_completion_state();
                } else if !self.attachments.is_empty() {
                    // Cursor at start and has attachments - delete last attachment
                    self.attachments.pop();
                }
            }
            KeyCode::Delete => {
                self.history.reset_navigation();
                if self.cursor < self.input_char_count {
                    let byte_idx = char_to_byte_index(&self.input, self.cursor);
                    self.input.remove(byte_idx);
                    self.input_char_count -= 1;
                    self.update_completion_state();
                }
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input_char_count);
            }
            KeyCode::Up => {
                if self.completion.has_matches() {
                    self.completion.prev();
                } else if let Some(entry) = self.history.prev(&self.input) {
                    self.input = entry.to_string();
                    self.input_char_count = self.input.chars().count();
                    self.cursor = self.input_char_count;
                }
            }
            KeyCode::Down => {
                if self.completion.has_matches() {
                    self.completion.next();
                } else if let Some(entry) = self.history.next_entry() {
                    self.input = entry.to_string();
                    self.input_char_count = self.input.chars().count();
                    self.cursor = self.input_char_count;
                } else if self.background.is_some() {
                    // ↓ past the newest history entry opens the background
                    // jobs/subagents panel (status bar hint: `N shells ↓`).
                    self.open_jobs();
                }
            }
            // Mouse capture + alternate screen swallow native terminal
            // scrolling, so chat scrolling must be handled here.
            KeyCode::PageUp => {
                let page = self.chat_scroll_page();
                self.scroll_chat_up(page);
            }
            KeyCode::PageDown => {
                let page = self.chat_scroll_page();
                self.scroll_chat_down(page);
            }
            KeyCode::Home => {
                self.cursor = 0;
            }
            KeyCode::End => {
                self.cursor = self.input_char_count;
            }
            KeyCode::Esc => {
                if self.prompt_suggestion.is_some() {
                    self.prompt_suggestion = None;
                    self.last_esc_at = Some(std::time::Instant::now());
                } else if self.completion.has_matches() {
                    self.completion.cancel();
                    self.last_esc_at = Some(std::time::Instant::now());
                } else if self
                    .last_esc_at
                    .is_some_and(|at| at.elapsed() <= DOUBLE_ESC_WINDOW)
                    && !self.processing
                    && self.open_rewind_picker()
                {
                    // Esc Esc: open the rewind picker.
                    self.last_esc_at = None;
                } else {
                    self.last_esc_at = Some(std::time::Instant::now());
                    if !self.input.is_empty() {
                        self.input.clear();
                        self.cursor = 0;
                        self.input_char_count = 0;
                    }
                }
            }
            KeyCode::Tab => {
                if self.completion.has_matches()
                    && let Some(item) = self.completion.complete()
                {
                    self.accept_completion(&item.insert.clone());
                    return None;
                }
                if let Some(suggestion) = self.own_visible_suggestion().map(str::to_owned) {
                    self.input_char_count = suggestion.chars().count();
                    self.input = suggestion;
                    self.cursor = self.input_char_count;
                    self.prompt_suggestion = None;
                    return None;
                }
                if self.processing && (!self.input.is_empty() || !self.attachments.is_empty()) {
                    if self.has_pending_attachments() {
                        self.set_status("Waiting for attachments to finish...");
                        return None;
                    }
                    return self.prepare_message(PendingUserMessageMode::Queue);
                }
            }
            KeyCode::Char(c) => {
                // Reset history navigation when typing
                self.history.reset_navigation();
                let byte_idx = char_to_byte_index(&self.input, self.cursor);
                self.input.insert(byte_idx, c);
                self.cursor += 1;
                self.input_char_count += 1;

                // Trigger or update completion
                self.update_completion_state();
            }
            _ => {}
        }
        None
    }

    /// The scroll offset the last frame actually showed: the follow-bottom
    /// sentinel resolved, and a stale offset clamped to content that has
    /// since shrunk (resize, collapsed tool calls).
    #[must_use]
    pub fn effective_scroll(&self) -> usize {
        self.scroll.min(self.max_chat_scroll())
    }

    /// Largest scroll offset that still shows a full view of content.
    fn max_chat_scroll(&self) -> usize {
        self.content_height
            .load(Ordering::Relaxed)
            .saturating_sub(self.view_height.load(Ordering::Relaxed))
    }

    /// Scroll the chat view up by `lines`, leaving follow-bottom mode.
    ///
    /// When the content already fits the view there is nothing to scroll, so
    /// follow-bottom mode is preserved and new content keeps the view pinned
    /// to the bottom.
    pub(super) fn scroll_chat_up(&mut self, lines: usize) {
        let max_scroll = self.max_chat_scroll();
        if max_scroll == 0 {
            return;
        }
        // Clamp first: an offset left beyond the content (which shrank
        // since) would otherwise swallow scroll steps without moving.
        self.scroll = self.scroll.min(max_scroll).saturating_sub(lines);
    }

    /// Scroll the chat view down by `lines`, re-entering follow-bottom mode
    /// when the bottom is reached.
    pub(super) fn scroll_chat_down(&mut self, lines: usize) {
        if self.scroll == usize::MAX {
            return;
        }
        let next = self.scroll.saturating_add(lines);
        self.scroll = if next >= self.max_chat_scroll() {
            usize::MAX
        } else {
            next
        };
    }

    /// Lines to move for one page of keyboard scrolling.
    fn chat_scroll_page(&self) -> usize {
        self.view_height
            .load(Ordering::Relaxed)
            .saturating_sub(1)
            .max(1)
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) -> AppResult<Option<SendAction>> {
        const SCROLL_STEP_LINES: usize = 3;

        self.hover = None;
        // Any click ends the fireworks and is spent on that.
        if self.new_year.animating() {
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                self.new_year.stop();
            }
            return Ok(None);
        }
        match &self.mode {
            AppMode::Chat => {}
            AppMode::ImageView(_) => {
                // A click outside the displayed image closes the zoom view.
                if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                    let inside = self.image_view_area.is_some_and(|rect| {
                        rect.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
                    });
                    if !inside {
                        self.mode = AppMode::Chat;
                        self.image_view_area = None;
                    }
                }
                return Ok(None);
            }
            _ => return Ok(None),
        }

        // Clicks and drags outside the chat column (the sidebar) select
        // nothing; the wheel scrolls the chat anywhere.
        let in_chat = self.chat_area.is_empty()
            || self
                .chat_area
                .contains(ratatui::layout::Position::new(mouse.column, mouse.row));
        if !in_chat
            && !matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            )
        {
            return Ok(None);
        }
        self.hover = in_chat.then_some((mouse.column, mouse.row));

        // Ctrl-click (Cmd where the terminal reports it) opens the link
        // under the pointer instead of starting a selection.
        let hit = self.link_at_screen(mouse.column, mouse.row);
        if super::ui::links::dispatch_click(self.link_opener.as_ref(), &mouse, hit.as_ref()) {
            self.selection = None;
            self.last_click = None;
            return Ok(None);
        }

        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_chat_up(SCROLL_STEP_LINES),
            MouseEventKind::ScrollDown => self.scroll_chat_down(SCROLL_STEP_LINES),
            MouseEventKind::Down(MouseButton::Left) => {
                let point = self.mouse_to_selection_point(mouse);
                if let Some(p) = point
                    && self.chat_layout.greeting_row() == Some(p.line)
                {
                    self.new_year.start();
                    self.selection = None;
                    return Ok(None);
                }
                // A click inside the composer moves the input caret to the
                // hit cell instead of starting a text selection.
                if let Some(p) = point
                    && let Some((start, rows)) = self.chat_layout.input_span()
                    && (start..start + rows).contains(&p.line)
                {
                    let mut cursor = 0usize;
                    for row in start..p.line {
                        if let Some(text) = self.chat_layout.plain_line(row) {
                            cursor += text.chars().count();
                        }
                    }
                    self.cursor = (cursor + p.column).min(self.input_char_count);
                    self.selection = None;
                    return Ok(None);
                }
                let double_click = point.is_some_and(|p| {
                    self.last_click.is_some_and(|(at, last)| {
                        at.elapsed() <= DOUBLE_CLICK_WINDOW
                            && last.line == p.line
                            && last.column.abs_diff(p.column) <= 1
                    })
                });
                if double_click {
                    // Second click at the same spot: select the word under
                    // the cursor instead of starting a point selection.
                    self.last_click = None;
                    self.selection = point.and_then(|p| {
                        self.chat_layout
                            .plain_line(p.line)
                            .and_then(|line| TextSelection::word_at(line, p))
                    });
                } else {
                    self.selection = point.map(|p| TextSelection {
                        anchor: p,
                        cursor: p,
                    });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let next = self.mouse_to_selection_point(mouse);
                if let (Some(sel), Some(point)) = (&mut self.selection, next) {
                    sel.cursor = point;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.selection.as_ref().is_some_and(TextSelection::is_empty) {
                    self.selection = None;
                    let point = ratatui::layout::Position::new(mouse.column, mouse.row);
                    // A press/release without a drag is a click: open the
                    // zoom view when it lands on an image; otherwise remember
                    // it for double-click detection, then toggle the collapsed
                    // state of the tool call under the cursor.
                    if let Some(region) = self.image_regions.iter().find(|r| r.rect.contains(point))
                    {
                        self.last_click = None;
                        self.mode = AppMode::ImageView(ImageViewState {
                            key: region.key.clone(),
                            caption: region.caption.clone(),
                        });
                    } else {
                        self.last_click = self
                            .mouse_to_selection_point(mouse)
                            .map(|point| (Instant::now(), point));
                        if let Some(point) = self.mouse_to_selection_point(mouse)
                            && let Some(region) = self
                                .chat_layout
                                .collapse_regions()
                                .into_iter()
                                .filter(|r| point.line >= r.start && point.line < r.end)
                                // The innermost span wins: a click inside an
                                // expanded "Worked for" member toggles the
                                // member, not the group around it.
                                .min_by_key(|r| r.end - r.start)
                        {
                            self.chat.toggle_expanded(&region.element_id);
                        }
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) if self.copy_selection_to_clipboard()? => {
                self.set_status("Copied selection");
            }
            _ => {}
        }

        Ok(None)
    }

    /// The link drawn at screen cell `(col, row)`.
    fn link_at_screen(&self, col: u16, row: u16) -> Option<super::ui::links::LinkHit> {
        let x = col.checked_sub(self.chat_area.x)?;
        let y = row.checked_sub(self.chat_area.y)?;
        let doc = (self.effective_scroll() + usize::from(y))
            .checked_sub(self.chat_layout.top_padding())?;
        self.chat_layout.link_at(doc, x)
    }

    /// The link under the pointer, if it is inside the chat column.
    pub(super) fn hovered_link(&self) -> Option<super::ui::links::LinkHit> {
        let (col, row) = self.hover?;
        self.link_at_screen(col, row)
    }

    fn mouse_to_selection_point(&self, mouse: MouseEvent) -> Option<SelectionPoint> {
        let rows = self.chat_layout.plain_len();
        if rows == 0 {
            return None;
        }

        let base_scroll = self.effective_scroll();

        let raw_line = base_scroll.saturating_add(mouse.row as usize);
        let line = raw_line.saturating_sub(self.chat_layout.top_padding());
        let line = line.min(rows.saturating_sub(1));
        let text = self.chat_layout.plain_line(line)?;
        let column = clamp_column_by_width(text, mouse.column as usize);
        Some(SelectionPoint { line, column })
    }

    fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        if sel.is_empty() {
            return None;
        }
        let (start, end) = sel.normalized();

        let rows = self.chat_layout.plain_len();
        if rows == 0 || start.line >= rows {
            return None;
        }

        let end_line = end.line.min(rows.saturating_sub(1));
        let mut out = String::new();

        for line_idx in start.line..=end_line {
            let line = self.chat_layout.plain_line(line_idx).unwrap_or_default();
            let char_count = line.chars().count();
            let from = if line_idx == start.line {
                start.column.min(char_count)
            } else {
                0
            };
            let to = if line_idx == end_line {
                end.column.min(char_count)
            } else {
                char_count
            };

            if from < to {
                let from_b = char_to_byte_index(line, from);
                let to_b = char_to_byte_index(line, to);
                out.push_str(&line[from_b..to_b]);
            }

            if line_idx != end_line {
                out.push('\n');
            }
        }

        if out.is_empty() { None } else { Some(out) }
    }

    fn copy_selection_to_clipboard(&self) -> AppResult<bool> {
        let Some(text) = self.selected_text() else {
            return Ok(false);
        };

        // Try terminal clipboard first, but do not fail hard if OSC52 is unsupported.
        let osc52_ok = super::clipboard::copy_via_osc52(&text).is_ok();

        // Also try rich+plain system clipboard copy.
        let html = super::clipboard::markdown_to_basic_html(&text);
        let arboard_ok = set_clipboard_html(&html, &text)
            .or_else(|_| set_clipboard_text(&text))
            .is_ok();

        if !osc52_ok && !arboard_ok {
            return Err(TuiError::ClipboardCopyFailed);
        }

        Ok(true)
    }

    /// Execute a slash command directly (from menu selection only).
    fn execute_slash_command(&mut self, command: SlashCommand) -> Option<SendAction> {
        match command {
            SlashCommand::Session => {
                self.session_view = super::state::SessionView::Recent;
                self.load_session_list();
                None
            }
            SlashCommand::Archive => {
                self.session_view = super::state::SessionView::Archive;
                self.load_session_list();
                None
            }
            SlashCommand::New => Some(SendAction::NewSession),
            SlashCommand::Profile => {
                if self.profiles.len() > 1 {
                    self.profile_selected = self
                        .profiles
                        .iter()
                        .position(|p| Some(p.id.as_str()) == self.current_profile_id.as_deref())
                        .unwrap_or(0);
                    self.mode = AppMode::ProfileSelect;
                } else {
                    self.session_view = super::state::SessionView::Recent;
                    self.load_session_list();
                }
                None
            }
            SlashCommand::Sandbox => {
                if let Some(session) = self.session_manager.current() {
                    let sandbox_dir = &session.sandbox_dir;
                    #[cfg(target_os = "macos")]
                    {
                        let _ = std::process::Command::new("open").arg(sandbox_dir).spawn();
                    }
                    #[cfg(target_os = "linux")]
                    {
                        let _ = std::process::Command::new("xdg-open")
                            .arg(sandbox_dir)
                            .spawn();
                    }
                    #[cfg(target_os = "windows")]
                    {
                        let _ = std::process::Command::new("explorer")
                            .arg(sandbox_dir)
                            .spawn();
                    }
                }
                None
            }
            SlashCommand::Help => {
                self.help_scroll = 0;
                self.mode = AppMode::Help;
                None
            }
            SlashCommand::Config => {
                self.config_state = ConfigState::default();
                self.config_state.load_providers(&self.config);
                self.mode = AppMode::Config;
                None
            }
            SlashCommand::Model => self.open_model_select(""),
            SlashCommand::Effort => {
                self.effort_ladder = self.effort_ladder_for_current();
                self.effort_index = self.effort_ladder_index();
                self.mode = AppMode::EffortSelect;
                self.fetch_current_effort();
                None
            }
            SlashCommand::Rewind => {
                if self.processing {
                    self.chat
                        .push_notice("Wait for the current turn to finish before rewinding");
                } else if !self.open_rewind_picker() {
                    self.chat.push_notice("No prompts to rewind to");
                }
                None
            }
            SlashCommand::Jobs => {
                self.open_jobs();
                None
            }
            SlashCommand::Questions => Some(SendAction::OpenQuestions),
            SlashCommand::Btw => {
                self.open_btw();
                None
            }
            SlashCommand::Progress => {
                self.open_progress();
                None
            }
            SlashCommand::ResumeClaude => {
                self.open_resume_picker();
                None
            }
            SlashCommand::ImportClaude => Some(SendAction::RulesImport(
                crate::session::import::ForeignAgent::Claude,
            )),
            SlashCommand::ImportCodex => Some(SendAction::RulesImport(
                crate::session::import::ForeignAgent::Codex,
            )),
            SlashCommand::Connect => {
                self.open_connect_picker();
                None
            }
            SlashCommand::Login => {
                self.login_selected = 0;
                self.mode = AppMode::Login;
                None
            }
            SlashCommand::ResumeCodex | SlashCommand::Secret => {
                // These take arguments — leave the command in the input so
                // the user can type them, rather than executing now.
                self.input = format!("{} ", command.name());
                self.input_char_count = self.input.chars().count();
                self.cursor = self.input_char_count;
                None
            }
            SlashCommand::Quit => self.request_quit(),
            SlashCommand::Compact => Some(SendAction::Compact(None)),
            SlashCommand::Recap => Some(SendAction::Recap),
            SlashCommand::Yolo => Some(SendAction::ToggleYolo),
            SlashCommand::Sleep => Some(SendAction::Sleep),
            SlashCommand::SleepOff => Some(SendAction::Wake),
            SlashCommand::Context => Some(SendAction::ContextReport),
            SlashCommand::Usage => Some(SendAction::UsageReport),
            SlashCommand::ContextDump => Some(SendAction::ContextDump),
            SlashCommand::Bug => Some(SendAction::BugReport),
            SlashCommand::Mcp => {
                self.mode = AppMode::McpManage;
                None
            }
            SlashCommand::Ssh => {
                self.mode = AppMode::SshManage;
                None
            }
            SlashCommand::Permission => {
                self.open_permission_gate();
                None
            }
            SlashCommand::Skill => {
                let tx = self.event_tx.clone();
                let may_dir = self.session_manager.may_dir().to_path_buf();
                let profile_id = self
                    .session_manager
                    .current_profile_id()
                    .map(ToOwned::to_owned)
                    .expect("session manager must always have a current profile");
                spawn(async move {
                    let home_paths = crate::session::MayHomePaths::new(may_dir);
                    let manager = crate::skill::SkillManager::from_profile_home(
                        &home_paths.profile_home(profile_id.as_str()),
                    );
                    let result = manager.list_skills().await;
                    let _ = tx
                        .send(AppEvent::SkillsLoaded(result.map_err(TuiError::from)))
                        .await;
                })
                .detach();
                self.set_status("Loading skills...");
                None
            }
        }
    }

    /// Queue a paste result that becomes an attachment (a large terminal
    /// paste already accepted for the input path).
    pub fn enqueue_paste_result(&mut self, result: Result<ClipboardPasteResult, TuiError>) {
        self.pending_paste_results
            .push_back(PendingPaste::Attachment(result));
    }

    /// Queue a Ctrl+V clipboard read. Clipboard text follows the
    /// terminal-paste path — the focused input when it fits, an attachment
    /// when large — while images always attach.
    pub fn enqueue_clipboard_result(&mut self, result: Result<ClipboardPasteResult, TuiError>) {
        let paste = match result {
            Ok(result) => match result.into_parts() {
                (None, Some(text)) => PendingPaste::Input(text),
                (image, text) => {
                    PendingPaste::Attachment(Ok(ClipboardPasteResult::from_parts(image, text)))
                }
            },
            Err(error) => PendingPaste::Attachment(Err(error)),
        };
        self.pending_paste_results.push_back(paste);
    }

    /// Dequeue the next pending paste.
    pub fn dequeue_pending_paste(&mut self) -> Option<PendingPaste> {
        self.pending_paste_results.pop_front()
    }

    /// Check if any attachment jobs are still pending.
    pub fn has_pending_attachments(&self) -> bool {
        self.pending_attachment_jobs > 0 || !self.pending_paste_results.is_empty()
    }

    /// Apply a queued paste to the UI.
    ///
    /// # Errors
    ///
    /// Returns [`TuiError`] when an attachment-bound paste fails to save or
    /// the result carries a clipboard failure.
    pub async fn handle_pending_paste(&mut self, paste: PendingPaste) -> AppResult<()> {
        match paste {
            PendingPaste::Input(text) => {
                // Clear the "Reading clipboard..." notice: attachment-bound
                // pastes replace it with their own status, text into an
                // input leaves none behind.
                self.status = None;
                self.handle_bracketed_paste(text);
                Ok(())
            }
            PendingPaste::Attachment(result) => self.handle_paste_result(result).await,
        }
    }

    /// Handle a completed paste result bound for an attachment (clipboard
    /// image or large pasted text).
    ///
    /// # Errors
    ///
    /// Returns [`TuiError::MissingImageManager`] or [`TuiError::MissingTextManager`]
    /// when the corresponding attachment manager is not initialized, an error from
    /// saving the pasted content, or [`TuiError::EmptyPasteResult`] when the paste
    /// result contains neither image nor text.
    async fn handle_paste_result(
        &mut self,
        result: Result<ClipboardPasteResult, TuiError>,
    ) -> AppResult<()> {
        let result = match result {
            Ok(result) => result,
            Err(e) => {
                self.set_status(format!("Clipboard paste failed: {e}"));
                return Ok(());
            }
        };

        if result.is_empty() {
            self.set_status("Clipboard is empty");
            return Ok(());
        }

        let (image, text) = result.into_parts();
        if let Some(image) = image {
            let manager = self
                .image_manager
                .as_mut()
                .ok_or(TuiError::MissingImageManager)?;
            let path = manager.save(&image).await?;
            self.attachments.push(Attachment::Image {
                path,
                data: image.data,
            });
            self.set_status("Image attached");
            return Ok(());
        }

        if let Some(text) = text {
            // Small single-line pastes insert into the input like a
            // terminal paste; only large or multiline text needs an
            // attachment (and its file).
            if !matches!(self.mode, AppMode::Chat)
                || (!text.contains('\n') && text.len() <= LARGE_PASTE_THRESHOLD)
            {
                self.handle_bracketed_paste(text);
                return Ok(());
            }
            let saved_path = if text.len() > LARGE_PASTE_THRESHOLD {
                let manager = self
                    .text_manager
                    .as_mut()
                    .ok_or(TuiError::MissingTextManager)?;
                Some(manager.save(&text).await?)
            } else {
                None
            };

            self.attachments.push(Attachment::PastedText {
                content: text,
                saved_path,
            });
            self.set_status("Text attached");
            return Ok(());
        }

        Err(TuiError::EmptyPasteResult)
    }

    /// Handle paste (Ctrl+V) - check for image or text in clipboard. The
    /// resolved result is queued via [`App::enqueue_clipboard_result`].
    fn handle_paste(&mut self) {
        if self.clipboard_rx.is_some() {
            self.set_status("Clipboard paste already in progress");
            return;
        }

        let (tx, rx) = async_channel::bounded(1);
        self.clipboard_rx = Some(rx);
        self.pending_attachment_jobs = self.pending_attachment_jobs.saturating_add(1);
        self.set_status("Reading clipboard...");

        spawn(async move {
            let result = match unblock(read_clipboard).await {
                Ok(paste) => {
                    let (image, text) = paste.into_parts();
                    if let Some(image) = image {
                        Ok(ClipboardPasteResult::image(image))
                    } else if let Some(text) = text {
                        if let Some(url) = is_image_url(&text) {
                            match download_image(&url).await {
                                Ok(image) => Ok(ClipboardPasteResult::image(image)),
                                Err(crate::tui::clipboard::ImageError::NotImage(_)) => {
                                    Ok(ClipboardPasteResult::text(text))
                                }
                                Err(error) => Err(TuiError::from(error)),
                            }
                        } else {
                            Ok(ClipboardPasteResult::text(text))
                        }
                    } else {
                        Ok(ClipboardPasteResult::empty())
                    }
                }
                Err(e) => Err(TuiError::from(e)),
            };

            let _ = tx.send(result).await;
        })
        .detach();
    }

    /// Submit the input as a bash-mode command: strip `!`, run it in may's
    /// environment, and show a running terminal block for it.
    fn submit_bash(&mut self) -> Option<SendAction> {
        let command = self.input[1..].trim().to_string();
        self.input.clear();
        self.cursor = 0;
        self.input_char_count = 0;
        if command.is_empty() {
            return None;
        }

        let (masked, had_secret) = self.secrets.mask(&command);
        self.history.push(format!("! {masked}"));
        self.history.save();
        if had_secret {
            self.chat.push_notice(crate::secrets::SECRET_WARNING);
        }

        let id = format!("bash-{}", crate::id::new());
        self.chat.push_tool(
            id.clone(),
            ToolDisplay::Terminal {
                description: String::new(),
                command: format!("! {masked}"),
                mode: aither_sandbox::TerminalExecutionMode::Unsafe,
                status: super::ui::chat::ToolStatus::Running,
                output: None,
                diffs: Vec::new(),
                pinned: None,
            },
        );
        self.scroll = usize::MAX;
        Some(SendAction::RunBash {
            call_id: id,
            command,
        })
    }

    /// Parse `/secret KEY VALUE` and store it in the OS keychain. The value
    /// is cleared from the input without touching input history or the
    /// transcript.
    fn submit_secret(&mut self) -> Option<SendAction> {
        let line = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.input_char_count = 0;
        let rest = line.trim_start()["/secret".len()..].trim();
        let Some((key, value)) = rest.split_once(char::is_whitespace) else {
            self.chat
                .push_error("usage: /secret KEY VALUE — stores VALUE in the OS keychain");
            return None;
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || value.is_empty() {
            self.chat
                .push_error("usage: /secret KEY VALUE — stores VALUE in the OS keychain");
            return None;
        }
        Some(SendAction::StoreSecret {
            key: key.to_string(),
            value: value.to_string(),
        })
    }

    /// Prepare a message for sending, returning the action.
    fn prepare_message(&mut self, delivery_mode: PendingUserMessageMode) -> Option<SendAction> {
        // Add to history before clearing (only if non-empty and not just whitespace)
        if !self.input.trim().is_empty() {
            self.history.push(self.input.clone());
            self.history.save();
        }

        let content = std::mem::take(&mut self.input);
        let attachments = std::mem::take(&mut self.attachments);
        self.cursor = 0;
        self.input_char_count = 0;

        let trimmed = content.trim();
        if trimmed == SlashCommand::Context.name() && attachments.is_empty() {
            return Some(SendAction::ContextReport);
        }
        if trimmed == SlashCommand::ContextDump.name() && attachments.is_empty() {
            return Some(SendAction::ContextDump);
        }
        if trimmed == SlashCommand::Bug.name() && attachments.is_empty() {
            return Some(SendAction::BugReport);
        }
        if let Some(rest) = trimmed.strip_prefix(SlashCommand::Compact.name())
            && attachments.is_empty()
        {
            let focus = rest.trim();
            if focus.is_empty() {
                return Some(SendAction::Compact(None));
            }
            return Some(SendAction::Compact(Some(focus.to_string())));
        }
        if attachments.is_empty()
            && let Some(source) = [
                crate::session::import::ForeignAgent::Claude,
                crate::session::import::ForeignAgent::Codex,
            ]
            .into_iter()
            .find(|source| trimmed == source.rules_command())
        {
            return Some(SendAction::RulesImport(source));
        }
        for (cmd, source) in [
            (
                SlashCommand::ResumeCodex.name(),
                crate::session::import::ForeignAgent::Codex,
            ),
            (
                SlashCommand::ResumeClaude.name(),
                crate::session::import::ForeignAgent::Claude,
            ),
        ] {
            if trimmed == cmd || trimmed.starts_with(&format!("{cmd} ")) {
                let query = trimmed[cmd.len()..].trim().to_string();
                if query.is_empty() && source == crate::session::import::ForeignAgent::Claude {
                    self.open_resume_picker();
                    return None;
                }
                return Some(SendAction::ResumeImport { source, query });
            }
        }
        // `/connect` opens the picker; `/connect <command> [args…]` connects
        // that command directly.
        if attachments.is_empty()
            && let Some(rest) = trimmed.strip_prefix(SlashCommand::Connect.name())
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            if rest.trim().is_empty() {
                self.open_connect_picker();
            } else {
                match super::connect::parse_custom(rest) {
                    Ok((command, args)) => self.connect_custom(command, args),
                    Err(message) => self.chat.push_error(&format!("/connect: {message}")),
                }
            }
            return None;
        }
        // `/model` opens the picker and `/model <query>` searches it —
        // either way the line is a command, never a message to the agent.
        if attachments.is_empty()
            && let Some(rest) = trimmed.strip_prefix(SlashCommand::Model.name())
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return self.open_model_select(rest.trim());
        }
        // `/btw` opens the side-question overlay and `/btw <question>`
        // asks it at once — either way the line is a command, never a
        // message to the agent.
        if attachments.is_empty()
            && let Some(rest) = trimmed.strip_prefix(SlashCommand::Btw.name())
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            let question = rest.trim();
            self.open_btw();
            if !question.is_empty()
                && let AppMode::Btw(state) = &mut self.mode
            {
                state.input = question.to_string();
                self.submit_btw();
            }
            return None;
        }
        // `/new <owner/repo|path>` starts the next session directly on that
        // repository, like `may --repo`; plain `/new` is handled by the menu.
        if attachments.is_empty()
            && let Some(rest) = trimmed.strip_prefix(SlashCommand::New.name())
            && rest.starts_with(char::is_whitespace)
            && !rest.trim().is_empty()
        {
            self.session_manager
                .set_launch(crate::session::SessionLaunch::Repo(rest.trim().to_string()));
            return Some(SendAction::NewSession);
        }
        // `/progress` is a side request too: never a message to the agent.
        if attachments.is_empty()
            && let Some(rest) = trimmed.strip_prefix(SlashCommand::Progress.name())
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            self.open_progress();
            return None;
        }

        self.sent_hidden_context = self.rewind_notice.take();
        let msg = PendingMessage {
            content,
            hidden_context_xml: self.sent_hidden_context.clone(),
            attachments,
            queued_at: jiff::Timestamp::now(),
            delivery_mode,
            display: None,
        };

        if self.processing {
            Some(SendAction::EnqueuePending(msg))
        } else {
            Some(SendAction::Send(msg))
        }
    }

    /// The daemon withdrew the latest turn, stopped before any tool call or
    /// response text: its message and the turn's markers leave the chat.
    /// The client that sent it gets its text and attachments back in the
    /// input — the usual "I sent a typo" recovery. The session history is
    /// reloaded without the message.
    pub fn withdraw_turn(&mut self, content: String, own: bool) {
        let attachments = self.chat.withdraw_last_prompt(&content);
        if own {
            // The withdrawn message took its hidden context with it; a
            // rewind since then supersedes it.
            if self.rewind_notice.is_none() {
                self.rewind_notice = self.sent_hidden_context.take();
            }
            self.input_char_count = content.chars().count();
            self.cursor = self.input_char_count;
            self.input = content;
            self.attachments = attachments.unwrap_or_default();
        }
    }

    /// Called when starting to process a message.
    pub fn start_processing(&mut self, msg: &PendingMessage) {
        self.processing = true;
        self.turn_started_at = Some(std::time::Instant::now());
        self.thinking_start = None;
        self.activity_phase = crate::tui::ui::chat::ActivityPhase::Thinking;
        self.decode_meter.reset();

        // Add user message to chat; the persisted row was appended last,
        // so it sits at the end of session history. A bash-mode run already
        // rendered its own terminal block, so it skips the user bubble.
        if msg.display.is_none() {
            let history_index = self.session_manager.current().map_or(0, |session| {
                session.history.messages().len().saturating_sub(1)
            });
            self.chat
                .push_user(&msg.content, msg.attachments.clone(), history_index);
        }

        // Scroll to bottom
        self.scroll = usize::MAX;
        self.input_visible = true;
    }

    /// Times the running turn from `started_at`, when the daemon started it,
    /// instead of from this client's first sight of it.
    pub fn set_turn_started_at(&mut self, started_at: jiff::Timestamp) {
        let elapsed = jiff::Timestamp::now()
            .duration_since(started_at)
            .unsigned_abs();
        let now = std::time::Instant::now();
        self.turn_started_at = Some(now.checked_sub(elapsed).unwrap_or(now));
    }

    /// Working directory for the status bar: the absolute path with the
    /// user home shown as `~` (component-wise, so `~/foo2` is not mistaken
    /// for home).
    pub fn cwd_label(&self) -> String {
        dirs::home_dir()
            .and_then(|home| {
                self.cwd.strip_prefix(home).ok().map(|rest| {
                    if rest.as_os_str().is_empty() {
                        "~".to_string()
                    } else {
                        format!("~/{}", rest.display())
                    }
                })
            })
            .unwrap_or_else(|| self.cwd.display().to_string())
    }

    /// Re-read the session's canonical `tasks.md` and refresh the todo
    /// block. `async_fs` keeps the filesystem IO off the UI thread. The
    /// sidebar's task tree is refetched from the daemon alongside.
    async fn refresh_todos(&mut self) {
        self.request_sidebar_tasks();
        self.request_sidebar_prs();
        let Some(session) = self
            .session_manager
            .current()
            .filter(|session| session.persisted)
        else {
            return;
        };
        let paths = self
            .session_manager
            .store()
            .session_paths(session.id.as_str(), session.profile_id.as_deref());
        match paths.tasks_content().await {
            Ok(content) => {
                self.chat.set_todos(crate::task_rows::task_rows(
                    content.as_deref().unwrap_or_default(),
                ));
            }
            Err(error) => {
                tracing::warn!(error = %error, "failed to read session tasks.md");
            }
        }
    }

    /// Fetch the current session's PRs for the sidebar from the daemon
    /// (`GET /api/sessions/{id}/prs`); the answer arrives as
    /// [`AppEvent::SidebarPrs`].
    pub fn request_sidebar_prs(&mut self) {
        let Some(session_id) = self
            .session_manager
            .current()
            .map(|session| session.id.as_str().to_string())
        else {
            return;
        };
        let Some(client) = self.btw_daemon.clone() else {
            return;
        };
        let tx = self.event_tx.clone();
        spawn(async move {
            match client
                .get::<Vec<crate::client::WirePr>>(&format!("/api/sessions/{session_id}/prs"))
                .await
            {
                Ok(prs) => {
                    let _ = tx.send(AppEvent::SidebarPrs { session_id, prs }).await;
                }
                Err(error) => {
                    tracing::debug!(%error, "sidebar: failed to fetch PRs");
                }
            }
        });
    }

    /// Fetch the current session's `tasks.md` for the sidebar from the
    /// daemon (`GET /api/sessions/{id}/tasks`); the answer arrives as
    /// [`AppEvent::SidebarTasks`]. Without a daemon the session's file is
    /// read directly.
    pub fn request_sidebar_tasks(&mut self) {
        let Some(session) = self
            .session_manager
            .current()
            .filter(|session| session.persisted)
        else {
            self.sidebar.reset();
            return;
        };
        let session_id = session.id.as_str().to_string();
        if self.sidebar.session_id.as_deref() != Some(session_id.as_str()) {
            // A switched session never shows the previous one's tasks.
            self.sidebar.reset();
        }
        let tx = self.event_tx.clone();
        let daemon = self.btw_daemon.clone();
        let paths = self
            .session_manager
            .store()
            .session_paths(session.id.as_str(), session.profile_id.as_deref());
        spawn(async move {
            #[derive(serde::Deserialize)]
            struct TasksPayload {
                #[serde(default)]
                content: String,
            }
            let content = match &daemon {
                Some(client) => match client
                    .get::<TasksPayload>(&crate::client::session_path(&session_id, "/tasks"))
                    .await
                {
                    Ok(payload) => Some(payload.content).filter(|content| !content.is_empty()),
                    Err(error) => {
                        tracing::debug!(%error, "sidebar: failed to fetch tasks.md");
                        return;
                    }
                },
                None => match paths.tasks_content().await {
                    Ok(content) => content,
                    Err(error) => {
                        tracing::debug!(%error, "sidebar: failed to read tasks.md");
                        return;
                    }
                },
            };
            let title = match &daemon {
                Some(client) => client
                    .get::<crate::client::WireSession>(&format!("/api/sessions/{session_id}"))
                    .await
                    .ok()
                    .map(|session| session.title),
                None => None,
            };
            let _ = tx
                .send(AppEvent::SidebarTasks {
                    session_id,
                    content,
                    title,
                })
                .await;
        })
        .detach();
    }

    /// Look up the issues `numbers` in the session's issue store (its
    /// backend is chosen by `workspace.toml`, as for the agent); the result
    /// arrives as [`AppEvent::SidebarIssues`].
    pub fn request_issue_lookups(&self, session_id: String, numbers: Vec<i64>) {
        if numbers.is_empty() {
            return;
        }
        let Some(client) = self.btw_daemon.clone() else {
            return;
        };
        let tx = self.event_tx.clone();
        spawn(async move {
            let issues: Vec<super::sidebar::IssueBrief> = match client
                .post::<_, Vec<super::sidebar::IssueBrief>>(
                    &format!("/api/sessions/{session_id}/issues/lookup"),
                    &serde_json::json!({ "numbers": numbers }),
                )
                .await
            {
                Ok(issues) => issues,
                Err(error) => {
                    tracing::debug!(%error, "sidebar: issue lookup failed");
                    Vec::new()
                }
            };
            let _ = tx
                .send(AppEvent::SidebarIssues { session_id, issues })
                .await;
        })
        .detach();
    }

    /// Show a tool call that started — live or replayed from history.
    fn show_tool_start(&mut self, id: String, kind: ToolStartKind) {
        let display = self.tool_start_display(kind);
        self.chat.upsert_tool_display(id, display);
    }

    /// Show the result of the tool call `id` — live or replayed from history.
    fn show_tool_end(&mut self, id: &str, result: &aither_core::llm::ToolResult) {
        match parse_tool_end(result) {
            ToolEndKind::Output(out) => {
                let image = out
                    .image
                    .or_else(|| self.imagine_result_image(id, &out.text));
                let status = out.failure.map_or(
                    super::ui::chat::ToolStatus::Done,
                    super::ui::chat::ToolStatus::Failed,
                );
                self.chat
                    .update_tool_with_output(id, status, Some(out.text.as_str()));
                if let Some(image) = image
                    && let Some(image) = self.install_tool_image(id, image)
                {
                    self.chat.set_tool_image(id, image);
                }
            }
            ToolEndKind::Error(err) => {
                self.chat.update_tool_with_output(
                    id,
                    super::ui::chat::ToolStatus::Failed(err),
                    None,
                );
            }
        }
    }

    /// Map a parsed tool-call start into its stream display. Shared by the
    /// canonical `ToolCallStart` path and the progressive `ToolCallDelta`
    /// projection — a partial parse only differs in fields still empty.
    fn tool_start_display(&self, kind: ToolStartKind) -> ToolDisplay {
        match kind {
            ToolStartKind::Terminal {
                description,
                script,
                mode,
            } => ToolDisplay::Terminal {
                description,
                command: script,
                mode,
                status: super::ui::chat::ToolStatus::Running,
                output: None,
                diffs: Vec::new(),
                pinned: None,
            },
            ToolStartKind::Stdin { task_id, input } => {
                let mut detail: Vec<String> = Vec::new();
                if let Some(target) = self.resolve_task_label(&task_id) {
                    detail.push(format!("→ {target}"));
                }
                if input.lines().count() > 1 {
                    detail.extend(input.lines().map(str::to_string));
                }
                ToolDisplay::Action {
                    chip: "stdin".to_string(),
                    task_id: Some(task_id),
                    summary: input.lines().next().unwrap_or_default().to_string(),
                    detail,
                    status: super::ui::chat::ToolStatus::Running,
                    output: None,
                    pinned: None,
                }
            }
            ToolStartKind::JobControl { action, task_id } => ToolDisplay::Action {
                chip: action.chip().to_string(),
                task_id: Some(task_id.clone()),
                summary: self
                    .resolve_task_label(&task_id)
                    .unwrap_or_else(|| "background task".to_string()),
                detail: Vec::new(),
                status: super::ui::chat::ToolStatus::Running,
                output: None,
                pinned: None,
            },
            ToolStartKind::Generic {
                name,
                summary,
                arguments,
            } => ToolDisplay::Action {
                // A compaction takes no arguments and has nothing to expand.
                detail: if name == super::agent_event_adapter::COMPACT_TOOL {
                    Vec::new()
                } else {
                    pretty_arguments(&arguments)
                },
                chip: name,
                task_id: None,
                summary,
                status: super::ui::chat::ToolStatus::Running,
                output: None,
                pinned: None,
            },
        }
    }

    /// The model response in flight is being sent again: drop what it
    /// streamed, which the replacement does not continue.
    pub fn retract_response(&mut self) {
        self.chat.retract_response();
        self.pending_tool_calls.retain(|_, pending| pending.started);
        self.thinking_start = None;
    }

    /// Handle an agent event. Returns true if stream ended.
    pub async fn handle_agent_event(&mut self, result: Result<TimedEvent, AgentError>) -> bool {
        match result {
            Ok(TimedEvent { event, .. }) => {
                match event {
                    AgentEvent::Text(chunk) => {
                        self.activity_phase = crate::tui::ui::chat::ActivityPhase::Working;
                        self.chat.collapse_thinking();
                        self.thinking_start = None;
                        self.decode_meter.note_output(chunk.len());
                        self.chat.push_text(&chunk);
                        self.input_visible = true;
                    }
                    AgentEvent::ReplyTranslated { text, language } => {
                        self.chat.set_translation(text, &language);
                    }
                    AgentEvent::Reasoning(chunk) => {
                        self.activity_phase = crate::tui::ui::chat::ActivityPhase::Thinking;
                        self.decode_meter.note_output(chunk.len());
                        if self.thinking_start.is_none() {
                            self.thinking_start = Some(std::time::Instant::now());
                        }
                        self.chat.push_thinking(&chunk);
                        // Update thinking time
                        if let Some(start) = self.thinking_start {
                            self.chat.update_thinking(start.elapsed().as_secs_f64());
                        }
                    }
                    AgentEvent::ToolCallDelta {
                        id,
                        name,
                        arguments_fragment,
                    } => {
                        self.activity_phase = crate::tui::ui::chat::ActivityPhase::Working;
                        self.decode_meter.note_output(arguments_fragment.len());
                        let kind = {
                            let pending = self.pending_tool_calls.entry(id.clone()).or_default();
                            if !name.is_empty() {
                                pending.name.clone_from(&name);
                            }
                            pending.arguments.push_str(&arguments_fragment);
                            if pending.started || pending.name.is_empty() {
                                None
                            } else {
                                Some(parse_tool_start_partial(&pending.name, &pending.arguments))
                            }
                        };
                        if let Some(kind) = kind {
                            // Materialize the call the moment its name is
                            // known and grow its display as arguments
                            // stream in; the canonical start re-parses
                            // and replaces it below.
                            self.chat.collapse_thinking();
                            self.chat.end_text();
                            self.thinking_start = None;
                            let display = self.tool_start_display(kind);
                            self.chat.upsert_tool_display(id, display);
                        }
                    }
                    AgentEvent::ToolCallStart {
                        id,
                        name,
                        arguments,
                    } => {
                        self.activity_phase = crate::tui::ui::chat::ActivityPhase::Working;
                        self.chat.collapse_thinking();
                        self.chat.end_text();
                        self.thinking_start = None;
                        // Args that never streamed as deltas still count as
                        // decoded output.
                        let streamed = self
                            .pending_tool_calls
                            .get(&id)
                            .map_or(0, |pending| pending.arguments.len());
                        self.decode_meter
                            .note_output(arguments.len().saturating_sub(streamed));
                        self.decode_meter.note_boundary();
                        if let Some(pending) = self.pending_tool_calls.get_mut(&id) {
                            pending.started = true;
                        }
                        self.show_tool_start(id, parse_tool_start(&name, &arguments));
                    }
                    AgentEvent::ToolCallEnd { id, result, .. } => {
                        // Back to waiting on the model for the next burst.
                        self.activity_phase = crate::tui::ui::chat::ActivityPhase::Thinking;
                        self.show_tool_end(&id, &result);
                        self.live_reads.remove(&id);
                        self.pending_tool_calls.remove(&id);
                        self.request_file_diff(&id);
                        self.refresh_todos().await;
                    }
                    AgentEvent::BuiltInToolCall { id, tool, detail } => {
                        // Provider-hosted call (server-side web search etc.):
                        // visible like a tool call, executed by the provider.
                        self.activity_phase = crate::tui::ui::chat::ActivityPhase::Working;
                        self.chat.collapse_thinking();
                        self.chat.end_text();
                        self.thinking_start = None;
                        self.decode_meter.note_boundary();
                        self.chat.push_builtin(id, tool, detail);
                    }
                    AgentEvent::BuiltInToolResult { id, tool, result } => {
                        // The provider reports the call done; the model call
                        // that follows is thinking again.
                        self.activity_phase = crate::tui::ui::chat::ActivityPhase::Thinking;
                        self.decode_meter.note_boundary();
                        self.chat.finish_builtin(&id, &tool, result);
                    }
                    // A reply that ends without tool calls may be followed
                    // by another in the same run (the harness re-prompting,
                    // a background command finishing): it starts a block of
                    // its own instead of being glued onto this one.
                    AgentEvent::TurnComplete { has_tool_calls, .. } => {
                        if !has_tool_calls {
                            self.chat.end_text();
                            self.decode_meter.note_boundary();
                        }
                        self.chat.seal_response();
                    }
                    AgentEvent::Complete { .. } => {
                        // Text was already streamed via AgentEvent::Text events
                        // Just finalize the message
                        self.chat.collapse_thinking();
                        self.chat.finish_running_calls();
                        self.chat.end_text();
                        self.decode_meter.note_boundary();
                        self.pending_tool_calls.clear();
                        self.input_visible = true;
                        // The turn may have edited tasks.md without a tool
                        // call ending after the edit (or in a foreign turn).
                        self.refresh_todos().await;
                        // Signal that processing is complete
                        return true;
                    }
                    AgentEvent::Usage(u) => {
                        self.decode_meter.note_usage(u.completion_tokens);
                        self.usage = Some(u);
                    }
                    AgentEvent::CompactionRecovery {
                        error,
                        alerts_removed,
                        outputs_spilled,
                    } => {
                        self.chat
                            .push_compaction_recovery(&error, alerts_removed, outputs_spilled);
                    }
                    // Lifecycle events are used for runtime coordination, not rendered.
                    _ => {}
                }
                false
            }
            Err(e) => {
                tracing::error!("Agent error: {}", e);
                self.decode_meter.note_boundary();
                self.pending_tool_calls.clear();
                // Display error to user
                self.chat.collapse_thinking();
                self.chat.finish_running_calls();
                self.chat.push_error(&format!("{e}"));
                self.input_visible = true;
                true
            }
        }
    }

    /// Called when agent stream ends.
    pub const fn finish_processing(&mut self) {
        self.processing = false;
    }

    /// Get next queued message if any.
    pub fn next_queued(&mut self) -> Option<PendingMessage> {
        if !self.processing && !self.queue.is_empty() {
            self.queue.dequeue()
        } else {
            None
        }
    }

    /// The suggested prompt when it belongs to the attached session and
    /// the input is empty — the only state in which ghost text shows.
    fn own_visible_suggestion(&self) -> Option<&str> {
        visible_suggestion(
            self.prompt_suggestion.as_ref(),
            &self.input,
            self.session_manager
                .current()
                .map(|session| session.id.as_str()),
        )
    }

    /// Mirror the daemon's pending-message queue from the last runtime
    /// snapshot, so an enqueued message keeps a
    /// `[steer]`/`[queue]` preview until the turn it starts renders it.
    pub fn sync_queue_from_runtime(&mut self) {
        let runtime = &self.session_runtime;
        self.queue.replace(
            runtime
                .pending_immediate
                .as_ref()
                .map(PendingMessage::from_wire),
            runtime
                .queued_messages
                .iter()
                .map(PendingMessage::from_wire)
                .collect(),
        );
    }

    /// Get current session ID for display.
    pub fn session_id_display(&self) -> String {
        self.session_manager
            .current()
            .map_or_else(|| "none".to_string(), |s| s.id.to_string())
    }

    /// The session's effective reasoning effort: the rung it selected, the
    /// last rung selected anywhere (`config.last_effort`), or medium before
    /// any selection — "default" is never a provider-decided level.
    pub fn effective_effort(&self) -> &str {
        self.current_effort
            .as_deref()
            .or(self.config.last_effort.as_deref())
            .unwrap_or(crate::models::DEFAULT_EFFORT)
    }

    /// The effort rungs `/effort` offers for the current model: the
    /// family's served rungs when the catalog reports them, else the
    /// provider protocol's generic ladder.
    pub fn effort_ladder_for_current(&self) -> Vec<&'static str> {
        let selector = self
            .current_model
            .as_deref()
            .and_then(|model| model.parse::<crate::ModelSelector>().ok());
        selector.map_or_else(
            || crate::models::provider_effort_ladder(self.current_provider_type()).to_vec(),
            |selector| {
                crate::models::effort_ladder(
                    &self.family_effort_rungs,
                    selector.provider_id(),
                    selector.model_id(),
                    self.current_provider_type(),
                )
            },
        )
    }

    /// The ladder index of [`Self::effective_effort`], falling back to the
    /// medium rung when the effective level is not on this provider's ladder.
    pub fn effort_ladder_index(&self) -> usize {
        self.effort_ladder
            .iter()
            .position(|rung| *rung == self.effective_effort())
            .or_else(|| {
                self.effort_ladder
                    .iter()
                    .position(|rung| *rung == crate::models::DEFAULT_EFFORT)
            })
            .unwrap_or(0)
    }

    /// Get model name for display.
    pub fn model_display(&self) -> String {
        // Show the actual model being used - no fallback
        self.current_model.as_ref().map_or_else(
            || "No model".to_string(),
            |model| {
                model.parse::<crate::ModelSelector>().map_or_else(
                    |_| model.clone(),
                    |selector| crate::models::effort_family_base(selector.model_id()),
                )
            },
        )
    }

    /// Set the current model being used.
    pub fn set_current_model(&mut self, model: &str) {
        // The window belongs to the model: the daemon sends the new one
        // with the next usage.
        if self.current_model.as_deref() != Some(model) {
            self.context_window = None;
        }
        self.current_model = Some(model.to_string());
    }

    /// Get context usage information for display.
    /// Returns (`prompt_tokens`, `context_window`) if available.
    pub fn context_usage(&self) -> Option<(u32, u32)> {
        let usage = self.usage.as_ref()?;
        let prompt_tokens = usage.prompt_tokens?;
        let context_window = self.context_window?;
        Some((prompt_tokens, context_window))
    }
}

/// An unknown tool's argument payload as expanded-body lines: the raw text
/// when it fits on one line, pretty-printed JSON otherwise.
fn pretty_arguments(arguments: &str) -> Vec<String> {
    if arguments.lines().count() <= 1 {
        return arguments.lines().map(str::to_string).collect();
    }
    serde_json::from_str::<serde_json::Value>(arguments).map_or_else(
        |_| arguments.lines().map(str::to_string).collect(),
        |value| {
            serde_json::to_string_pretty(&value).map_or_else(
                |_| arguments.lines().map(str::to_string).collect(),
                |pretty| pretty.lines().map(str::to_string).collect(),
            )
        },
    )
}

/// Actions returned by event handling.
#[derive(Debug)]
pub enum SendAction {
    /// User wants to quit.
    Quit,
    /// User wants the session's running agent stopped, then to quit.
    StopAgentAndQuit,
    /// User wants to create a new session.
    NewSession,
    /// User wants to load an existing session.
    LoadSession(SessionId),
    /// Send this message to the agent.
    Send(PendingMessage),
    /// Persist a pending message for later dispatch.
    EnqueuePending(PendingMessage),
    /// Ask the daemon for a catch-up recap of the session.
    Recap,
    /// Flip the session's yolo switch.
    ToggleYolo,
    /// Mark the user asleep for every session; this session's agent says
    /// good night.
    Sleep,
    /// End the user's sleep state (`/sleep off`).
    Wake,
    /// Show an open question of the session in the answer modal.
    OpenQuestions,
    /// Reload the session's history, which turns this client missed
    /// changed.
    ReloadHistory,
    /// Raise a desktop notification through the terminal: a popup of some
    /// session waits on the user.
    DesktopNotification(Box<crate::notification::UserNotification>),
    /// Submit an ask-user response.
    RespondAskUser {
        /// ID of the pending ask-user request being answered.
        request_id: String,
        /// The prompt a run is blocked on, or `None` for an open question.
        request: Option<crate::ask_user::AskUserRequest>,
        /// The user's answers, one entry per question.
        answers: Vec<crate::ask_user::QuestionAnswer>,
    },
    /// Start GitHub Copilot OAuth authentication.
    StartCopilotAuth,
    /// Open Copilot auth URL in browser and copy code to clipboard.
    OpenCopilotAuthUrl {
        /// OAuth URL to open in the browser.
        url: String,
        /// Device-flow code to copy to the clipboard.
        code: String,
    },
    /// Start `ChatGPT` subscription OAuth authentication.
    StartChatgptAuth,
    /// Open the `ChatGPT` authorization URL in a browser.
    OpenChatgptAuthUrl {
        /// Authorization URL to open in the browser.
        url: String,
        /// Device-flow user code to copy to the clipboard (empty for browser flow).
        code: String,
    },
    /// Start xAI Grok OAuth authentication.
    StartGrokAuth,
    /// Open the xAI verification URL in a browser and copy the user code.
    OpenGrokAuthUrl {
        /// Verification URL to open in the browser.
        url: String,
        /// Device-flow user code to copy to the clipboard.
        code: String,
    },
    /// Start `OpenRouter` OAuth authentication.
    StartOpenrouterAuth,
    /// Open the `OpenRouter` authorization URL in a browser.
    OpenOpenrouterAuthUrl {
        /// Authorization URL to open in the browser.
        url: String,
    },
    /// Submit the pasted `OpenRouter` callback URL, code, or `sk-or-` key.
    SubmitOpenrouterCode {
        /// The pasted text to resolve.
        input: String,
    },
    /// Switch to a different model for this session.
    SwitchModel(String),
    /// Set the session's reasoning effort (`None` clears to provider default).
    SetEffort(Option<String>),
    /// Compact the conversation history with optional focus hint.
    Compact(Option<String>),
    /// Write the recent-log snapshot to `~/.may/bugs`.
    BugReport,
    /// Measure and display context-window usage.
    ContextReport,
    /// Aggregate and display provider usage and cost for `/usage`.
    UsageReport,
    /// Dump the verbatim assembled context to the session's outputs dir.
    ContextDump,
    /// MCP server management actions.
    McpAdd {
        /// Name of the MCP server to add.
        name: String,
        /// Command used to launch the server.
        command: String,
        /// Arguments passed to the launch command.
        args: Vec<String>,
    },
    /// Remove an MCP server.
    McpRemove {
        /// Name of the MCP server to remove.
        name: String,
    },
    /// Reconnect an MCP server.
    McpReconnect {
        /// Name of the MCP server to reconnect.
        name: String,
    },
    /// SSH server management actions.
    SshAdd {
        /// Display name for the SSH server.
        name: String,
        /// SSH destination, e.g. `user@host`.
        target: String,
    },
    /// Remove an SSH server.
    SshRemove {
        /// Name of the SSH server to remove.
        name: String,
    },
    /// Answer the launch-directory grant prompt.
    RespondLaunchGrant {
        /// The directory being decided.
        dir: std::path::PathBuf,
        /// `workspace.toml` the decision is written to.
        config_path: std::path::PathBuf,
        /// Whether the user granted read/write access to the launch dir.
        approved: bool,
    },
    /// Answer the repo-trust prompt.
    RespondRepoTrust {
        /// The repository's worktree root.
        worktree: std::path::PathBuf,
        /// Whether the remote is GitHub-backed.
        kind: crate::trust::RepoKind,
        /// `workspace.toml` the decision is written to.
        config_path: std::path::PathBuf,
        /// The picked level; `None` leaves the decision unchanged.
        level: Option<crate::trust::TrustLevel>,
    },
    /// Submit a workspace approval response.
    RespondWorkspace {
        /// ID of the pending workspace request being answered.
        request_id: String,
        /// The in-process request that was presented to the user; `None`
        /// when the daemon holds it.
        request: Option<crate::workspace_request::WorkspaceRequest>,
        /// Whether the user approved the request.
        approved: bool,
    },
    /// Submit a permission approval response.
    RespondPermission {
        /// ID of the pending permission request being answered.
        request_id: String,
        /// Whether the user approved the permission request.
        approved: bool,
    },
    /// Skill management actions.
    SkillInstall {
        /// GitHub repository (`owner/name`) to install the skill from.
        repo: String,
    },
    /// Delete an installed skill.
    SkillDelete {
        /// Directory name of the skill to delete.
        dir_name: String,
    },
    /// Import a Codex/Claude agent's global rules and skills: copy the
    /// skills and open the session that rewrites the rules.
    RulesImport(crate::session::import::ForeignAgent),
    /// Import a Codex/Claude transcript into a new session.
    ResumeImport {
        /// Which agent's transcript to parse.
        source: crate::session::import::ForeignAgent,
        /// Session id, id prefix, or transcript path.
        query: String,
    },
    /// Rewind the conversation to before the user message at
    /// `history_index`, optionally restoring the code too.
    Rewind {
        /// Position of the target user message in session history.
        history_index: usize,
        /// Whether to restore tracked files to the prompt's checkpoint.
        restore_code: bool,
    },
    /// Run a bash-mode (`!`) command in may's environment.
    RunBash {
        /// Chat element id (`bash-*`) already rendering this run.
        call_id: String,
        /// The raw command to execute.
        command: String,
    },
    /// Store a secret in the OS keychain and refresh the session env file.
    StoreSecret {
        /// Environment variable name.
        key: String,
        /// Secret value — never persisted to transcript or input history.
        value: String,
    },
}

fn btw_message(role: &str, content: impl Into<String>) -> crate::daemon::BtwMessage {
    crate::daemon::BtwMessage {
        role: role.to_string(),
        content: content.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::utf8_complete_len;
    use super::{
        AgentEvent, App, AppEvent, KeyCode, KeyModifiers, MayConfig, MessageRole, SendAction,
        TimedEvent,
    };
    use crate::session::SessionManager;
    use crate::tui::ui::chat::ChatElement;

    /// An app over a scratch May directory with one attached session,
    /// returning the session id. The directory must outlive the app.
    async fn app_with_session(dir: &std::path::Path) -> (App, String) {
        let mut sessions = SessionManager::at(dir.to_path_buf())
            .await
            .expect("session manager");
        let id = sessions
            .create_session_in_profile(None, None)
            .expect("session")
            .id
            .as_str()
            .to_string();
        let (tx, _rx) = async_channel::unbounded();
        let app = App::with_parts(MayConfig::default(), sessions, tx, None, None);
        (app, id)
    }

    fn suggest(app: &mut App, session_id: &str, text: &str) {
        app.handle_event(AppEvent::PromptSuggestion {
            request_id: 1,
            session_id: session_id.to_string(),
            text: Some(text.to_string()),
        })
        .expect("suggestion event");
    }

    fn summary(id: &str, archived: bool, working: bool) -> crate::session::SessionSummary {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "title": format!("title of {id}"),
            "pinned": false,
            "archived": archived,
            "message_count": 1,
            "content_preview": "",
            "working": working,
        }))
        .expect("summary")
    }

    fn session_screen(app: &mut App) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).expect("terminal");
        let frame = terminal
            .draw(|frame| crate::tui::ui::render(frame, app))
            .expect("draw");
        crate::tui::ui::test_screen::buffer_screen(frame.buffer)
    }

    const BLOCK_BG: ratatui::style::Color = ratatui::style::Color::Rgb(40, 40, 48);
    const BOX_DRAWING: [char; 8] = ['┌', '┐', '└', '┘', '─', '│', '╭', '╯'];

    /// Draws `app` at `width`×`height`, returning the buffer.
    fn draw_buffer(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("terminal");
        terminal
            .draw(|frame| crate::tui::ui::render(frame, app))
            .expect("draw")
            .buffer
            .clone()
    }

    fn row_of(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    /// The jobs panel and the command list are filled blocks: the title is
    /// the block's first row, no box-drawing character appears, and every
    /// cell of the block carries the block colour.
    #[tokio::test]
    async fn popups_are_filled_blocks_without_borders() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;
        app.band_bg = Some(BLOCK_BG);

        app.mode = crate::tui::AppMode::Jobs;
        let buffer = draw_buffer(&mut app, 100, 30);
        let title = (0..30)
            .find(|&y| row_of(&buffer, y).contains("Jobs & subagents"))
            .expect("the panel title row");
        let panel_rows = title..title + 6;
        for y in panel_rows {
            let row = row_of(&buffer, y);
            assert!(!row.contains(BOX_DRAWING), "row {y}: {row:?}");
            assert!(
                (1..99).all(|x| buffer[(x, y)].bg == BLOCK_BG),
                "row {y} is filled: {row:?}"
            );
        }
        assert!(
            row_of(&buffer, title).trim_start().starts_with("Jobs"),
            "the title is inside the block"
        );

        app.mode = crate::tui::AppMode::Chat;
        app.completion.start_slash("");
        let buffer = draw_buffer(&mut app, 100, 30);
        let title = (0..30)
            .find(|&y| row_of(&buffer, y).contains("Commands"))
            .expect("the list title row");
        let help = (title..30)
            .find(|&y| row_of(&buffer, y).contains("/help"))
            .expect("a command row");
        for y in title..=help {
            let row = row_of(&buffer, y);
            assert!(!row.contains(BOX_DRAWING), "row {y}: {row:?}");
        }
        assert!(
            (title..=help)
                .any(|y| (1..99).any(|x| buffer[(x, y)].bg == ratatui::style::Color::Blue)),
            "the selected row is shown by its background"
        );
    }

    /// The sidebar is one block: no divider column, its whole area filled
    /// with the block colour, which the chat beside it does not use.
    #[tokio::test]
    async fn the_sidebar_is_a_filled_block_without_a_divider() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;
        app.band_bg = Some(BLOCK_BG);
        let (width, height) = (160u16, 30u16);
        assert!(app.sidebar.visible_at(width));
        let buffer = draw_buffer(&mut app, width, height);
        let sidebar_width = crate::tui::sidebar::SidebarState::width_for(width);
        let left = width - sidebar_width;
        for y in 0..height {
            assert!(
                (left..width).all(|x| buffer[(x, y)].bg == BLOCK_BG),
                "sidebar row {y} is filled"
            );
            assert!(
                (left..width).all(|x| !BOX_DRAWING
                    .contains(&buffer[(x, y)].symbol().chars().next().unwrap_or(' '))),
                "no divider on row {y}: {:?}",
                row_of(&buffer, y)
            );
        }
        // Inner padding: the first column of the block stays empty.
        assert!((0..height).all(|y| buffer[(left, y)].symbol() == " "));
        // The chat beside it is not painted with the block colour.
        assert_ne!(buffer[(left - 1, 0)].bg, BLOCK_BG);
    }

    #[tokio::test]
    async fn a_working_session_reads_working_in_the_session_list() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;
        app.session_list = vec![summary("busy", false, true), summary("calm", false, false)];
        app.mode = crate::tui::AppMode::SessionSelect;

        let screen = session_screen(&mut app).join("\n");
        let busy = screen
            .lines()
            .find(|line| line.contains("title of busy"))
            .expect("busy session listed");
        assert!(busy.contains("Working..."), "{busy}");
        let calm = screen
            .lines()
            .find(|line| line.contains("title of calm"))
            .expect("calm session listed");
        assert!(!calm.contains("Working..."), "{calm}");
    }

    #[tokio::test]
    async fn the_archive_view_titles_the_popup() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;
        app.session_view = crate::tui::state::SessionView::Archive;
        app.session_list = vec![summary("old", true, false)];
        app.mode = crate::tui::AppMode::SessionSelect;

        let screen = session_screen(&mut app).join("\n");
        assert!(screen.contains("Archived sessions"), "{screen}");
        assert!(screen.contains("title of old"), "{screen}");

        app.session_view = crate::tui::state::SessionView::Recent;
        let screen = session_screen(&mut app).join("\n");
        assert!(!screen.contains("Archived sessions"), "{screen}");
    }

    fn press(app: &mut App, code: KeyCode) -> Option<SendAction> {
        app.handle_key(code, KeyModifiers::NONE).expect("key")
    }

    /// A withdrawn message takes its hidden context with it; the sender puts
    /// the rewind notice back so the message it sends again carries it.
    #[tokio::test]
    async fn a_withdrawn_message_returns_its_rewind_notice_to_the_sender() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;
        let text = "你好 こんにちは 안녕하세요";
        let notice = crate::xml::XmlBlock::parse("The conversation was rewound.".to_string())
            .expect("notice parses");
        app.rewind_notice = Some(notice.clone());
        for c in text.chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let Some(SendAction::Send(sent)) = press(&mut app, KeyCode::Enter) else {
            panic!("Enter sends the message");
        };
        assert_eq!(sent.hidden_context_xml, Some(notice.clone()));
        assert!(app.rewind_notice.is_none(), "the send took the notice");

        // Another client's withdrawn turn leaves this client's state alone.
        app.withdraw_turn("someone else's".to_string(), false);
        assert!(app.rewind_notice.is_none());

        app.withdraw_turn(text.to_string(), true);
        assert_eq!(app.input, text);
        assert_eq!(app.rewind_notice, Some(notice.clone()));

        let Some(SendAction::Send(again)) = press(&mut app, KeyCode::Enter) else {
            panic!("Enter sends the restored message");
        };
        assert_eq!(again.content, text);
        assert_eq!(again.hidden_context_xml, Some(notice));
    }

    #[tokio::test]
    async fn an_unbound_modified_character_is_not_text() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;
        for modifier in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
        ] {
            for c in ['a', 'k', 'u'] {
                assert!(
                    app.handle_key(KeyCode::Char(c), modifier)
                        .expect("key")
                        .is_none()
                );
            }
        }
        assert_eq!(app.input, "", "no modified key types its letter");

        app.handle_key(KeyCode::Char('A'), KeyModifiers::SHIFT)
            .expect("key");
        press(&mut app, KeyCode::Char('b'));
        assert_eq!(app.input, "Ab", "plain and shifted characters still type");
    }

    /// An Enter carrying Ctrl or Cmd is a link-open request, not a submit:
    /// with no link under the pointer it does nothing.
    #[tokio::test]
    async fn a_modified_enter_never_sends() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;
        for c in "draft".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        for modifier in [KeyModifiers::SUPER, KeyModifiers::CONTROL] {
            assert!(
                app.handle_key(KeyCode::Enter, modifier)
                    .expect("key")
                    .is_none()
            );
        }
        assert_eq!(app.input, "draft", "a modified Enter sends nothing");
    }

    #[tokio::test]
    async fn a_replayed_turn_is_timed_from_the_daemons_start_and_matched_by_row_id() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;

        app.set_turn_started_at(jiff::Timestamp::now() - std::time::Duration::from_secs(90));
        let elapsed = app.turn_started_at.expect("set").elapsed().as_secs();
        assert!(
            (90..95).contains(&elapsed),
            "counts from the start: {elapsed}"
        );

        let mut prompt = crate::session::ChatMessage::user("hello");
        prompt.id = Some(7);
        app.session_manager.record_unpersisted(prompt);
        let session = app.session_manager.current().expect("session");
        assert!(session.holds_message(7));
        assert!(!session.holds_message(8));
    }

    #[tokio::test]
    async fn permission_sheet_opens_on_the_persisted_trust_level() {
        use crate::trust::{RepoKind, TrustLevel};

        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;
        let repo = tempfile::tempdir().expect("repo");
        let status = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(repo.path())
            .status()
            .expect("git init");
        assert!(status.success());
        app.cwd = repo.path().to_path_buf();
        let home = crate::session::MayHomePaths::new(dir.path().to_path_buf());
        let config_path = app.session_manager.current_profile_id().map_or_else(
            || home.workspace_config(),
            |id| home.profile_home(id).workspace_config(),
        );
        std::fs::create_dir_all(config_path.parent().expect("parent")).expect("profile dir");
        let kind = RepoKind::Github {
            owner_repo: "acme/app".to_string(),
        };
        crate::trust::persist(&config_path, repo.path(), Some(&kind), TrustLevel::Local)
            .expect("persist");

        app.open_permission_gate();
        let super::AppMode::RepoTrust(state) = &app.mode else {
            panic!("/permission inside a repository opens the repo-trust sheet");
        };
        let cursor = state.ask.question_states[0].cursor;
        assert_eq!(
            crate::trust::option_level(&state.ask.questions[0].option[cursor]),
            Some(TrustLevel::Local),
            "the sheet highlights the level in force, not the first option"
        );
    }

    fn last_error(app: &App) -> Option<&str> {
        app.chat
            .elements()
            .iter()
            .rev()
            .find_map(|element| match element {
                ChatElement::Error { message } => Some(message.as_str()),
                _ => None,
            })
    }

    #[tokio::test]
    async fn a_refused_daemon_request_is_a_chat_error_not_a_failure() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _) = app_with_session(dir.path()).await;
        let refused: Result<(), crate::tui::runtime::TuiRuntimeError> =
            Err(crate::client::DaemonClientError::Status {
                path: "/api/permissions/respond".to_string(),
                status: hyper::StatusCode::BAD_REQUEST,
                message: "permission request not found or already resolved".to_string(),
            }
            .into());
        assert!(app.settle(refused).is_none());
        let shown = last_error(&app).expect("the refusal is shown in the chat");
        assert!(shown.contains("already resolved"), "{shown}");
        // The app still handles input afterwards.
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.input, "a");
    }

    #[tokio::test]
    async fn a_failed_session_operation_is_a_chat_error_not_a_failure() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _) = app_with_session(dir.path()).await;
        let failed: Result<(), crate::session::SessionError> =
            Err(crate::session::SessionError::NotFound(
                "history position 3 is not a user message".to_string(),
            ));
        assert!(app.settle(failed).is_none());
        let shown = last_error(&app).expect("the failure is shown in the chat");
        assert!(shown.contains("is not a user message"), "{shown}");
        assert_eq!(
            app.settle(Ok::<_, crate::session::SessionError>(7)),
            Some(7)
        );
    }

    #[tokio::test]
    async fn a_suggestion_frame_shows_for_the_attached_session_only() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, id) = app_with_session(dir.path()).await;
        suggest(&mut app, "another-session", "not mine");
        assert_eq!(app.own_visible_suggestion(), None);
        suggest(&mut app, &id, "run the tests");
        assert_eq!(app.own_visible_suggestion(), Some("run the tests"));
        app.handle_event(AppEvent::PromptSuggestion {
            request_id: 2,
            session_id: id,
            text: None,
        })
        .expect("clear");
        assert_eq!(
            app.own_visible_suggestion(),
            None,
            "a cleared frame hides it"
        );
    }

    #[tokio::test]
    async fn tab_lifts_the_suggestion_into_the_draft_without_sending() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, id) = app_with_session(dir.path()).await;
        suggest(&mut app, &id, "run the tests");
        let action = press(&mut app, KeyCode::Tab);
        assert!(action.is_none(), "accepting never submits");
        assert_eq!(app.input, "run the tests");
        assert_eq!(app.cursor, app.input_char_count);
        assert!(app.prompt_suggestion.is_none(), "consumed");
        press(&mut app, KeyCode::Char('!'));
        assert_eq!(app.input, "run the tests!", "the lifted text is editable");
    }

    #[tokio::test]
    async fn esc_dismisses_the_suggestion_and_leaves_the_draft_empty() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, id) = app_with_session(dir.path()).await;
        suggest(&mut app, &id, "run the tests");
        assert!(press(&mut app, KeyCode::Esc).is_none());
        assert!(app.prompt_suggestion.is_none());
        assert_eq!(app.input, "");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "", "nothing left to accept");
    }

    #[tokio::test]
    async fn typing_hides_the_suggestion() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, id) = app_with_session(dir.path()).await;
        suggest(&mut app, &id, "run the tests");
        press(&mut app, KeyCode::Char('h'));
        assert_eq!(app.input, "h");
        assert_eq!(app.own_visible_suggestion(), None, "hidden while typing");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "h", "Tab never overwrites a typed draft");
    }

    #[tokio::test]
    async fn recap_command_asks_the_daemon_and_the_frame_adds_a_card() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, id) = app_with_session(dir.path()).await;
        for c in "/recap".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let mut action = press(&mut app, KeyCode::Enter);
        if action.is_none() {
            action = press(&mut app, KeyCode::Enter);
        }
        assert!(matches!(action, Some(SendAction::Recap)));

        app.handle_event(AppEvent::Recap {
            session_id: id,
            state: "ready".to_string(),
            summary: Some("Tests pass.".to_string()),
            next_action: Some("Merge it.".to_string()),
        })
        .expect("recap frame");
        assert!(matches!(
            app.chat.elements().last(),
            Some(ChatElement::Recap { summary, next_action })
                if summary == "Tests pass." && next_action.as_deref() == Some("Merge it.")
        ));
    }

    #[tokio::test]
    async fn the_status_window_is_the_one_the_daemon_sent() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, id) = app_with_session(dir.path()).await;
        app.usage = Some(aither_core::llm::Usage {
            prompt_tokens: Some(1_000),
            ..aither_core::llm::Usage::default()
        });
        // A model the table lacks: the client looks nothing up.
        app.set_current_model("relay::relay-only-model");
        assert_eq!(
            app.context_usage(),
            None,
            "no window before the daemon sends one"
        );

        app.handle_event(AppEvent::ContextWindow {
            session_id: "another-session".to_string(),
            window: 1,
        })
        .expect("window frame");
        assert_eq!(
            app.context_usage(),
            None,
            "another session's window is ignored"
        );

        app.handle_event(AppEvent::ContextWindow {
            session_id: id,
            window: 262_144,
        })
        .expect("window frame");
        assert_eq!(app.context_usage(), Some((1_000, 262_144)));

        app.set_current_model("relay::another-model");
        assert_eq!(
            app.context_usage(),
            None,
            "a new model drops the old window"
        );
    }

    #[tokio::test]
    async fn usage_events_keep_the_latest_request_not_their_sum() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;
        let usage = |prompt_tokens| {
            Ok(TimedEvent {
                at: "2026-11-07T00:00:00Z".parse().expect("time"),
                event: AgentEvent::Usage(aither_core::llm::Usage {
                    prompt_tokens: Some(prompt_tokens),
                    ..aither_core::llm::Usage::default()
                }),
            })
        };
        app.handle_agent_event(usage(40_000)).await;
        app.handle_agent_event(usage(45_000)).await;
        assert_eq!(
            app.usage.and_then(|u| u.prompt_tokens),
            Some(45_000),
            "each request resends the whole context, so prompt_tokens is \
             the latest request's footprint, not a running total"
        );
    }

    #[tokio::test]
    async fn a_stored_recap_row_is_restored_as_a_card_when_history_reloads() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;
        let record = crate::daemon::RecapRecord {
            summary: "Where it stands.".to_string(),
            next_action: None,
            turns: 1,
        };
        let mut row = crate::ChatMessage::user("");
        row.role = MessageRole::Recap;
        row.content = serde_json::to_string(&record).expect("record");
        app.render_history(&[crate::ChatMessage::user("hi"), row])
            .await;
        assert!(matches!(
            app.chat.elements().last(),
            Some(ChatElement::Recap { summary, next_action: None }) if summary == "Where it stands."
        ));
    }

    #[tokio::test]
    async fn rebuilt_history_never_stacks_worked_for_rows() {
        use crate::ChatMessage;
        use crate::session::ToolEventRecord;

        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;
        let at = |second: i64| jiff::Timestamp::from_second(1_800_000_000 + second).expect("time");
        let call = |id: &str, script: &str, second: i64| {
            let start = ToolEventRecord::Start {
                id: id.to_string(),
                name: "terminal".to_string(),
                arguments: serde_json::json!({ "description": script, "script": script })
                    .to_string(),
            };
            let end = ToolEventRecord::End {
                id: id.to_string(),
                name: "terminal".to_string(),
                result: aither_core::llm::ToolResult::text("done"),
            };
            [
                ChatMessage::tool(
                    id,
                    serde_json::to_string(&start).expect("start"),
                    at(second),
                ),
                ChatMessage::tool(
                    id,
                    serde_json::to_string(&end).expect("end"),
                    at(second + 1),
                ),
            ]
        };
        // An interrupted turn with no reply, then a normal turn.
        let rows: Vec<ChatMessage> = std::iter::once(ChatMessage::user("実行 运行 실행"))
            .chain(call("terminal:0#a", "one", 2))
            .chain(call("terminal:0#b", "two", 4))
            .chain([ChatMessage::user("next")])
            .chain(call("terminal:0#c", "three", 32))
            .chain(call("terminal:0#d", "four", 40))
            .chain([ChatMessage::assistant("done 完成 완료", at(50))])
            .collect();
        app.render_history(&rows).await;

        let mut previous_row = false;
        let mut rows_seen = 0;
        for element in app.chat.elements() {
            let is_row = matches!(element, ChatElement::WorkedFor { .. });
            assert!(!(is_row && previous_row), "adjacent rows");
            previous_row = is_row;
            rows_seen += usize::from(is_row);
        }
        assert_eq!(rows_seen, 2);
        assert!(matches!(
            app.chat.elements().last(),
            Some(ChatElement::AssistantText { .. })
        ));
    }

    #[tokio::test]
    async fn a_session_loaded_from_the_database_shows_its_tool_calls_folded_per_turn() {
        use crate::ChatMessage;
        use crate::session::ToolEventRecord;
        use crate::tui::ui::chat::ToolDisplay;

        let dir = tempfile::tempdir().expect("dir");
        let db = crate::db::ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("open database");
        db.create_session("s", None, None)
            .await
            .expect("create session");
        let at = |second: i64| jiff::Timestamp::from_second(1_800_000_000 + second).expect("time");
        let call = |id: &str, script: &str, second: i64| {
            let start = ToolEventRecord::Start {
                id: id.to_string(),
                name: "terminal".to_string(),
                arguments: serde_json::json!({ "description": script, "script": script })
                    .to_string(),
            };
            let end = ToolEventRecord::End {
                id: id.to_string(),
                name: "terminal".to_string(),
                result: aither_core::llm::ToolResult::text("done"),
            };
            [
                ChatMessage::tool(
                    id,
                    serde_json::to_string(&start).expect("start"),
                    at(second),
                ),
                ChatMessage::tool(
                    id,
                    serde_json::to_string(&end).expect("end"),
                    at(second + 1),
                ),
            ]
        };
        let mut first = ChatMessage::user("list issues");
        first.timestamp = Some(at(0));
        let mut second = ChatMessage::user("and the backend?");
        second.timestamp = Some(at(30));
        let rows: Vec<ChatMessage> = std::iter::once(first)
            .chain(call("terminal:0#a", "issue list", 2))
            .chain(call("terminal:0#b", "issue list --state all", 4))
            .chain([ChatMessage::assistant("None open.", at(20)), second])
            .chain(call("terminal:0#c", "cat workspace.toml", 32))
            .chain([ChatMessage::assistant("It is local.", at(40))])
            .collect();
        for row in &rows {
            db.append_chat_message("s", row).await.expect("append row");
        }
        let history = crate::session::load_history_from_db(&db, "s")
            .await
            .expect("load history");

        let (mut app, _id) = app_with_session(dir.path()).await;
        app.render_history(history.messages()).await;

        let commands = |elements: &[ChatElement]| -> Vec<String> {
            elements
                .iter()
                .map(|element| match element {
                    ChatElement::ToolCall {
                        display:
                            ToolDisplay::Terminal {
                                command, status, ..
                            },
                        ..
                    } => {
                        assert!(
                            matches!(status, crate::tui::ui::chat::ToolStatus::Done),
                            "{status:?}"
                        );
                        command.clone()
                    }
                    other => panic!("unexpected folded element {other:?}"),
                })
                .collect()
        };
        let elements = app.chat.elements();
        assert_eq!(elements.len(), 6, "{elements:#?}");
        assert!(matches!(&elements[0], ChatElement::UserMessage { .. }));
        let ChatElement::WorkedFor {
            seconds,
            elements: work,
            ..
        } = &elements[1]
        else {
            panic!("first turn is not folded: {:?}", elements[1]);
        };
        assert_eq!(*seconds, 20);
        assert_eq!(commands(work), ["issue list", "issue list --state all"]);
        assert!(
            matches!(&elements[2], ChatElement::AssistantText { content, .. } if content == "None open.")
        );
        assert!(matches!(&elements[3], ChatElement::UserMessage { .. }));
        // A turn whose work is one command shows that command's own row.
        assert_eq!(
            commands(&elements[4..5]),
            ["cat workspace.toml"],
            "{:?}",
            elements[4]
        );
        assert!(
            matches!(&elements[5], ChatElement::AssistantText { content, .. } if content == "It is local.")
        );
    }

    #[test]
    fn utf8_complete_len_holds_only_incomplete_trailing_sequence() {
        assert_eq!(utf8_complete_len(b"hello"), 5);
        assert_eq!(utf8_complete_len(b""), 0);

        // é (2-byte) split mid-sequence — the second byte is held back.
        let split = "é".as_bytes();
        let buf = [b"hi", &split[..1]].concat();
        assert_eq!(utf8_complete_len(&buf), 2);
        let buf = [b"hi", split].concat();
        assert_eq!(utf8_complete_len(&buf), 4);

        // 4-byte emoji split at 1, 2 and 3 bytes — all held.
        let emoji = "🦀".as_bytes();
        for cut in 1..emoji.len() {
            let buf = [b"x", &emoji[..cut]].concat();
            assert_eq!(utf8_complete_len(&buf), 1, "cut {cut}");
        }
        let buf = [b"x", emoji].concat();
        assert_eq!(utf8_complete_len(&buf), 5);

        // A genuinely invalid byte mid-buffer is skipped, not held; only a
        // trailing incomplete sequence shortens the prefix.
        let buf = [b"ok".as_slice(), &[0xff], b"!", &emoji[..2]].concat();
        assert_eq!(utf8_complete_len(&buf), 4);
    }

    fn waiting_permission_snapshot(id: Option<&str>) -> AppEvent {
        let runtime = crate::client::SessionRuntimeSnapshot {
            waiting_permission: id.map(|id| crate::client::PermissionPollItem {
                id: id.to_string(),
                request: crate::session::PersistedPermissionRequestPayload {
                    mode: crate::PermissionRequestMode::Unsafe,
                    script: "rm -rf build".to_string(),
                    session_id: None,
                    agent_id: None,
                },
            }),
            ..Default::default()
        };
        AppEvent::SessionRuntime(Box::new(crate::client::SessionRuntimeEvent {
            session_id: "s".to_string(),
            runtime,
        }))
    }

    #[tokio::test]
    async fn an_answered_permission_request_does_not_reopen_the_dialog() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _) = app_with_session(dir.path()).await;
        app.handle_event(waiting_permission_snapshot(Some("p1")))
            .expect("snapshot");
        app.open_waiting_permission();
        assert!(matches!(
            app.mode,
            crate::tui::AppMode::PermissionConfirm(_)
        ));
        let action = press(&mut app, KeyCode::Enter);
        assert!(matches!(
            action,
            Some(SendAction::RespondPermission { approved: true, .. })
        ));
        assert!(matches!(app.mode, crate::tui::AppMode::Chat));
        // The daemon has not reported the resolution yet: same snapshot.
        app.open_waiting_permission();
        assert!(
            matches!(app.mode, crate::tui::AppMode::Chat),
            "the stale snapshot must not reopen the answered request"
        );
    }

    #[tokio::test]
    async fn a_request_resolved_elsewhere_closes_the_dialog() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _) = app_with_session(dir.path()).await;
        app.handle_event(waiting_permission_snapshot(Some("p1")))
            .expect("snapshot");
        app.open_waiting_permission();
        assert!(matches!(
            app.mode,
            crate::tui::AppMode::PermissionConfirm(_)
        ));
        app.handle_event(waiting_permission_snapshot(None))
            .expect("resolved snapshot");
        assert!(matches!(app.mode, crate::tui::AppMode::Chat));
        // A later, different request still opens its own dialog.
        app.handle_event(waiting_permission_snapshot(Some("p2")))
            .expect("next snapshot");
        app.open_waiting_permission();
        assert!(matches!(
            app.mode,
            crate::tui::AppMode::PermissionConfirm(_)
        ));
    }

    /// The screen as rows of text.
    fn draw_rows(
        terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
        app: &mut App,
    ) -> Vec<String> {
        terminal
            .draw(|frame| crate::tui::ui::render(frame, app))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    fn set_date(app: &mut App, date: &str, zone: &str) {
        let zone = jiff::tz::TimeZone::get(zone).expect("zone");
        let now = date
            .parse::<jiff::civil::DateTime>()
            .expect("datetime")
            .to_zoned(zone.clone())
            .expect("zoned")
            .timestamp();
        app.new_year.set_date(now, &zone);
    }

    #[tokio::test]
    async fn the_greeting_sits_above_the_input_only_on_new_year_days() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        for zone in ["America/New_York", "Asia/Tokyo"] {
            for (date, shown) in [
                ("2026-12-30T12:00:00", false),
                ("2026-12-31T12:00:00", true),
                ("2027-01-01T12:00:00", true),
                ("2027-01-02T12:00:00", false),
            ] {
                set_date(&mut app, date, zone);
                let rows = draw_rows(&mut terminal, &mut app);
                let greeting = rows
                    .iter()
                    .position(|row| row.contains(crate::tui::new_year::GREETING));
                assert_eq!(greeting.is_some(), shown, "{date} in {zone}");
                if greeting.is_some() {
                    assert!(app.chat_layout.greeting_row().is_some());
                } else {
                    assert_eq!(app.chat_layout.greeting_row(), None);
                }
            }
        }
    }

    #[tokio::test]
    async fn fireworks_start_and_end_early_leave_the_chat_as_it_was() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;
        app.chat.push_user("hello", Vec::new(), 0);
        app.input = "draft".to_string();
        app.cursor = 5;
        app.input_char_count = 5;
        set_date(&mut app, "2026-12-31T12:00:00", "America/New_York");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        let before = draw_rows(&mut terminal, &mut app);
        let scroll = app.scroll;

        // Ctrl+F starts them; the animation paints over the chat.
        app.handle_key(KeyCode::Char('f'), KeyModifiers::CONTROL)
            .expect("key");
        assert!(app.new_year.animating());
        for _ in 0..10 {
            app.new_year.tick();
        }
        assert_ne!(draw_rows(&mut terminal, &mut app), before, "sparks drawn");

        // Any key ends them early and is not typed.
        press(&mut app, KeyCode::Char('x'));
        assert!(!app.new_year.animating());
        assert_eq!(app.input, "draft");
        assert_eq!(app.scroll, scroll);
        assert_eq!(draw_rows(&mut terminal, &mut app), before);

        // A click on the greeting starts them; a click anywhere ends them.
        let row = app.chat_layout.greeting_row().expect("greeting row");
        let area = app.chat_area;
        let screen_row = area.y
            + u16::try_from(row + app.chat_layout.top_padding() - app.effective_scroll())
                .expect("row");
        let click = |kind| crossterm::event::MouseEvent {
            kind,
            column: area.x + 2,
            row: screen_row,
            modifiers: KeyModifiers::NONE,
        };
        let down = crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left);
        app.handle_mouse(click(down)).expect("mouse");
        assert!(app.new_year.animating(), "clicking the greeting plays them");
        app.handle_mouse(click(down)).expect("mouse");
        assert!(!app.new_year.animating());
        assert_eq!(draw_rows(&mut terminal, &mut app), before);

        // They end by themselves.
        app.new_year.start();
        for _ in 0..crate::tui::new_year::Fireworks::FRAMES {
            app.new_year.tick();
        }
        assert!(!app.new_year.animating());
        assert_eq!(draw_rows(&mut terminal, &mut app), before);
    }

    #[tokio::test]
    async fn ctrl_f_does_nothing_off_new_year_days() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut app, _id) = app_with_session(dir.path()).await;
        set_date(&mut app, "2026-12-30T12:00:00", "Asia/Tokyo");
        app.handle_key(KeyCode::Char('f'), KeyModifiers::CONTROL)
            .expect("key");
        assert!(!app.new_year.animating());
    }

    async fn effort_app(dir: &std::path::Path) -> App {
        let (mut app, _) = app_with_session(dir).await;
        app.current_effort = Some("medium".to_string());
        app.effort_ladder = vec!["low", "medium", "high", "max"];
        app.effort_index = app.effort_ladder_index();
        app.mode = crate::tui::AppMode::EffortSelect;
        app
    }

    #[tokio::test]
    async fn effort_arrows_send_nothing_until_enter() {
        let dir = tempfile::tempdir().expect("dir");
        let mut app = effort_app(dir.path()).await;
        assert!(press(&mut app, KeyCode::Right).is_none());
        assert!(press(&mut app, KeyCode::Right).is_none());
        assert!(press(&mut app, KeyCode::Left).is_none());
        assert!(matches!(app.mode, crate::tui::AppMode::EffortSelect));
        assert_eq!(app.current_effort.as_deref(), Some("medium"));
        match press(&mut app, KeyCode::Enter) {
            Some(SendAction::SetEffort(Some(effort))) => assert_eq!(effort, "high"),
            _ => panic!("Enter must apply the highlighted level"),
        }
        assert!(matches!(app.mode, crate::tui::AppMode::Chat));
    }

    #[tokio::test]
    async fn effort_escape_keeps_the_previous_effort() {
        let dir = tempfile::tempdir().expect("dir");
        let mut app = effort_app(dir.path()).await;
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Right);
        assert!(press(&mut app, KeyCode::Esc).is_none());
        assert!(matches!(app.mode, crate::tui::AppMode::Chat));
        assert_eq!(app.current_effort.as_deref(), Some("medium"));
        assert_eq!(app.effective_effort(), "medium");
    }

    #[tokio::test]
    async fn effort_panel_is_drawn_with_the_marker_under_the_heading() {
        let dir = tempfile::tempdir().expect("dir");
        let mut app = effort_app(dir.path()).await;
        let screen = session_screen(&mut app).join("\n");
        assert!(screen.contains("Effort"), "{screen}");
        assert!(screen.contains("▲"), "{screen}");
        assert!(screen.contains("Esc to cancel"), "{screen}");
    }
}
