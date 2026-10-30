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

use super::agent_event_adapter::{ToolEndKind, ToolStartKind, parse_tool_end, parse_tool_start};
use super::clipboard::{
    ClipboardPasteResult, ImageManager, TextManager, download_image, is_image_url, read_clipboard,
    set_clipboard_html, set_clipboard_text,
};
use super::completion::{Completion, CompletionMode};
use super::error::{AppResult, TuiError};
use super::event::{AppEvent, EventSender};
use super::file_diff::FileDiffHandle;
use super::history::InputHistory;
use super::queue::{Attachment, MessageQueue, PendingMessage};
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
    ActiveTextInput, AppMode, AskUserState, ChatgptAuthStatus, ConfigEditMode, ConfigState,
    CopilotAuthStatus, McpAddMode, PasteFilter, SelectionPoint, SkillAddMode, SshAddMode,
    StatusNotice, TextSelection, detect_api_key_from_env,
};
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
    /// Unified configuration state (for Config mode)
    pub config_state: ConfigState,
    /// Selected index in model select popup
    pub model_select_index: usize,
    /// Available models for model select (cached)
    pub model_select_list: Vec<String>,
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
    /// Per-session file-diff worker (rebuilt on session switch)
    pub file_diffs: Option<FileDiffHandle>,
    /// Receiver for clipboard paste resolution (oneshot)
    pub clipboard_rx: Option<Receiver<Result<ClipboardPasteResult, TuiError>>>,
    /// Pending paste results waiting to be handled
    pub pending_paste_results: std::collections::VecDeque<PendingPaste>,
    /// Pending attachment jobs (clipboard read / download)
    pub pending_attachment_jobs: usize,
    /// Cumulative token usage for current session
    pub usage: Option<aither_core::llm::Usage>,
    /// Decode-speed measurement for the streaming response
    pub decode_meter: crate::decode_meter::DecodeMeter,
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
    /// Whether the archived section is expanded in `SessionSelect`
    pub archived_expanded: bool,

    /// Event sender for spawning background tasks
    pub event_tx: EventSender,
    /// Terminal image rendering support (None if terminal doesn't support graphics)
    pub image_support: Option<super::ui::image::TerminalImageSupport>,
    /// Image cache: URL -> fetched image state (for terminal image rendering)
    pub image_cache: ahash::AHashMap<String, super::ui::image::ImageState>,
    /// URLs currently being fetched (to avoid duplicate requests)
    pub image_fetching: ahash::AHashSet<String>,
    /// Image keys that failed to fetch or decode (avoids retry loops)
    pub image_failed: ahash::AHashSet<String>,
    /// Mouse/text selection in rendered chat content
    pub selection: Option<TextSelection>,
    /// Plain text render cache used for selection copy
    pub rendered_plain_lines: Vec<String>,
    /// Top padding rows added when viewport is taller than rendered content
    pub render_top_padding: usize,
    /// Row spans of collapsible tool calls in `rendered_plain_lines` space
    pub collapse_regions: Vec<super::ui::chat::ClickRegion>,
    /// Timing breakdown of the last rendered frame (jank reporting).
    pub frame_stats: super::perf::FrameStats,
    /// When and where the last plain click finished; a second press at the
    /// same spot inside `DOUBLE_CLICK_WINDOW` selects the word under it.
    last_click: Option<(Instant, SelectionPoint)>,
    /// Session background-job registry (set when the agent worker installs)
    pub job_registry: Option<aither_sandbox::JobRegistry>,
    /// In-flight subagent runs for the session (list/cancel handle)
    pub subagent_runs: Option<aither_agent::specialized::SubagentRuns>,
    /// Model clone for `/btw` side questions (outside conversation history).
    pub btw_llm: Option<crate::agent::RequestParameterizedModel<aither_cloud::CloudProvider>>,
    /// Latest job list shown in `AppMode::Jobs`
    pub jobs: Vec<aither_sandbox::JobInfo>,
    /// Selected row in `AppMode::Jobs`
    pub jobs_selected: usize,
    /// Last jobs refresh (panel auto-refreshes on tick while open)
    pub jobs_refreshed: Option<std::time::Instant>,
    /// Live tracker of `subagent` runs, shared with the session worker's
    /// `subagent` IPC command (set when the agent worker installs)
    pub subagent_tracker: crate::agent::SubagentTracker,
    /// The lease of the run in flight, held from its start until the TUI has
    /// persisted its last message.
    pub run_guard: Option<crate::session::SessionRunGuard>,
    /// Running terminal calls bound to registry jobs for live output.
    pub live_reads: std::collections::HashMap<String, LiveRead>,
    /// Last time file-diff previews were requested for running calls.
    pub diff_preview_at: Option<std::time::Instant>,
    /// Loaded secret values for bash-mode masking and env injection.
    pub secrets: crate::secrets::Secrets,
    /// Questions the agent asked that nobody answered yet, for the status bar.
    pub open_question_count: usize,
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
        let mut session_manager = SessionManager::new().await?;

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

        let needs_setup = !config.is_configured();
        // Always start in Chat mode - show inline message if not configured
        let mode = AppMode::Chat;

        // Initialize config state
        let mut config_state = ConfigState::default();
        config_state.load_providers(&config);

        // Load profiles from database
        let profiles = crate::profile::list_profiles(session_manager.db())
            .await
            .unwrap_or_default();
        let current_profile_id = profiles.first().map(|p| p.id.clone());
        if let Some(ref profile_id) = current_profile_id {
            session_manager.set_current_profile(profile_id.clone());
        }

        // Load input history
        let history = InputHistory::new(session_manager.may_dir());

        // Initialize completion
        let completion = Completion::new();

        // Get current working directory
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        // Build status message for env-detected providers
        let status = if env_providers_added.is_empty() {
            None
        } else {
            let names: Vec<_> = env_providers_added
                .iter()
                .map(|(id, _)| id.as_str())
                .collect();
            Some(StatusNotice::persistent(format!(
                "Auto-configured from env: {}",
                names.join(", ")
            )))
        };

        let secrets = crate::secrets::Secrets::load(
            &crate::secrets::SecretStore::new(session_manager.may_dir()),
            &config,
        );

        Ok(Self {
            session_manager,
            worker_settings: crate::agent::AgentLiveSettings::of(&config),
            config,
            input: String::new(),
            cursor: 0,
            input_char_count: 0,
            attachments: Vec::new(),
            chat: ChatStream::new(),
            scroll: usize::MAX, // Follow bottom by default
            input_visible: true,
            content_height: AtomicUsize::new(0),
            view_height: AtomicUsize::new(0),
            mode,
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
            config_state,
            model_select_index: 0,
            model_select_list: Vec::new(),
            needs_setup,
            cwd,
            quit_pending: false,
            last_esc_at: None,
            thinking_start: None,
            copilot_auth_status: None,
            chatgpt_auth_status: None,
            tier_picker_tier: None,
            tier_picker_models: Vec::new(),
            tier_picker_filtered: Vec::new(),
            tier_picker_selected: 0,
            tier_picker_query: String::new(),
            tier_picker_loading: false,
            model_fetch_rx: None,
            file_diffs: None,
            clipboard_rx: None,
            pending_paste_results: std::collections::VecDeque::new(),
            pending_attachment_jobs: 0,
            usage: None,
            decode_meter: crate::decode_meter::DecodeMeter::default(),
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
            archived_expanded: false,

            event_tx,
            image_support,
            image_cache: ahash::AHashMap::new(),
            image_fetching: ahash::AHashSet::new(),
            image_failed: ahash::AHashSet::new(),
            selection: None,
            rendered_plain_lines: Vec::new(),
            render_top_padding: 0,
            frame_stats: super::perf::FrameStats::default(),
            collapse_regions: Vec::new(),
            last_click: None,
            job_registry: None,
            subagent_runs: None,
            btw_llm: None,
            jobs: Vec::new(),
            jobs_selected: 0,
            jobs_refreshed: None,
            subagent_tracker: crate::agent::SubagentTracker::default(),
            run_guard: None,
            live_reads: std::collections::HashMap::new(),
            diff_preview_at: None,
            secrets,
            open_question_count: 0,
            question_cursor: 0,
            parked_question: None,
        })
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
    /// The worker tracks the session sandbox plus every writable workspace
    /// dir. Container-profile sessions are skipped: their writes land
    /// inside the container where host-side git cannot see them.
    fn ensure_file_diffs(&mut self) {
        let Some(session) = self.session_manager.current() else {
            self.file_diffs = None;
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
            return;
        }

        let session_paths = crate::session::SessionPaths::new(session.sandbox_dir.clone());
        let mut roots = vec![
            crate::file_tracker::TrackedRootSpec::new(session.sandbox_dir.clone())
                .exclude("filebase")
                .exclude("tmp")
                .exclude("outputs")
                .exclude("skills")
                .exclude("subagents"),
        ];
        if let Ok(config) =
            crate::workspace::WorkspaceConfig::load(&session_paths.workspace_config())
        {
            roots.extend(
                config
                    .workdirs
                    .iter()
                    .filter(|dir| dir.permission == crate::workspace::Permission::ReadWrite)
                    .map(|dir| crate::file_tracker::TrackedRootSpec::new(dir.path.clone())),
            );
        }
        self.file_diffs =
            FileDiffHandle::start(session_id, session_paths.root().join("filebase"), roots);
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
    /// recorded at prompt dispatch. `fallback_call_ids` are the tool
    /// calls of the rewound turns; the earliest of their checkpoints
    /// applies when the prompt mark does not exist.
    pub fn request_prompt_rewind(&mut self, message_db_id: i64, fallback_call_ids: Vec<String>) {
        self.ensure_file_diffs();
        if let Some(handle) = &self.file_diffs {
            handle.request_rewind(&format!("prompt:{message_db_id}"), fallback_call_ids);
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
            options_for: None,
            option_selected: 0,
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
        let outcomes = match &self.file_diffs {
            Some(handle) => handle.drain(),
            None => return,
        };
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
                }
                super::file_diff::Outcome::Rewound { diffs, .. } => {
                    self.apply_rewind_result(&diffs);
                }
            }
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
        let files: usize = diffs.iter().map(|d| d.files).sum();
        let additions: usize = diffs.iter().map(|d| d.additions).sum();
        let deletions: usize = diffs.iter().map(|d| d.deletions).sum();
        self.chat.push_notice(&format!(
            "⏪ restored {files} file{} (+{additions} −{deletions})",
            if files == 1 { "" } else { "s" },
        ));
    }

    /// Re-fetch the session's background jobs from the registry.
    pub fn refresh_jobs(&mut self) {
        let Some(registry) = self.job_registry.clone() else {
            return;
        };
        let tx = self.event_tx.clone();
        spawn(async move {
            let _ = tx.send(AppEvent::JobsLoaded(registry.list().await)).await;
        })
        .detach();
        self.jobs_refreshed = Some(std::time::Instant::now());
    }

    /// Cancel the subagent run matching a tracked snapshot. The tool's own
    /// registry keys runs by its internal id; both registries record the run
    /// back-to-back, so the earliest name+prompt match is the same
    /// invocation.
    pub fn cancel_subagent(&mut self, run: &crate::agent::SubagentRunSnapshot) {
        let agent = run.agent.clone();
        let prompt = run.prompt.clone();
        let hit = self.subagent_runs.as_ref().and_then(|runs| {
            runs.list()
                .into_iter()
                .filter(|info| info.subagent == agent && info.prompt == prompt)
                .min_by_key(|info| info.id)
                .map(|info| (info.id, runs.clone()))
        });
        match hit {
            Some((id, runs)) if runs.cancel(id) => {
                self.set_status(format!("Stopping subagent {}…", run.agent));
            }
            _ => {
                self.set_status(format!("No live registry entry for subagent {}", run.agent));
            }
        }
    }

    /// Kill a background job by pid, then refresh the panel.
    pub fn kill_job(&mut self, pid: u32) {
        let Some(registry) = self.job_registry.clone() else {
            return;
        };
        let tx = self.event_tx.clone();
        spawn(async move {
            let ok = registry.kill(pid).await;
            let _ = tx.send(AppEvent::JobKilled { pid, ok }).await;
            let _ = tx.send(AppEvent::JobsLoaded(registry.list().await)).await;
        })
        .detach();
    }

    /// Open the `/btw` side-question overlay.
    pub fn open_btw(&mut self) {
        if self.btw_llm.is_none() {
            self.chat
                .push_notice("No model available for /btw — start a session first");
            return;
        }
        self.mode = AppMode::Btw(super::state::BtwState::default());
    }

    /// Submit the question typed in the `/btw` overlay. Streams the answer
    /// back through [`AppEvent`]s; nothing touches the conversation history.
    pub fn submit_btw(&mut self) {
        let (AppMode::Btw(state), Some(llm)) = (&self.mode, self.btw_llm.clone()) else {
            return;
        };
        let question = state.input.trim().to_string();
        if question.is_empty() || state.loading {
            return;
        }
        let messages = self.btw_messages(&question);
        let tx = self.event_tx.clone();
        if let AppMode::Btw(state) = &mut self.mode {
            state.loading = true;
        }
        spawn(async move {
            use aither_core::LanguageModel;
            use futures_lite::StreamExt;
            let request = aither_core::llm::LLMRequest::new(messages);
            let mut stream = std::pin::pin!(llm.respond(request));
            let mut failed = false;
            while let Some(result) = stream.next().await {
                match result {
                    Ok(aither_core::llm::Event::Text(text)) => {
                        if tx.send(AppEvent::BtwDelta(text)).await.is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        failed = true;
                        let _ = tx.send(AppEvent::BtwFailed(error.to_string())).await;
                        break;
                    }
                }
            }
            if !failed {
                let _ = tx.send(AppEvent::BtwFinished).await;
            }
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
    fn btw_messages(&self, question: &str) -> Vec<aither_core::llm::Message> {
        use super::ui::chat::{ChatElement, ToolDisplay};
        use aither_core::llm::Message;

        let mut messages = vec![Message::system(Self::BTW_SYSTEM)];
        for element in self
            .chat
            .elements()
            .iter()
            .rev()
            .take(Self::BTW_CONTEXT_ELEMENTS)
        {
            let message = match element {
                ChatElement::UserMessage { content, .. } => Some(Message::user(content.as_str())),
                ChatElement::AssistantText { content, .. } => {
                    (!content.is_empty()).then(|| Message::assistant(content.as_str()))
                }
                ChatElement::ToolCall { display, .. } => {
                    let summary = match display {
                        ToolDisplay::Terminal {
                            command, output, ..
                        } => {
                            let mut text = format!("ran terminal command:\n{command}");
                            if let Some(info) = output
                                && !info.output.is_empty()
                            {
                                let tail: String = info.output.chars().take(600).collect();
                                text.push_str("\noutput:\n");
                                text.push_str(&tail);
                            }
                            text
                        }
                        ToolDisplay::Search { query, .. } => format!("web search: {query}"),
                        ToolDisplay::Fetch { url, .. } => format!("fetched {url}"),
                        ToolDisplay::Task {
                            subagent, prompt, ..
                        } => format!("delegated to subagent {subagent}: {prompt}"),
                        ToolDisplay::Builtin { tool, detail, .. } => {
                            format!("hosted tool {tool}: {detail}")
                        }
                    };
                    Some(Message::assistant(summary))
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
        ordered.push(Message::user(question));
        ordered
    }

    /// Open the jobs/subagents panel.
    pub fn open_jobs(&mut self) {
        if self.job_registry.is_none() {
            self.chat
                .push_error("No active session — jobs list unavailable");
            return;
        }
        self.mode = AppMode::Jobs;
        self.jobs_selected = 0;
        self.refresh_jobs();
    }

    /// Whether any background job or tracked subagent is currently running.
    #[must_use]
    pub fn has_running_background(&self) -> bool {
        !self.subagent_tracker.running().is_empty()
            || self
                .jobs
                .iter()
                .any(|job| matches!(job.status, aither_sandbox::JobStatus::Running))
    }

    /// Poll live output and file-diff previews for running terminal calls —
    /// the streaming half of tool-call rendering. Runs on each tick while
    /// the agent processes.
    pub fn poll_live_output(&mut self) {
        // Drop bindings whose call left the Running state (end, failure,
        // interrupt); the remaining reads still refresh on status.
        self.live_reads
            .retain(|call_id, _| self.chat.is_tool_running(call_id));

        if let Some(registry) = self.job_registry.clone() {
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

            if !self.live_reads.is_empty() {
                let tx = self.event_tx.clone();
                let reads: Vec<(String, String, usize)> = self
                    .live_reads
                    .iter()
                    .map(|(call_id, read)| (call_id.clone(), read.task_id.clone(), read.cursor))
                    .collect();
                spawn(async move {
                    let mut deltas = Vec::with_capacity(reads.len());
                    for (call_id, task_id, cursor) in reads {
                        if let Ok(delta) = registry.terminal_read(&task_id, cursor, 64 * 1024).await
                        {
                            deltas.push(super::event::TerminalOutputDelta {
                                call_id,
                                cursor: delta.cursor,
                                bytes: delta.bytes,
                                status: delta.status,
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
                state.subagent = self.subagent_tracker.get(run_id);
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
                let Some(registry) = self.job_registry.clone() else {
                    state.finished = true;
                    return;
                };
                let tx = self.event_tx.clone();
                spawn(async move {
                    let (job, output) =
                        futures_lite::future::zip(registry.get(pid), registry.terminal_output(pid))
                            .await;
                    let _ = tx.send(AppEvent::JobDetailLoaded { job, output }).await;
                })
                .detach();
            }
            crate::tui::state::JobDetailTarget::Subagent(run_id) => {
                match self.subagent_tracker.get(run_id) {
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
                })
            }
            Kind::Stored { url, path } => {
                let label = std::path::Path::new(if path.is_empty() { &url } else { &path })
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("image")
                    .to_string();
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
                Some(super::ui::chat::ToolCallImage { key, label })
            }
        }
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
        use super::ui::markdown::extract_images;

        // Collect image URLs first to avoid borrow issues
        let urls: Vec<String> = self
            .chat
            .active_elements()
            .iter()
            .flat_map(|elem| {
                if let ChatElement::AssistantText { content, .. } = elem {
                    extract_images(content.as_str())
                        .into_iter()
                        .map(|(_, url)| url)
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                }
            })
            .collect();

        if !urls.is_empty() {
            tracing::debug!(
                "scan_and_fetch_images: found {} URLs: {:?}",
                urls.len(),
                urls
            );
        }

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

    /// Initialize for a session (sets up image manager and status).
    ///
    /// Takes the session ID and a slice of messages to avoid borrow conflicts
    /// with the session manager.
    ///
    /// # Panics
    ///
    /// Panics if the session manager has no current session while a session is
    /// being initialized.
    pub async fn init_session(
        &mut self,
        session_id: &SessionId,
        messages: &[ChatMessage],
        model: Option<&str>,
    ) {
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
        self.rendered_plain_lines.clear();
        self.render_top_padding = 0;
        self.collapse_regions.clear();
        self.jobs.clear();
        self.jobs_selected = 0;
        self.image_cache.clear();
        self.image_fetching.clear();
        self.image_failed.clear();
        self.jobs_refreshed = None;
        self.subagent_runs = None;
        self.subagent_tracker = crate::agent::SubagentTracker::default();
        self.live_reads.clear();
        self.diff_preview_at = None;

        // Load chat history into display
        self.chat = ChatStream::new();
        for (history_index, msg) in messages.iter().enumerate() {
            match msg.role {
                MessageRole::User => {
                    let restored = futures_util::future::join_all(msg.attachments.iter().map(
                        |att| async move {
                            super::runtime::tui_attachment_from_persisted(att.clone())
                                .await
                                .map_err(|error| (att.name.clone(), error))
                        },
                    ))
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
                }
                _ => {}
            }
        }

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

        // Set context window from model info
        if let Some(model_name) = model {
            if let Some(info) = aither_models::lookup(model_name) {
                self.context_window = info.max_input_tokens();
            } else {
                self.context_window = None;
            }
        } else {
            self.context_window = None;
        }
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

    /// Bring back the parked open question once the screen is free.
    pub fn restore_parked_question(&mut self) {
        super::reducer::ask_user::restore_parked_question(
            &mut self.mode,
            &mut self.parked_question,
        );
    }

    /// Show a status notice that stays until replaced or cleared.
    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = Some(StatusNotice::persistent(text));
    }

    /// Show a status notice that clears itself after [`STATUS_NOTICE_TTL`].
    pub fn set_transient_status(&mut self, text: impl Into<String>) {
        self.status = Some(StatusNotice::transient(text, STATUS_NOTICE_TTL));
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
            AppMode::SessionSelect => {
                return Some(ActiveTextInput::Simple {
                    input: &mut self.session_search,
                    filter: PasteFilter::Any,
                });
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
                if let AppMode::AskUser(state) = &mut self.mode
                    && let Some(qs) = state.current_state_mut()
                    && qs.custom_mode
                {
                    return Some(ActiveTextInput::Simple {
                        input: &mut qs.custom_text,
                        filter: PasteFilter::Any,
                    });
                }
            }
            // FilePicker is handled separately (has its own insert_str method)
            _ => {}
        }
        None
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
        // Global shortcuts
        if modifiers.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('c') => {
                    if self.copy_selection_to_clipboard()? {
                        self.quit_pending = false;
                        self.set_transient_status("Copied selection");
                        return Ok(None);
                    }

                    // Double-press Ctrl+C to quit
                    if self.quit_pending {
                        return Ok(Some(SendAction::Quit));
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
                KeyCode::Char('q') => return Ok(Some(SendAction::Quit)),
                // Ctrl+V pastes the system clipboard through the same path
                // as a terminal paste; bound globally so every text input
                // can use it.
                KeyCode::Char('v' | 'V') => {
                    self.handle_paste();
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
                        self.load_session_list();
                    }
                    return Ok(None);
                }
                _ => {}
            }
        }

        // Reset quit confirmation on any other input
        if self.quit_pending {
            self.quit_pending = false;
            self.status = None;
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
            AppMode::TierModelPicker => Ok(self.handle_tier_model_picker_key(code)),
            AppMode::CopilotAuth => Ok(super::reducer::manage::handle_copilot_auth_key(self, code)),
            AppMode::ChatgptAuth => Ok(super::reducer::manage::handle_chatgpt_auth_key(self, code)),
            AppMode::PermissionConfirm(_) => Ok(
                super::reducer::manage::handle_permission_confirm_key(self, code),
            ),
            AppMode::WorkspaceConfirm(_) => Ok(
                super::reducer::manage::handle_workspace_confirm_key(self, code),
            ),
            AppMode::McpManage => Ok(super::reducer::manage::handle_mcp_key(self, code)),
            AppMode::SshManage => Ok(super::reducer::manage::handle_ssh_key(self, code)),
            AppMode::AskUser(_) => Ok(super::reducer::ask_user::handle_ask_user_key(self, code)),
            AppMode::SkillManage => Ok(super::reducer::manage::handle_skill_key(self, code)),
            AppMode::ProfileSelect => Ok(super::reducer::navigation::handle_profile_select_key(
                self, code,
            )),
            AppMode::Rewind(_) => Ok(super::reducer::navigation::handle_rewind_key(self, code)),
            AppMode::Jobs => Ok(super::reducer::navigation::handle_jobs_key(self, code)),
            AppMode::JobDetail(_) => Ok(super::reducer::navigation::handle_job_detail_key(
                self, code,
            )),
            AppMode::Btw(_) => Ok(super::reducer::navigation::handle_btw_key(self, code)),
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

    /// Trigger async loading of session list for the session select popup.
    /// Uses profile sessions when a profile is active, falls back to flat list.
    pub fn load_session_list(&mut self) {
        let tx = self.event_tx.clone();
        let db = self.session_manager.db().clone();
        let profile_id = self.current_profile_id.clone();

        spawn(async move {
            if let Some(pid) = profile_id {
                let result = crate::profile::query_profile_sessions(&db, &pid).await;
                let _ = tx
                    .send(AppEvent::ProfileSessionsLoaded(
                        result.map_err(TuiError::from),
                    ))
                    .await;
            } else {
                let result = crate::session::list_summaries_from_db(&db).await;
                let _ = tx
                    .send(AppEvent::SessionsLoaded(result.map_err(TuiError::from)))
                    .await;
            }
        })
        .detach();
        self.set_status("Loading sessions...");
    }

    /// Build a flat session list from profile sessions for navigation.
    /// Pinned items come first, then regular, then archived (if expanded).
    pub fn build_session_list_from_profile(&mut self) {
        let Some(ref ps) = self.profile_sessions else {
            self.session_list.clear();
            return;
        };

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

        // Regular sessions
        list.extend(ps.regular.iter().cloned());

        // Archived (only if expanded)
        if self.archived_expanded {
            list.extend(ps.archived.iter().cloned());
        }

        self.session_list = list;
    }

    /// Get the current profile name for display.
    pub fn current_profile_name(&self) -> Option<&str> {
        let pid = self.current_profile_id.as_deref()?;
        self.profiles
            .iter()
            .find(|p| p.id == pid)
            .map(|p| p.name.as_str())
    }

    /// Reload profile sessions asynchronously (used after pin/archive mutations).
    pub fn refresh_profile_sessions(&mut self) {
        let Some(pid) = self.current_profile_id.clone() else {
            return;
        };
        let tx = self.event_tx.clone();
        let db = self.session_manager.db().clone();

        spawn(async move {
            let result = crate::profile::query_profile_sessions(&db, &pid).await;
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

    /// Handle key in chat mode (unified input + navigation).
    fn handle_chat_key(&mut self, code: KeyCode) -> Option<SendAction> {
        match code {
            KeyCode::Enter => {
                // Handle completion if active
                if self.completion.is_active() {
                    let mode = self.completion.mode;
                    if let Some(item) = self.completion.complete() {
                        let cmd = item.insert.clone();
                        self.completion.cancel();

                        // For slash commands, execute directly without showing in input
                        if mode == CompletionMode::Slash {
                            self.input.clear();
                            self.cursor = 0;
                            self.input_char_count = 0;
                            return self.execute_slash_command(&cmd);
                        }

                        // For file completion, insert into input
                        self.accept_completion(&cmd);
                    } else {
                        // No item to select, just cancel completion
                        self.completion.cancel();
                    }
                    return None;
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
                    return Some(self.prepare_message(PendingUserMessageMode::Steer));
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
                if self.completion.is_active() {
                    self.completion.prev();
                } else if let Some(entry) = self.history.prev(&self.input) {
                    self.input = entry.to_string();
                    self.input_char_count = self.input.chars().count();
                    self.cursor = self.input_char_count;
                }
            }
            KeyCode::Down => {
                if self.completion.is_active() {
                    self.completion.next();
                } else if let Some(entry) = self.history.next_entry() {
                    self.input = entry.to_string();
                    self.input_char_count = self.input.chars().count();
                    self.cursor = self.input_char_count;
                } else if self.job_registry.is_some() {
                    // ↓ past the newest history entry opens the background
                    // jobs/subagents panel (status bar hint: "↓ jobs").
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
                if self.completion.is_active() {
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
                if self.completion.is_active()
                    && let Some(item) = self.completion.complete()
                {
                    self.accept_completion(&item.insert.clone());
                    return None;
                }
                if self.processing && (!self.input.is_empty() || !self.attachments.is_empty()) {
                    if self.has_pending_attachments() {
                        self.set_status("Waiting for attachments to finish...");
                        return None;
                    }
                    return Some(self.prepare_message(PendingUserMessageMode::Queue));
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
        let base = if self.scroll == usize::MAX {
            max_scroll
        } else {
            self.scroll
        };
        self.scroll = base.saturating_sub(lines);
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

        if !matches!(self.mode, AppMode::Chat) {
            return Ok(None);
        }

        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_chat_up(SCROLL_STEP_LINES),
            MouseEventKind::ScrollDown => self.scroll_chat_down(SCROLL_STEP_LINES),
            MouseEventKind::Down(MouseButton::Left) => {
                let point = self.mouse_to_selection_point(mouse);
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
                        self.rendered_plain_lines
                            .get(p.line)
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
                    // A press/release without a drag is a click: remember it
                    // for double-click detection, then toggle the collapsed
                    // state of the tool call under the cursor.
                    self.last_click = self
                        .mouse_to_selection_point(mouse)
                        .map(|point| (Instant::now(), point));
                    if let Some(point) = self.mouse_to_selection_point(mouse)
                        && let Some(region) = self
                            .collapse_regions
                            .iter()
                            .find(|r| point.line >= r.start && point.line < r.end)
                    {
                        let id = region.element_id.clone();
                        self.chat.toggle_expanded(&id);
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) if self.copy_selection_to_clipboard()? => {
                self.set_transient_status("Copied selection");
            }
            _ => {}
        }

        Ok(None)
    }

    fn mouse_to_selection_point(&self, mouse: MouseEvent) -> Option<SelectionPoint> {
        if self.rendered_plain_lines.is_empty() {
            return None;
        }

        let base_scroll = if self.scroll == usize::MAX {
            let content_h = self.content_height.load(Ordering::Relaxed);
            let view_h = self.view_height.load(Ordering::Relaxed);
            content_h.saturating_sub(view_h)
        } else {
            self.scroll
        };

        let raw_line = base_scroll.saturating_add(mouse.row as usize);
        let line = raw_line.saturating_sub(self.render_top_padding);
        let line = line.min(self.rendered_plain_lines.len().saturating_sub(1));
        let text = self.rendered_plain_lines.get(line)?;
        let column = clamp_column_by_width(text, mouse.column as usize);
        Some(SelectionPoint { line, column })
    }

    fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        if sel.is_empty() {
            return None;
        }
        let (start, end) = sel.normalized();

        if self.rendered_plain_lines.is_empty() || start.line >= self.rendered_plain_lines.len() {
            return None;
        }

        let end_line = end
            .line
            .min(self.rendered_plain_lines.len().saturating_sub(1));
        let mut out = String::new();

        for line_idx in start.line..=end_line {
            let line = &self.rendered_plain_lines[line_idx];
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
    fn execute_slash_command(&mut self, cmd: &str) -> Option<SendAction> {
        match cmd {
            "/session" => {
                self.load_session_list();
                None
            }
            "/new" => Some(SendAction::NewSession),
            "/profile" => {
                if self.profiles.len() > 1 {
                    self.profile_selected = self
                        .profiles
                        .iter()
                        .position(|p| Some(p.id.as_str()) == self.current_profile_id.as_deref())
                        .unwrap_or(0);
                    self.mode = AppMode::ProfileSelect;
                } else {
                    self.load_session_list();
                }
                None
            }
            "/sandbox" => {
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
            "/config" => {
                self.config_state = ConfigState::default();
                self.config_state.load_providers(&self.config);
                self.mode = AppMode::Config;
                None
            }
            "/model" => {
                self.model_select_list = self.collect_all_models();
                self.model_select_index = 0;
                let fallback_model = self.config.last_model.as_ref().map(ToString::to_string);
                let current = self.current_model.as_deref().or(fallback_model.as_deref());
                if let Some(current) = current
                    && let Some(idx) = self.model_select_list.iter().position(|m| m == current)
                {
                    self.model_select_index = idx;
                }
                self.mode = AppMode::ModelSelect;
                None
            }
            "/rewind" => {
                if self.processing {
                    self.chat
                        .push_notice("Wait for the current turn to finish before rewinding");
                } else if !self.open_rewind_picker() {
                    self.chat.push_notice("No prompts to rewind to");
                }
                None
            }
            "/jobs" => {
                self.open_jobs();
                None
            }
            "/questions" => Some(SendAction::OpenQuestions),
            "/btw" => {
                self.open_btw();
                None
            }
            "/resume-codex" | "/resume-claude" | "/secret" => {
                // These take arguments — leave the command in the input so
                // the user can type them, rather than executing now.
                self.input = format!("{cmd} ");
                self.input_char_count = self.input.chars().count();
                self.cursor = self.input_char_count;
                None
            }
            "/quit" => Some(SendAction::Quit),
            "/compact" => Some(SendAction::Compact(None)),
            "/context" => Some(SendAction::ContextReport),
            "/context dump" => Some(SendAction::ContextDump),
            "/mcp" => {
                self.mode = AppMode::McpManage;
                None
            }
            "/ssh" => {
                self.mode = AppMode::SshManage;
                None
            }
            "/skill" => {
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
            _ => None,
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

        let id = format!("bash-{}", uuid::Uuid::new_v4());
        self.chat.push_tool(
            id.clone(),
            ToolDisplay::Terminal {
                command: format!("! {masked}"),
                mode: aither_sandbox::TerminalExecutionMode::Default,
                status: super::ui::chat::ToolStatus::Running,
                output: None,
                diffs: Vec::new(),
                expanded: false,
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
    fn prepare_message(&mut self, delivery_mode: PendingUserMessageMode) -> SendAction {
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
        if trimmed == "/context" && attachments.is_empty() {
            return SendAction::ContextReport;
        }
        if trimmed == "/context dump" && attachments.is_empty() {
            return SendAction::ContextDump;
        }
        if let Some(rest) = trimmed.strip_prefix("/compact")
            && attachments.is_empty()
        {
            let focus = rest.trim();
            if focus.is_empty() {
                return SendAction::Compact(None);
            }
            return SendAction::Compact(Some(focus.to_string()));
        }
        for (cmd, source) in [
            ("/resume-codex", crate::session::import::ForeignAgent::Codex),
            (
                "/resume-claude",
                crate::session::import::ForeignAgent::Claude,
            ),
        ] {
            if trimmed == cmd || trimmed.starts_with(&format!("{cmd} ")) {
                return SendAction::ResumeImport {
                    source,
                    query: trimmed[cmd.len()..].trim().to_string(),
                };
            }
        }

        let msg = PendingMessage {
            content,
            hidden_context_xml: None,
            attachments,
            queued_at: jiff::Timestamp::now(),
            delivery_mode,
            display: None,
        };

        if self.processing {
            SendAction::EnqueuePending(msg)
        } else {
            SendAction::Send(msg)
        }
    }

    /// Called when starting to process a message.
    pub fn start_processing(&mut self, msg: &PendingMessage) {
        self.processing = true;
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
    /// block. `async_fs` keeps the filesystem IO off the UI thread.
    async fn refresh_todos(&mut self) {
        let Some(session) = self.session_manager.current() else {
            return;
        };
        let paths = self
            .session_manager
            .store()
            .session_paths(session.id.as_str(), session.profile_id.as_deref());
        match paths.tasks_content().await {
            Ok(content) => {
                let checklist = crate::todo_parser::TodoChecklist::parse(
                    content.as_deref().unwrap_or_default(),
                );
                self.chat.set_todos(checklist.items);
            }
            Err(error) => {
                tracing::warn!(error = %error, "failed to read session tasks.md");
            }
        }
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
                        self.scroll = usize::MAX;
                        self.input_visible = true;
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
                            let secs = start.elapsed().as_secs();
                            self.chat.update_thinking(secs);
                        }
                    }
                    AgentEvent::ToolCallDelta {
                        arguments_fragment, ..
                    } => {
                        self.activity_phase = crate::tui::ui::chat::ActivityPhase::Working;
                        self.decode_meter.note_output(arguments_fragment.len());
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
                        self.decode_meter.note_boundary();
                        let display = match parse_tool_start(&name, &arguments) {
                            ToolStartKind::Terminal { command, mode } => ToolDisplay::Terminal {
                                command,
                                mode,
                                status: super::ui::chat::ToolStatus::Running,
                                output: None,
                                diffs: Vec::new(),
                                expanded: false,
                            },
                            ToolStartKind::Generic { name, arguments } => ToolDisplay::Terminal {
                                command: format!("{name} {arguments}"),
                                mode: aither_sandbox::TerminalExecutionMode::Default,
                                status: super::ui::chat::ToolStatus::Running,
                                output: None,
                                diffs: Vec::new(),
                                expanded: false,
                            },
                        };
                        self.chat.push_tool(id, display);
                    }
                    AgentEvent::ToolCallEnd { id, result, .. } => {
                        // Back to waiting on the model for the next burst.
                        self.activity_phase = crate::tui::ui::chat::ActivityPhase::Thinking;
                        let feedback = parse_tool_end(&result);
                        match feedback {
                            ToolEndKind::Output(out) => {
                                let image = out
                                    .image
                                    .or_else(|| self.imagine_result_image(&id, &out.text));
                                self.chat.update_tool_with_output(
                                    &id,
                                    super::ui::chat::ToolStatus::Done,
                                    None,
                                    Some(out.text.as_str()),
                                );
                                if let Some(image) = image
                                    && let Some(image) = self.install_tool_image(&id, image)
                                {
                                    self.chat.set_tool_image(&id, image);
                                }
                            }
                            ToolEndKind::Error(err) => {
                                self.chat.update_tool_with_output(
                                    &id,
                                    super::ui::chat::ToolStatus::Failed(err),
                                    None,
                                    None,
                                );
                            }
                        }
                        self.live_reads.remove(&id);
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
                        self.scroll = usize::MAX;
                    }
                    AgentEvent::BuiltInToolResult { id, tool, result } => {
                        // The provider reports the call done; the model call
                        // that follows is thinking again.
                        self.activity_phase = crate::tui::ui::chat::ActivityPhase::Thinking;
                        self.decode_meter.note_boundary();
                        self.chat.finish_builtin(&id, &tool, result);
                    }
                    AgentEvent::Complete { .. } => {
                        // Text was already streamed via AgentEvent::Text events
                        // Just finalize the message
                        self.chat.collapse_thinking();
                        self.chat.end_text();
                        self.decode_meter.note_boundary();
                        self.scroll = usize::MAX;
                        self.input_visible = true;
                        // Signal that processing is complete
                        return true;
                    }
                    AgentEvent::Usage(u) => {
                        self.decode_meter.note_usage(u.completion_tokens);
                        // Accumulate token usage
                        if let Some(ref mut usage) = self.usage {
                            usage.accumulate(&u);
                        } else {
                            self.usage = Some(u);
                        }
                    }
                    // Lifecycle events are used for runtime coordination, not rendered.
                    _ => {}
                }
                false
            }
            Err(e) => {
                tracing::error!("Agent error: {}", e);
                self.decode_meter.note_boundary();
                // Display error to user
                self.chat.collapse_thinking();
                self.chat.push_error(&format!("{e}"));
                self.scroll = usize::MAX;
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

    /// Get current session ID for display.
    pub fn session_id_display(&self) -> String {
        self.session_manager
            .current()
            .map_or_else(|| "none".to_string(), |s| s.id.to_string())
    }

    /// Get model name for display.
    pub fn model_display(&self) -> String {
        // Show the actual model being used - no fallback
        self.current_model.as_ref().map_or_else(
            || "No model".to_string(),
            |model| {
                model.parse::<crate::ModelSelector>().map_or_else(
                    |_| model.clone(),
                    |selector| selector.model_id().to_string(),
                )
            },
        )
    }

    /// Set the current model being used.
    pub fn set_current_model(&mut self, model: &str) {
        self.current_model = Some(model.to_string());
        // Also update context window from model info
        let lookup_model = model.parse::<crate::ModelSelector>().map_or_else(
            |_| model.to_string(),
            |selector| selector.model_id().to_string(),
        );
        if let Some(info) = aither_models::lookup(&lookup_model) {
            self.context_window = info.max_input_tokens();
        }
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

/// Actions returned by event handling.
#[derive(Debug)]
pub enum SendAction {
    /// User wants to quit.
    Quit,
    /// User wants to create a new session.
    NewSession,
    /// User wants to load an existing session.
    LoadSession(SessionId),
    /// Send this message to the agent.
    Send(PendingMessage),
    /// Persist a pending message for later dispatch.
    EnqueuePending(PendingMessage),
    /// Show an open question of the session in the answer modal.
    OpenQuestions,
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
    /// Switch to a different model for this session.
    SwitchModel(String),
    /// Compact the conversation history with optional focus hint.
    Compact(Option<String>),
    /// Measure and display context-window usage.
    ContextReport,
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
    /// Submit a workspace approval response.
    RespondWorkspace {
        /// ID of the pending workspace request being answered.
        request_id: String,
        /// The workspace request that was presented to the user.
        request: crate::workspace_request::WorkspaceRequest,
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

#[cfg(test)]
mod tests {
    use super::utf8_complete_len;

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
}
