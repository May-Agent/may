//! Application state and logic.

use std::cell::Cell;
use std::path::PathBuf;

use async_channel::Receiver;
use blocking::unblock;
use crossterm::event::{KeyCode, KeyModifiers};
use executor_core::spawn;

use crate::config::{ModelTier, Provider};
use crate::models::FetchedModel;
use crate::ask_user::AskUserRequest;
use crate::workspace_request::WorkspaceRequest;
use crate::{ChatMessage, MayConfig, MessageRole, SessionId, SessionManager, SessionSummary};

use super::clipboard::{download_image, is_image_url, read_clipboard, ClipboardPasteResult, ImageManager, TextManager};
use super::completion::{Completion, CompletionMode};
use super::event::{AppEvent, EventSender};
use super::history::InputHistory;
use super::queue::{Attachment, MessageQueue, PendingMessage, LARGE_PASTE_THRESHOLD};
use super::ui::chat::{parse_tool_display, parse_tool_result, ChatElement, ChatStream};
use super::selection::{CoordinateMapper, Selection};
use super::ui::file_picker::FilePicker;

const ERR_TOOL_RESPONSE_SEND: &str = include_str!("../texts/tool_response_send_failed.txt");

/// Convert a character index to a byte index in a string.
fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Agent event from aither-agent.
pub use aither_agent::{AgentError, AgentEvent};

/// Application mode.
#[derive(Debug)]
pub enum AppMode {
    /// Main chat mode - typing and viewing
    Chat,
    /// Session selection popup
    SessionSelect,
    /// File picker popup
    FilePicker,
    /// Help screen
    Help,
    /// Full-screen tool detail view
    ToolDetail,
    /// Unified configuration screen with sections (Providers, Models, Preferences)
    Config,
    /// Quick model selector popup (anchored near input)
    ModelSelect,
    /// Model picker for tier configuration in Config screen
    TierModelPicker,
    /// GitHub Copilot OAuth authentication
    CopilotAuth,
    /// Workspace access confirmation dialog
    WorkspaceConfirm(WorkspaceRequest),
    /// MCP server management overlay
    McpManage,
    /// Ask user dialog
    AskUser(AskUserRequest, Vec<usize>),
    /// Skill management popup
    SkillManage,
}

/// Status of Copilot OAuth authentication flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopilotAuthStatus {
    /// Requesting device code from GitHub
    RequestingCode,
    /// Waiting for user to authorize in browser
    WaitingForUser {
        user_code: String,
        verification_uri: String,
    },
    /// Successfully authenticated
    Success,
    /// Authentication failed
    Failed(String),
}

/// Character filter for paste validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteFilter {
    /// Allow all characters
    Any,
    /// Only alphanumeric, dash, underscore (for identifiers)
    Identifier,
    /// Alphanumeric, dash, underscore, slash (for GitHub repos)
    GitHubRepo,
}

impl PasteFilter {
    /// Filter a string, keeping only valid characters.
    fn filter(self, s: &str) -> String {
        match self {
            PasteFilter::Any => s.to_string(),
            PasteFilter::Identifier => s
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect(),
            PasteFilter::GitHubRepo => s
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '/')
                .collect(),
        }
    }
}

/// Represents an active text input field for paste handling.
pub enum ActiveTextInput<'a> {
    /// Input with cursor position tracking (insert at cursor)
    Cursored {
        input: &'a mut String,
        cursor: &'a mut usize,
    },
    /// Simple input (append at end) with character filter
    Simple {
        input: &'a mut String,
        filter: PasteFilter,
    },
}

/// MCP add server mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpAddMode {
    /// Not adding a server
    #[default]
    None,
    /// Entering server name
    Name,
    /// Entering command
    Command,
}

/// Skill add mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkillAddMode {
    /// Not adding a skill
    #[default]
    None,
    /// Entering GitHub repo (owner/repo format)
    GitHubRepo,
}

/// Config section tabs (like Claude Code's /config).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigSection {
    #[default]
    Providers,
    Models,
    Preferences,
}

impl ConfigSection {
    /// All sections for iteration.
    pub const ALL: &'static [ConfigSection] = &[
        ConfigSection::Providers,
        ConfigSection::Models,
        ConfigSection::Preferences,
    ];

    /// Display name for the section tab.
    pub fn name(self) -> &'static str {
        match self {
            Self::Providers => "Providers",
            Self::Models => "Models",
            Self::Preferences => "Preferences",
        }
    }

    /// Next section (Tab).
    pub fn next(self) -> Self {
        match self {
            Self::Providers => Self::Models,
            Self::Models => Self::Preferences,
            Self::Preferences => Self::Providers,
        }
    }

    /// Previous section (Shift+Tab).
    pub fn prev(self) -> Self {
        match self {
            Self::Providers => Self::Preferences,
            Self::Models => Self::Providers,
            Self::Preferences => Self::Models,
        }
    }
}

/// What field is being edited in the config screen.
#[derive(Debug, Clone)]
pub enum ConfigEditMode {
    /// Not editing, just navigating
    None,
    /// Editing a text field (provider API key, base URL, etc.)
    Text {
        input: String,
        cursor: usize,
    },
    /// Selecting from a list (provider type, model list)
    List {
        items: Vec<String>,
        selected: usize,
    },
    /// Editing a custom provider (name, base_url, api_key)
    CustomProvider {
        /// Which field is focused (0=name, 1=base_url, 2=api_key)
        focused_field: usize,
        /// Provider name/ID
        name: String,
        /// Base URL (required)
        base_url: String,
        /// API key (optional)
        api_key: String,
        /// Cursor position in the focused field
        cursor: usize,
        /// Whether we're editing an existing provider
        editing_existing: bool,
        /// Original name if editing existing
        original_name: Option<String>,
    },
}

impl Default for ConfigEditMode {
    fn default() -> Self {
        Self::None
    }
}

/// Unified config state for all sections.
pub struct ConfigState {
    /// Current section (Tab to switch)
    pub section: ConfigSection,
    /// Selected row within current section
    pub selected: usize,
    /// Edit mode (text input, list selection, or none)
    pub edit_mode: ConfigEditMode,
    /// Cached provider list for display
    pub provider_ids: Vec<String>,
    /// Whether we detected API key from env var
    pub key_from_env: bool,
    /// Name of env var the key was found in
    pub key_env_var_name: Option<String>,
}

impl Default for ConfigState {
    fn default() -> Self {
        Self {
            section: ConfigSection::Providers,
            selected: 0,
            edit_mode: ConfigEditMode::None,
            provider_ids: Vec::new(),
            key_from_env: false,
            key_env_var_name: None,
        }
    }
}

impl ConfigState {
    /// Load provider IDs from config.
    pub fn load_providers(&mut self, config: &MayConfig) {
        self.provider_ids = config.provider.keys().cloned().collect();
        self.provider_ids.sort();
    }

    /// Get row count for current section.
    pub fn row_count(&self, _config: &MayConfig) -> usize {
        match self.section {
            ConfigSection::Providers => {
                // Providers list + "Add provider..."
                self.provider_ids.len() + 1
            }
            ConfigSection::Models => {
                // 4 tiers
                4
            }
            ConfigSection::Preferences => {
                // max_iterations, load_balance, warn_on_model_change
                3
            }
        }
    }

    /// Check if currently editing a text field.
    pub fn is_editing(&self) -> bool {
        !matches!(self.edit_mode, ConfigEditMode::None)
    }

    /// Start text editing mode.
    pub fn start_text_edit(&mut self, initial: &str) {
        let len = initial.len();
        self.edit_mode = ConfigEditMode::Text {
            input: initial.to_string(),
            cursor: len,
        };
    }

    /// Cancel editing.
    pub fn cancel_edit(&mut self) {
        self.edit_mode = ConfigEditMode::None;
    }

    /// Get current edit text (if in text edit mode).
    pub fn edit_text(&self) -> Option<(&str, usize)> {
        match &self.edit_mode {
            ConfigEditMode::Text { input, cursor } => Some((input, *cursor)),
            _ => None,
        }
    }

    /// Get mutable edit text.
    pub fn edit_text_mut(&mut self) -> Option<(&mut String, &mut usize)> {
        match &mut self.edit_mode {
            ConfigEditMode::Text { input, cursor } => Some((input, cursor)),
            _ => None,
        }
    }
}

/// Try to detect API key from environment variables for the given provider.
fn detect_api_key_from_env(provider: Provider) -> Option<(String, &'static str)> {
    let env_vars = match provider {
        Provider::Anthropic => &["ANTHROPIC_API_KEY", "CLAUDE_API_KEY"][..],
        Provider::OpenAI => &["OPENAI_API_KEY"][..],
        Provider::Google => &["GOOGLE_API_KEY", "GEMINI_API_KEY"][..],
        Provider::OpenRouter => &["OPENROUTER_API_KEY"][..],
        Provider::DeepSeek => &["DEEPSEEK_API_KEY"][..],
        Provider::Groq => &["GROQ_API_KEY"][..],
        Provider::Together => &["TOGETHER_API_KEY"][..],
        Provider::Mistral => &["MISTRAL_API_KEY"][..],
        Provider::XAI => &["XAI_API_KEY", "GROK_API_KEY"][..],
        Provider::Copilot | Provider::Ollama | Provider::Custom => return None,
    };

    for var_name in env_vars {
        if let Ok(key) = std::env::var(var_name) {
            if !key.is_empty() {
                return Some((key, *var_name));
            }
        }
    }
    None
}

/// Application state (UI only - agent managed externally).
pub struct App {
    /// Session manager
    pub session_manager: SessionManager,
    /// Configuration
    pub config: MayConfig,
    /// Current input text
    pub input: String,
    /// Input cursor position (character index, not byte index)
    pub cursor: usize,
    /// Cached character count for input (avoids O(n) .chars().count())
    pub input_char_count: usize,
    /// Pending attachments
    pub attachments: Vec<Attachment>,
    /// Chat stream for display
    pub chat: ChatStream,
    /// Scroll offset in chat (usize::MAX = follow bottom)
    pub scroll: usize,
    /// Whether chat input is shown (hidden during full-screen scroll)
    pub input_visible: bool,
    /// Last known content height (set during rendering for scroll normalization)
    pub content_height: Cell<usize>,
    /// View height (set during rendering for scroll normalization)
    pub view_height: Cell<usize>,
    /// Text selection state
    pub selection: Selection,
    /// Coordinate mapper for selection (rebuilt during render)
    pub coord_mapper: CoordinateMapper,
    /// Current mode
    pub mode: AppMode,
    /// Message queue
    pub queue: MessageQueue,
    /// Status message
    pub status: Option<String>,
    /// Background task count
    pub background_tasks: usize,
    /// Whether agent is currently processing
    pub processing: bool,
    /// File picker state (when in FilePicker mode)
    pub file_picker: Option<FilePicker>,
    /// Image manager for clipboard images
    pub image_manager: Option<ImageManager>,
    /// Text manager for clipboard text
    pub text_manager: Option<TextManager>,
    /// Input history for up/down navigation
    pub history: InputHistory,
    /// Input completion state
    pub completion: Completion,
    /// Session list for selection (when in SessionSelect mode)
    pub session_list: Vec<SessionSummary>,
    /// Selected session index (in filtered list)
    pub session_selected: usize,
    /// Session search query
    pub session_search: String,
    /// Session list scroll offset
    pub session_scroll: usize,
    /// Currently selected tool ID (for ToolDetail mode)
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
    /// When thinking started (for thinking models)
    pub thinking_start: Option<std::time::Instant>,
    /// Copilot OAuth authentication status
    pub copilot_auth_status: Option<CopilotAuthStatus>,
    /// Tier being edited in TierModelPicker mode
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
    /// Receiver for clipboard paste resolution (oneshot)
    pub clipboard_rx: Option<Receiver<Result<ClipboardPasteResult, String>>>,
    /// Pending paste results waiting to be handled
    pub pending_paste_results: std::collections::VecDeque<Result<ClipboardPasteResult, String>>,
    /// Pending attachment jobs (clipboard read / download)
    pub pending_attachment_jobs: usize,
    /// Cumulative token usage for current session
    pub usage: Option<aither_core::llm::Usage>,
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
    /// Current option selected in AskUser mode
    pub ask_user_cursor: usize,
    /// Event sender for spawning background tasks
    pub event_tx: EventSender,
    /// Terminal image rendering support (None if terminal doesn't support graphics)
    pub image_support: Option<super::ui::image::TerminalImageSupport>,
    /// Image cache: URL -> fetched image state (for terminal image rendering)
    pub image_cache: ahash::AHashMap<String, super::ui::image::ImageState>,
    /// URLs currently being fetched (to avoid duplicate requests)
    pub image_fetching: ahash::AHashSet<String>,
}

impl App {
    /// Create a new application.
    /// `image_support` should be detected before entering raw mode using `TerminalImageSupport::detect()`.
    pub fn new(
        event_tx: EventSender,
        image_support: Option<super::ui::image::TerminalImageSupport>,
    ) -> anyhow::Result<Self> {
        let mut config = MayConfig::load().unwrap_or_default();
        let session_manager = SessionManager::new()?;

        // Auto-detect API keys from environment variables
        let mut env_providers_added = Vec::new();
        for provider_type in Provider::ALL {
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
            if let Err(e) = config.save() {
                tracing::error!("Failed to save config after env detection: {}", e);
            }
        }

        let needs_setup = !config.is_configured();
        // Always start in Chat mode - show inline message if not configured
        let mode = AppMode::Chat;

        // Initialize config state
        let mut config_state = ConfigState::default();
        config_state.load_providers(&config);

        // Load input history
        let history = InputHistory::new(session_manager.may_dir());

        // Initialize completion
        let completion = Completion::new();

        // Get current working directory
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        // Build status message for env-detected providers
        let status = if !env_providers_added.is_empty() {
            let names: Vec<_> = env_providers_added.iter().map(|(id, _)| id.as_str()).collect();
            Some(format!("Auto-configured from env: {}", names.join(", ")))
        } else {
            None
        };

        Ok(Self {
            session_manager,
            config,
            input: String::new(),
            cursor: 0,
            input_char_count: 0,
            attachments: Vec::new(),
            chat: ChatStream::new(),
            scroll: usize::MAX, // Follow bottom by default
            input_visible: true,
            content_height: Cell::new(0),
            view_height: Cell::new(0),
            selection: Selection::new(),
            coord_mapper: CoordinateMapper::new(),
            mode,
            queue: MessageQueue::new(),
            status,
            background_tasks: 0,
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
            thinking_start: None,
            copilot_auth_status: None,
            tier_picker_tier: None,
            tier_picker_models: Vec::new(),
            tier_picker_filtered: Vec::new(),
            tier_picker_selected: 0,
            tier_picker_query: String::new(),
            tier_picker_loading: false,
            model_fetch_rx: None,
            clipboard_rx: None,
            pending_paste_results: std::collections::VecDeque::new(),
            pending_attachment_jobs: 0,
            usage: None,
            context_window: None,
            current_model: None,
            mcp_servers: Vec::new(),
            mcp_selected: 0,
            mcp_add_mode: McpAddMode::None,
            mcp_add_name: String::new(),
            mcp_add_command: String::new(),
            skill_list: Vec::new(),
            skill_selected: 0,
            skill_scroll: 0,
            skill_add_mode: SkillAddMode::None,
            skill_github_input: String::new(),
            skill_search: String::new(),
            ask_user_cursor: 0,
            event_tx,
            image_support,
            image_cache: ahash::AHashMap::new(),
            image_fetching: ahash::AHashSet::new(),
        })
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
        // Already fetching?
        if self.image_fetching.contains(url) {
            return false;
        }
        // Check if terminal supports images
        if self.image_support.is_none() {
            return false;
        }
        // Mark as fetching
        self.image_fetching.insert(url.to_string());

        // Spawn background fetch task
        let url_owned = url.to_string();
        let tx = self.event_tx.clone();
        tracing::debug!("Starting image fetch: {}", url);
        spawn(async move {
            match super::ui::image::fetch_image(&url_owned).await {
                Some(data) => {
                    tracing::debug!("Image fetch succeeded: {} ({} bytes)", url_owned, data.len());
                    if let Err(e) = tx.send(AppEvent::ImageFetched {
                        url: url_owned,
                        data,
                    }).await {
                        tracing::warn!("Failed to send ImageFetched event: {}", e);
                    }
                }
                None => {
                    tracing::debug!("Image fetch returned None: {}", url_owned);
                }
            }
        }).detach();

        false
    }

    /// Handle image fetched event.
    pub fn handle_image_fetched(&mut self, url: String, data: Vec<u8>) {
        tracing::debug!("Image fetched: url={}, {} bytes", url, data.len());
        self.image_fetching.remove(&url);
        if let Some(ref support) = self.image_support {
            if let Some(state) = support.create_image_state_from_bytes(&data) {
                tracing::debug!("Image cached successfully: {}", url);
                self.image_cache.insert(url, state);
            } else {
                tracing::debug!("Failed to create image state from bytes: {}", url);
            }
        } else {
            tracing::debug!("No image support, not caching: {}", url);
        }
    }

    /// Check if terminal supports image rendering.
    pub fn supports_images(&self) -> bool {
        self.image_support.is_some()
    }

    /// Get cached image state for a URL.
    pub fn get_cached_image(&self, url: &str) -> Option<&super::ui::image::ImageState> {
        self.image_cache.get(url)
    }

    /// Scan chat content for images and request fetches for any not yet cached.
    pub fn scan_and_fetch_images(&mut self) {
        use super::ui::chat::ChatElement;
        use super::ui::markdown::extract_images;

        // Collect image URLs first to avoid borrow issues
        let urls: Vec<String> = self.chat.elements().iter().flat_map(|elem| {
            if let ChatElement::AssistantText { content, .. } = elem {
                extract_images(content.as_str()).into_iter().map(|(_, url)| url).collect::<Vec<_>>()
            } else {
                Vec::new()
            }
        }).collect();

        if !urls.is_empty() {
            tracing::debug!("scan_and_fetch_images: found {} URLs: {:?}", urls.len(), urls);
        }

        for url in urls {
            self.request_image(&url);
        }
    }

    /// Normalize scroll value if it's at the "follow bottom" sentinel.
    /// This converts usize::MAX to actual max scroll position.
    pub fn normalize_scroll(&mut self) {
        let content = self.content_height.get();
        let view = self.view_height.get();
        if self.scroll > content {
            let max_scroll = content.saturating_sub(view);
            self.scroll = max_scroll;
        }
    }

    /// Scroll up by one line.
    fn scroll_up(&mut self) {
        self.scroll_up_by(1);
    }

    /// Scroll down by one line.
    fn scroll_down(&mut self) {
        self.scroll_down_by(1);
    }

    /// Scroll up by a number of lines.
    fn scroll_up_by(&mut self, lines: usize) {
        if self.scroll == usize::MAX {
            self.normalize_scroll();
        }
        self.scroll = self.scroll.saturating_sub(lines);
    }

    /// Scroll down by a number of lines.
    fn scroll_down_by(&mut self, lines: usize) {
        if self.scroll == usize::MAX {
            self.normalize_scroll();
        }

        self.scroll = self.scroll.saturating_add(lines);

        // Check if we've reached the bottom - snap to sentinel
        let content = self.content_height.get();
        let view = self.view_height.get();
        let max_scroll = content.saturating_sub(view);

        if self.scroll >= max_scroll {
            self.scroll = usize::MAX;
        }
    }

    /// Handle a mouse wheel scroll event based on current mode.
    fn handle_scroll(&mut self, delta: i16) {
        match self.mode {
            AppMode::Chat => {
                if delta > 0 {
                    self.scroll_up();
                } else if delta < 0 {
                    self.scroll_down();
                }
            }
            AppMode::ToolDetail => {
                if delta > 0 {
                    self.tool_detail_scroll = self.tool_detail_scroll.saturating_sub(1);
                } else if delta < 0 {
                    self.tool_detail_scroll = self.tool_detail_scroll.saturating_add(1);
                }
            }
            _ => {}
        }
    }

    /// Handle mouse button down event.
    fn handle_mouse_down(&mut self, x: u16, y: u16) {
        // Only handle in chat mode
        if !matches!(self.mode, AppMode::Chat) {
            return;
        }

        // Clear any existing selection and start a new one
        if let Some(pos) = self.coord_mapper.screen_to_position(x, y) {
            self.selection.start(pos);
        }
    }

    /// Handle mouse drag event.
    fn handle_mouse_drag(&mut self, x: u16, y: u16) {
        // Only handle in chat mode
        if !matches!(self.mode, AppMode::Chat) {
            return;
        }

        if let Some(pos) = self.coord_mapper.screen_to_position(x, y) {
            self.selection.extend(pos);
        }
    }

    /// Handle mouse button up event.
    fn handle_mouse_up(&mut self, x: u16, y: u16) {
        // Only handle in chat mode
        if !matches!(self.mode, AppMode::Chat) {
            return;
        }

        // Extend to final position
        if let Some(pos) = self.coord_mapper.screen_to_position(x, y) {
            self.selection.extend(pos);
        }

        // Finish selection
        self.selection.finish();

        // Copy to clipboard if selection is not empty
        if self.selection.is_active() {
            if let Some((start, end)) = self.selection.range() {
                let text = self.coord_mapper.extract(start, end);
                if !text.is_empty() {
                    if let Err(e) = super::clipboard::set_clipboard_text(&text) {
                        tracing::warn!("Failed to copy selection to clipboard: {}", e);
                    }
                }
            }
        }
    }

    /// Clear the current selection.
    pub fn clear_selection(&mut self) {
        self.selection.clear();
    }

    /// Initialize for a session (sets up image manager and status).
    ///
    /// Takes the session ID and a slice of messages to avoid borrow conflicts
    /// with the session manager.
    pub fn init_session(&mut self, session_id: &SessionId, messages: &[ChatMessage], model: Option<&str>) {
        // Load chat history into display
        self.chat = ChatStream::new();
        for msg in messages {
            match msg.role {
                MessageRole::User => {
                    self.chat.push_user(&msg.content, vec![]);
                }
                MessageRole::Assistant => {
                    self.chat.push_text(&msg.content);
                    self.chat.end_text();
                }
                _ => {}
            }
        }

        let session_dir = self.session_manager.store().session_dir(session_id.as_str());
        self.image_manager = Some(ImageManager::new(&session_dir));
        self.text_manager = Some(TextManager::new(&session_dir));

        // Reset usage tracking for new session
        self.usage = None;

        // Set context window from model info
        if let Some(model_name) = model {
            if let Some(info) = aither_models::lookup(model_name) {
                self.context_window = Some(info.context_window);
            } else {
                self.context_window = None;
            }
        } else {
            self.context_window = None;
        }
    }

    /// Handle an event. Returns Some(message) if user wants to send a message.
    pub fn handle_event(&mut self, event: AppEvent) -> anyhow::Result<Option<SendAction>> {
        match event {
            AppEvent::Key(code, modifiers) => self.handle_key(code, modifiers),
            AppEvent::Tick => {
                // Animate loading spinner if processing
                if self.processing {
                    self.chat.tick_loading();
                    // Update thinking time if in thinking state
                    if let Some(start) = self.thinking_start {
                        let secs = start.elapsed().as_secs();
                        self.chat.update_thinking(secs);
                    }
                }
                Ok(None)
            }
            AppEvent::Resize(_, _) => Ok(None),
            AppEvent::Scroll(delta) => {
                self.handle_scroll(delta);
                Ok(None)
            }
            AppEvent::MouseDown { x, y } => {
                self.handle_mouse_down(x, y);
                Ok(None)
            }
            AppEvent::MouseDrag { x, y } => {
                self.handle_mouse_drag(x, y);
                Ok(None)
            }
            AppEvent::MouseUp { x, y } => {
                self.handle_mouse_up(x, y);
                Ok(None)
            }
            AppEvent::Paste(text) => self.handle_bracketed_paste(text),

            // ─── Async operation results ───────────────────────────────────
            AppEvent::SessionsLoaded(result) => {
                self.status = None;
                match result {
                    Ok(sessions) => {
                        self.session_list = sessions;
                        self.session_selected = 0;
                        self.session_search.clear();
                        self.session_scroll = 0;
                        self.mode = AppMode::SessionSelect;
                    }
                    Err(e) => {
                        self.chat.push_error(&format!("Failed to load sessions: {}", e));
                    }
                }
                Ok(None)
            }
            AppEvent::SessionDeleted { was_current, result } => {
                self.status = None;
                match result {
                    Ok(sessions) => {
                        self.session_list = sessions;
                        // Adjust selection if needed
                        if self.session_selected >= self.session_list.len()
                            && self.session_selected > 0
                        {
                            self.session_selected -= 1;
                        }
                        // If we deleted the current session, create a new one
                        if was_current {
                            self.mode = AppMode::Chat;
                            return Ok(Some(SendAction::NewSession));
                        }
                    }
                    Err(e) => {
                        self.chat.push_error(&format!("Failed to delete session: {}", e));
                    }
                }
                Ok(None)
            }

            // ─── Skill events ────────────────────────────────────────────
            AppEvent::SkillsLoaded(result) => {
                self.status = None;
                match result {
                    Ok(skills) => {
                        self.skill_list = skills;
                        self.skill_selected = 0;
                        self.mode = AppMode::SkillManage;
                    }
                    Err(e) => {
                        self.chat.push_error(&format!("Failed to load skills: {}", e));
                    }
                }
                Ok(None)
            }
            AppEvent::SkillInstalled(result) => {
                self.status = None;
                match result {
                    Ok(skills) => {
                        self.skill_list = skills;
                        // Adjust selection if needed
                        if self.skill_selected >= self.skill_list.len() && self.skill_selected > 0 {
                            self.skill_selected = self.skill_list.len().saturating_sub(1);
                        }
                    }
                    Err(e) => {
                        self.chat.push_error(&format!("Failed to install skill: {}", e));
                    }
                }
                Ok(None)
            }
            AppEvent::SkillDeleted(result) => {
                self.status = None;
                match result {
                    Ok(skills) => {
                        self.skill_list = skills;
                        // Adjust selection if needed
                        if self.skill_selected >= self.skill_list.len() && self.skill_selected > 0 {
                            self.skill_selected -= 1;
                        }
                    }
                    Err(e) => {
                        self.chat.push_error(&format!("Failed to delete skill: {}", e));
                    }
                }
                Ok(None)
            }
            AppEvent::ImageFetched { url, data } => {
                self.handle_image_fetched(url, data);
                Ok(None)
            }
        }
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
            AppMode::McpManage => {
                match self.mcp_add_mode {
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
                }
            }
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
            AppMode::FilePicker => {
                // FilePicker is handled separately (has its own insert_str method)
            }
            _ => {}
        }
        None
    }

    /// Handle bracketed paste event.
    fn handle_bracketed_paste(&mut self, text: String) -> anyhow::Result<Option<SendAction>> {
        // Special handling for Chat mode: large pastes become attachments
        if matches!(self.mode, AppMode::Chat)
            && (text.contains('\n') || text.len() > LARGE_PASTE_THRESHOLD)
        {
            self.status = Some("Saving pasted text...".to_string());
            self.enqueue_paste_result(Ok(ClipboardPasteResult::text(text)));
            return Ok(None);
        }

        // Use first line only for non-chat inputs (URLs, API keys, etc. are single-line)
        let text = if matches!(self.mode, AppMode::Chat) {
            text
        } else {
            text.lines().next().unwrap_or("").to_string()
        };

        if text.is_empty() {
            return Ok(None);
        }

        // Handle FilePicker separately (has its own insert_str method)
        if matches!(self.mode, AppMode::FilePicker) {
            if let Some(picker) = &mut self.file_picker {
                picker.insert_str(&text);
            }
            return Ok(None);
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

        Ok(None)
    }

    /// Handle a key event.
    fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> anyhow::Result<Option<SendAction>> {
        // Global shortcuts
        if modifiers.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('c') => {
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
                    self.status = Some("^C Press Ctrl+C again to exit".to_string());
                    return Ok(None);
                }
                KeyCode::Char('q') => return Ok(Some(SendAction::Quit)),
                KeyCode::Char('n') => {
                    self.quit_pending = false;
                    self.status = None;
                    return Ok(Some(SendAction::NewSession));
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
            AppMode::Chat => self.handle_chat_key(code, modifiers),
            AppMode::SessionSelect => self.handle_session_select_key(code),
            AppMode::FilePicker => self.handle_file_picker_key(code),
            AppMode::Help => self.handle_help_key(code),
            AppMode::ToolDetail => self.handle_tool_detail_key(code),
            AppMode::Config => self.handle_config_key(code, modifiers),
            AppMode::ModelSelect => self.handle_model_select_key(code),
            AppMode::TierModelPicker => self.handle_tier_model_picker_key(code),
            AppMode::CopilotAuth => self.handle_copilot_auth_key(code),
            AppMode::WorkspaceConfirm(_) => self.handle_workspace_confirm_key(code),
            AppMode::McpManage => self.handle_mcp_key(code),
            AppMode::AskUser(_, _) => self.handle_ask_user_key(code),
            AppMode::SkillManage => self.handle_skill_key(code),
        }
    }

    /// Handle key in Copilot OAuth authentication mode.
    fn handle_copilot_auth_key(&mut self, code: KeyCode) -> anyhow::Result<Option<SendAction>> {
        match code {
            KeyCode::Esc => {
                // Cancel auth and go back to config
                self.copilot_auth_status = None;
                self.mode = AppMode::Config;
            }
            KeyCode::Enter => {
                // If waiting for user, open URL in browser and copy code
                if let Some(CopilotAuthStatus::WaitingForUser { user_code, verification_uri }) = &self.copilot_auth_status {
                    return Ok(Some(SendAction::OpenCopilotAuthUrl {
                        url: verification_uri.clone(),
                        code: user_code.clone(),
                    }));
                }
                // If auth succeeded or failed, go back to config
                if matches!(
                    self.copilot_auth_status,
                    Some(CopilotAuthStatus::Success | CopilotAuthStatus::Failed(_))
                ) {
                    self.copilot_auth_status = None;
                    self.mode = AppMode::Config;
                }
            }
            _ => {}
        }
        Ok(None)
    }

    /// Handle key in workspace confirmation dialog.
    fn handle_workspace_confirm_key(&mut self, code: KeyCode) -> anyhow::Result<Option<SendAction>> {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                // Approve: take the request and approve it
                let mode = std::mem::replace(&mut self.mode, AppMode::Chat);
                if let AppMode::WorkspaceConfirm(request) = mode {
                    request
                        .respond(true)
                        .expect(ERR_TOOL_RESPONSE_SEND.trim());
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                // Deny: take the request and deny it
                let mode = std::mem::replace(&mut self.mode, AppMode::Chat);
                if let AppMode::WorkspaceConfirm(request) = mode {
                    request
                        .respond(false)
                        .expect(ERR_TOOL_RESPONSE_SEND.trim());
                }
            }
            _ => {}
        }
        Ok(None)
    }

    /// Handle key in AskUser mode.
    fn handle_ask_user_key(&mut self, code: KeyCode) -> anyhow::Result<Option<SendAction>> {
        let (request, selections) = match &mut self.mode {
            AppMode::AskUser(req, sels) => (req, sels),
            _ => return Ok(None),
        };

        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.ask_user_cursor > 0 {
                    self.ask_user_cursor -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.ask_user_cursor + 1 < request.args.options.len() {
                    self.ask_user_cursor += 1;
                }
            }
            KeyCode::Char(' ') => {
                // Toggle selection
                if request.args.multi_select {
                    let cursor = self.ask_user_cursor;
                    if selections.iter().any(|&x| x == cursor) {
                        selections.retain(|&x| x != cursor);
                    } else {
                        selections.push(cursor);
                    }
                } else {
                    selections.clear();
                    selections.push(self.ask_user_cursor);
                }
            }
            KeyCode::Enter => {
                // If single select and nothing selected, select current cursor
                if !request.args.multi_select && selections.is_empty() {
                    selections.push(self.ask_user_cursor);
                }

                // If multi select and nothing selected, do nothing or allow empty?
                // For now, let's require at least one selection if options exist
                if !request.args.options.is_empty() && selections.is_empty() {
                    return Ok(None);
                }

                // Get selected values
                use itertools::Itertools;
                let selected_values: Vec<String> = selections
                    .iter()
                    .sorted()
                    .filter_map(|&idx| request.args.options.get(idx).cloned())
                    .collect();

                // Send response
                let mode = std::mem::replace(&mut self.mode, AppMode::Chat);
                if let AppMode::AskUser(req, _) = mode {
                    req.respond(selected_values)
                        .expect(ERR_TOOL_RESPONSE_SEND.trim());
                }
            }
            _ => {}
        }
        Ok(None)
    }

    /// Handle key in MCP management mode.
    fn handle_mcp_key(&mut self, code: KeyCode) -> anyhow::Result<Option<SendAction>> {
        match self.mcp_add_mode {
            McpAddMode::None => {
                // Normal navigation mode
                match code {
                    KeyCode::Esc => {
                        self.mode = AppMode::Chat;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        if self.mcp_selected > 0 {
                            self.mcp_selected -= 1;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if self.mcp_selected + 1 < self.mcp_servers.len() {
                            self.mcp_selected += 1;
                        }
                    }
                    KeyCode::Char('a') => {
                        // Start add mode
                        self.mcp_add_mode = McpAddMode::Name;
                        self.mcp_add_name.clear();
                        self.mcp_add_command.clear();
                    }
                    KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                        // Delete selected server
                        if let Some(server) = self.mcp_servers.get(self.mcp_selected) {
                            let name = server.name.clone();
                            return Ok(Some(SendAction::McpRemove { name }));
                        }
                    }
                    KeyCode::Char('r') => {
                        // Reconnect selected server
                        if let Some(server) = self.mcp_servers.get(self.mcp_selected) {
                            let name = server.name.clone();
                            return Ok(Some(SendAction::McpReconnect { name }));
                        }
                    }
                    _ => {}
                }
            }
            McpAddMode::Name => {
                match code {
                    KeyCode::Esc => {
                        self.mcp_add_mode = McpAddMode::None;
                    }
                    KeyCode::Enter => {
                        if !self.mcp_add_name.is_empty() {
                            self.mcp_add_mode = McpAddMode::Command;
                        }
                    }
                    KeyCode::Backspace => {
                        self.mcp_add_name.pop();
                    }
                    KeyCode::Char(c) => {
                        // Only allow valid identifier characters
                        if c.is_alphanumeric() || c == '-' || c == '_' {
                            self.mcp_add_name.push(c);
                        }
                    }
                    _ => {}
                }
            }
            McpAddMode::Command => {
                match code {
                    KeyCode::Esc => {
                        self.mcp_add_mode = McpAddMode::Name;
                    }
                    KeyCode::Enter => {
                        if !self.mcp_add_command.is_empty() {
                            // Parse command into program and args
                            let parts: Vec<&str> = self.mcp_add_command.split_whitespace().collect();
                            if let Some((command, args)) = parts.split_first() {
                                let name = std::mem::take(&mut self.mcp_add_name);
                                let command_str = (*command).to_string();
                                let args_vec: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
                                self.mcp_add_mode = McpAddMode::None;
                                self.mcp_add_command.clear();
                                return Ok(Some(SendAction::McpAdd {
                                    name,
                                    command: command_str,
                                    args: args_vec,
                                }));
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        self.mcp_add_command.pop();
                    }
                    KeyCode::Char(c) => {
                        self.mcp_add_command.push(c);
                    }
                    _ => {}
                }
            }
        }
        Ok(None)
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
                        .map(|t| t.to_lowercase().contains(&query))
                        .unwrap_or(false)
                        // Search in content preview
                        || s.content_preview.to_lowercase().contains(&query)
                })
                .collect()
        }
    }

    /// Handle key in skill management mode.
    fn handle_skill_key(&mut self, code: KeyCode) -> anyhow::Result<Option<SendAction>> {
        match self.skill_add_mode {
            SkillAddMode::None => {
                // Normal navigation mode with search
                match code {
                    KeyCode::Esc => {
                        if !self.skill_search.is_empty() {
                            // Clear search first
                            self.skill_search.clear();
                            self.skill_selected = 0;
                            self.skill_scroll = 0;
                        } else {
                            self.mode = AppMode::Chat;
                            self.skill_search.clear();
                            self.skill_scroll = 0;
                        }
                    }
                    KeyCode::Up => {
                        if self.skill_selected > 0 {
                            self.skill_selected -= 1;
                            // Scroll up if needed
                            if self.skill_selected < self.skill_scroll {
                                self.skill_scroll = self.skill_selected;
                            }
                        }
                    }
                    KeyCode::Down => {
                        let filtered_len = self.filtered_skills().len();
                        if self.skill_selected + 1 < filtered_len {
                            self.skill_selected += 1;
                        }
                    }
                    KeyCode::Backspace => {
                        self.skill_search.pop();
                        self.skill_selected = 0;
                        self.skill_scroll = 0;
                    }
                    KeyCode::Char('a') if self.skill_search.is_empty() => {
                        // Start GitHub repo input mode (only if not searching)
                        self.skill_add_mode = SkillAddMode::GitHubRepo;
                        self.skill_github_input.clear();
                    }
                    KeyCode::Char('d') if self.skill_search.is_empty() => {
                        // Delete selected skill (only if not searching)
                        let filtered = self.filtered_skills();
                        if let Some(skill) = filtered.get(self.skill_selected) {
                            let dir_name = skill.dir_name.clone();
                            return Ok(Some(SendAction::SkillDelete { dir_name }));
                        }
                    }
                    KeyCode::Delete => {
                        // Delete selected skill
                        let filtered = self.filtered_skills();
                        if let Some(skill) = filtered.get(self.skill_selected) {
                            let dir_name = skill.dir_name.clone();
                            return Ok(Some(SendAction::SkillDelete { dir_name }));
                        }
                    }
                    KeyCode::Enter => {
                        // Select skill and insert into input
                        let filtered = self.filtered_skills();
                        if let Some(skill) = filtered.get(self.skill_selected) {
                            let prefix = format!("Use {} skill, ", skill.name);
                            let prefix_len = prefix.chars().count();
                            self.input.insert_str(0, &prefix);
                            self.cursor += prefix_len;
                            self.mode = AppMode::Chat;
                            self.skill_search.clear();
                            self.skill_scroll = 0;
                        }
                    }
                    KeyCode::Char(c) => {
                        // Add to search query
                        self.skill_search.push(c);
                        self.skill_selected = 0;
                        self.skill_scroll = 0;
                    }
                    _ => {}
                }
            }
            SkillAddMode::GitHubRepo => {
                match code {
                    KeyCode::Esc => {
                        self.skill_add_mode = SkillAddMode::None;
                        self.skill_github_input.clear();
                    }
                    KeyCode::Enter => {
                        if !self.skill_github_input.is_empty() {
                            let input = std::mem::take(&mut self.skill_github_input);
                            self.skill_add_mode = SkillAddMode::None;
                            return Ok(Some(SendAction::SkillInstall { repo: input }));
                        }
                    }
                    KeyCode::Backspace => {
                        self.skill_github_input.pop();
                    }
                    KeyCode::Char(c) => {
                        // Allow alphanumeric, dash, underscore, and slash
                        if c.is_alphanumeric() || c == '-' || c == '_' || c == '/' {
                            self.skill_github_input.push(c);
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(None)
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

    /// Handle key in chat mode (unified input + navigation).
    fn handle_chat_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> anyhow::Result<Option<SendAction>> {
        // Clear selection on any keyboard input
        self.selection.clear();

        // Handle Ctrl+V for paste
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('v') {
            self.handle_paste()?;
            return Ok(None);
        }

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
                    return Ok(None);
                }
                if self.has_pending_attachments() {
                    self.status = Some("Waiting for attachments to finish...".to_string());
                    return Ok(None);
                }
                // Send as message (slash commands only work from menu selection)
                if !self.input.is_empty() || !self.attachments.is_empty() {
                    return Ok(Some(self.prepare_message()));
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
                // Up navigates completion if active
                if self.completion.is_active() {
                    self.completion.prev();
                } else if self.input.is_empty() || self.history.is_navigating() {
                    // Up navigates history when input is empty or already navigating
                    if let Some(entry) = self.history.prev(&self.input) {
                        self.input = entry.to_string();
                        self.input_char_count = self.input.chars().count();
                        self.cursor = self.input_char_count;
                    }
                } else {
                    self.scroll_up();
                }
            }
            KeyCode::Down => {
                // Down navigates completion if active
                if self.completion.is_active() {
                    self.completion.next();
                } else if self.history.is_navigating() {
                    // Down navigates history when already navigating
                    if let Some(entry) = self.history.next() {
                        self.input = entry.to_string();
                        self.input_char_count = self.input.chars().count();
                        self.cursor = self.input_char_count;
                    }
                } else {
                    self.scroll_down();
                }
            }
            KeyCode::PageUp => {
                self.scroll_up_by(10);
            }
            KeyCode::PageDown => {
                self.scroll_down_by(10);
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
                } else if !self.input.is_empty() {
                    self.input.clear();
                    self.cursor = 0;
                    self.input_char_count = 0;
                }
            }
            KeyCode::Tab => {
                // Accept completion if active
                if self.completion.is_active() {
                    if let Some(item) = self.completion.complete() {
                        self.accept_completion(&item.insert.clone());
                    }
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
        Ok(None)
    }

    /// Execute a slash command directly (from menu selection only).
    fn execute_slash_command(&mut self, cmd: &str) -> anyhow::Result<Option<SendAction>> {
        match cmd {
            "/session" => {
                let tx = self.event_tx.clone();
                let store = self.session_manager.store().clone();
                spawn(async move {
                    let result = store.list_session_summaries().await;
                    let _ = tx
                        .send(AppEvent::SessionsLoaded(
                            result.map_err(|e| e.to_string()),
                        ))
                        .await;
                })
                .detach();
                self.status = Some("Loading skills...".to_string());
                Ok(None)
            }
            "/new" => Ok(Some(SendAction::NewSession)),
            "/sandbox" => {
                if let Some(session) = self.session_manager.current() {
                    let sandbox_dir = &session.sandbox_dir;
                    #[cfg(target_os = "macos")]
                    {
                        let _ = std::process::Command::new("open")
                            .arg(sandbox_dir)
                            .spawn();
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
                Ok(None)
            }
            "/config" => {
                self.config_state = ConfigState::default();
                self.config_state.load_providers(&self.config);
                self.mode = AppMode::Config;
                Ok(None)
            }
            "/model" => {
                self.model_select_list = self.collect_all_models();
                self.model_select_index = 0;
                if let Some(current) = self.current_model.as_deref().or(self.config.last_model.as_deref()) {
                    if let Some(idx) = self.model_select_list.iter().position(|m| m == current) {
                        self.model_select_index = idx;
                    }
                }
                self.mode = AppMode::ModelSelect;
                Ok(None)
            }
            "/quit" => Ok(Some(SendAction::Quit)),
            "/compact" => Ok(Some(SendAction::Compact)),
            "/mcp" => {
                self.mode = AppMode::McpManage;
                Ok(None)
            }
            "/skill" => {
                let tx = self.event_tx.clone();
                let may_dir = self.session_manager.may_dir().to_path_buf();
                spawn(async move {
                    let manager = crate::skill::SkillManager::new(&may_dir);
                    let result = manager.list_skills().await;
                    let _ = tx
                        .send(AppEvent::SkillsLoaded(result.map_err(|e| e.to_string())))
                        .await;
                })
                .detach();
                self.status = Some("Loading skills...".to_string());
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Handle key in session select mode.
    fn handle_session_select_key(&mut self, code: KeyCode) -> anyhow::Result<Option<SendAction>> {
        let filtered = self.filtered_sessions();
        let filtered_len = filtered.len();

        match code {
            KeyCode::Esc => {
                if !self.session_search.is_empty() {
                    // Clear search first
                    self.session_search.clear();
                    self.session_selected = 0;
                    self.session_scroll = 0;
                } else {
                    self.mode = AppMode::Chat;
                }
            }
            KeyCode::Up => {
                if self.session_selected > 0 {
                    self.session_selected -= 1;
                }
            }
            KeyCode::Down => {
                if self.session_selected + 1 < filtered_len {
                    self.session_selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some((original_idx, _)) = filtered.get(self.session_selected) {
                    if let Some(summary) = self.session_list.get(*original_idx) {
                        let id = summary.id.clone();
                        self.mode = AppMode::Chat;
                        return Ok(Some(SendAction::LoadSession(id)));
                    }
                }
            }
            KeyCode::Backspace => {
                self.session_search.pop();
                // Reset selection when search changes
                self.session_selected = 0;
                self.session_scroll = 0;
            }
            KeyCode::Char(c) => {
                // Special keys only work when search is empty
                if self.session_search.is_empty() {
                    match c {
                        'k' => {
                            if self.session_selected > 0 {
                                self.session_selected -= 1;
                            }
                            return Ok(None);
                        }
                        'j' => {
                            if self.session_selected + 1 < filtered_len {
                                self.session_selected += 1;
                            }
                            return Ok(None);
                        }
                        'n' => {
                            self.mode = AppMode::Chat;
                            return Ok(Some(SendAction::NewSession));
                        }
                        'd' => {
                            // Delete selected session (async)
                            if let Some((original_idx, _)) = filtered.get(self.session_selected) {
                                if let Some(summary) = self.session_list.get(*original_idx) {
                                    let deleted_id = summary.id.as_str().to_string();
                                    let was_current = self
                                        .session_manager
                                        .current()
                                        .map(|s| s.id.as_str() == deleted_id)
                                        .unwrap_or(false);

                                    // Spawn background task to delete and refresh
                                    let tx = self.event_tx.clone();
                                    let store = self.session_manager.store().clone();
                                    spawn(async move {
                                        // Delete the session
                                        if let Err(e) = store.delete_session(&deleted_id).await {
                                            let _ = tx
                                                .send(AppEvent::SessionDeleted {
                                                    was_current,
                                                    result: Err(e.to_string()),
                                                })
                                                .await;
                                            return;
                                        }
                                        // Refresh list
                                        let result = store.list_session_summaries().await;
                                        let _ = tx
                                            .send(AppEvent::SessionDeleted {
                                                was_current,
                                                result: result.map_err(|e| e.to_string()),
                                            })
                                            .await;
                                    })
                                    .detach();
                                    self.status = Some("Deleting session...".to_string());
                                }
                            }
                            return Ok(None);
                        }
                        _ => {}
                    }
                }
                // Add character to search
                self.session_search.push(c);
                // Reset selection when search changes
                self.session_selected = 0;
                self.session_scroll = 0;
            }
            _ => {}
        }
        Ok(None)
    }

    /// Handle key in help mode.
    fn handle_help_key(&mut self, code: KeyCode) -> anyhow::Result<Option<SendAction>> {
        match code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                self.mode = AppMode::Chat;
            }
            _ => {}
        }
        Ok(None)
    }

    /// Handle key in tool detail mode.
    fn handle_tool_detail_key(&mut self, code: KeyCode) -> anyhow::Result<Option<SendAction>> {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = AppMode::Chat;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.tool_detail_scroll = self.tool_detail_scroll.saturating_add(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.tool_detail_scroll = self.tool_detail_scroll.saturating_sub(1);
            }
            KeyCode::Char('G') => {
                // Go to bottom
                self.tool_detail_scroll = usize::MAX;
            }
            KeyCode::Char('g') => {
                // Go to top
                self.tool_detail_scroll = 0;
            }
            _ => {}
        }
        Ok(None)
    }

    /// Handle key in file picker mode.
    fn handle_file_picker_key(&mut self, code: KeyCode) -> anyhow::Result<Option<SendAction>> {
        let Some(picker) = self.file_picker.as_mut() else {
            self.mode = AppMode::Chat;
            return Ok(None);
        };

        match code {
            KeyCode::Esc => {
                self.file_picker = None;
                self.mode = AppMode::Chat;
            }
            KeyCode::Enter => {
                if let Some(path) = picker.confirm() {
                    // Add file as attachment
                    self.attachments.push(Attachment::FileReference { path });
                }
                self.file_picker = None;
                self.mode = AppMode::Chat;
            }
            KeyCode::Up => {
                picker.select_prev();
            }
            KeyCode::Down => {
                picker.select_next();
            }
            KeyCode::Backspace => {
                picker.delete_char();
            }
            KeyCode::Char(c) => {
                picker.insert_char(c);
            }
            _ => {}
        }
        Ok(None)
    }

    /// Handle key in tier model picker mode.
    fn handle_tier_model_picker_key(&mut self, code: KeyCode) -> anyhow::Result<Option<SendAction>> {
        match code {
            KeyCode::Esc => {
                // Cancel and go back to config
                self.mode = AppMode::Config;
                self.tier_picker_query.clear();
            }
            KeyCode::Enter => {
                // Select model and add to tier
                if let Some(&idx) = self.tier_picker_filtered.get(self.tier_picker_selected) {
                    if let Some(model) = self.tier_picker_models.get(idx) {
                        if let Some(tier) = self.tier_picker_tier {
                            self.config.models.add_model(tier, &model.id, &model.provider_id);
                            if let Err(e) = self.config.save() {
                                tracing::error!("Failed to save config: {}", e);
                            }
                        }
                    }
                }
                self.mode = AppMode::Config;
                self.tier_picker_query.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.tier_picker_selected = self.tier_picker_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.tier_picker_filtered.is_empty() {
                    self.tier_picker_selected =
                        (self.tier_picker_selected + 1).min(self.tier_picker_filtered.len() - 1);
                }
            }
            KeyCode::Backspace => {
                self.tier_picker_query.pop();
                self.update_tier_picker_filter();
            }
            KeyCode::Char(c) => {
                self.tier_picker_query.push(c);
                self.update_tier_picker_filter();
            }
            _ => {}
        }
        Ok(None)
    }

    /// Update tier picker filtered list based on query.
    fn update_tier_picker_filter(&mut self) {
        let query_lower = self.tier_picker_query.to_lowercase();
        self.tier_picker_filtered = self
            .tier_picker_models
            .iter()
            .enumerate()
            .filter(|(_, model)| {
                query_lower.is_empty() || model.id.to_lowercase().contains(&query_lower)
            })
            .map(|(i, _)| i)
            .collect();
        // Reset selection if out of bounds
        if self.tier_picker_selected >= self.tier_picker_filtered.len() {
            self.tier_picker_selected = 0;
        }
    }

    /// Open tier model picker for a specific tier.
    /// Returns a receiver to poll for fetched models if async fetch was started.
    pub fn open_tier_model_picker(&mut self, tier: ModelTier) -> Option<Receiver<Vec<FetchedModel>>> {
        self.tier_picker_tier = Some(tier);
        self.tier_picker_query.clear();
        self.tier_picker_selected = 0;
        self.tier_picker_models.clear();
        self.tier_picker_filtered.clear();
        self.tier_picker_loading = true;
        self.mode = AppMode::TierModelPicker;

        // Create channel and spawn fetch task
        let (tx, rx) = async_channel::bounded(1);
        let config = self.config.clone();
        spawn(async move {
            let models = crate::models::fetch_all_models(&config).await;
            let _ = tx.send(models).await;
        })
        .detach();

        Some(rx)
    }

    /// Queue a paste result to be handled by the main loop.
    pub fn enqueue_paste_result(&mut self, result: Result<ClipboardPasteResult, String>) {
        self.pending_paste_results.push_back(result);
    }

    /// Dequeue the next pending paste result.
    pub fn dequeue_paste_result(&mut self) -> Option<Result<ClipboardPasteResult, String>> {
        self.pending_paste_results.pop_front()
    }

    /// Check if any attachment jobs are still pending.
    pub fn has_pending_attachments(&self) -> bool {
        self.pending_attachment_jobs > 0 || !self.pending_paste_results.is_empty()
    }

    /// Handle a completed paste result (clipboard or bracketed paste).
    pub async fn handle_paste_result(
        &mut self,
        result: Result<ClipboardPasteResult, String>,
    ) -> anyhow::Result<()> {
        let result = match result {
            Ok(result) => result,
            Err(e) => {
                self.status = Some(format!("Clipboard paste failed: {}", e));
                return Ok(());
            }
        };

        if result.is_empty() {
            self.status = Some("Clipboard is empty".to_string());
            return Ok(());
        }

        let (image, text) = result.into_parts();
        if let Some(image) = image {
            let manager = self
                .image_manager
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("Image manager not initialized"))?;
            let path = manager.save(&image).await?;
            self.attachments.push(Attachment::Image {
                path,
                data: image.data,
            });
            self.status = Some("Image attached".to_string());
            return Ok(());
        }

        if let Some(text) = text {
            let saved_path = if text.len() > LARGE_PASTE_THRESHOLD {
                let manager = self
                    .text_manager
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("Text manager not initialized"))?;
                Some(manager.save(&text).await?)
            } else {
                None
            };

            self.attachments.push(Attachment::PastedText {
                content: text,
                saved_path,
            });
            self.status = Some("Text attached".to_string());
            return Ok(());
        }

        Err(anyhow::anyhow!(
            "Clipboard paste result contained no image or text"
        ))
    }

    /// Handle fetched models from async task.
    pub fn handle_fetched_models(&mut self, mut models: Vec<FetchedModel>) {
        // Sort by tier relevance:
        // 1. Models matching the current tier (from aither_models database)
        // 2. Models in the database but not matching this tier
        // 3. Unknown models (not in database)
        let current_tier = self.tier_picker_tier;
        let aither_tier = current_tier.map(|t| match t {
            ModelTier::Advanced => aither_models::ModelTier::Flagship,
            ModelTier::Balanced => aither_models::ModelTier::Balanced,
            ModelTier::Fast => aither_models::ModelTier::Fast,
            ModelTier::ImageGeneration => aither_models::ModelTier::Flagship,
        });

        models.sort_by(|a, b| {
            let a_info = aither_models::lookup(&a.id);
            let b_info = aither_models::lookup(&b.id);

            // Check tier match
            let a_tier_match = aither_tier
                .and_then(|tier| a_info.map(|info| info.has_tier(tier)))
                .unwrap_or(false);
            let b_tier_match = aither_tier
                .and_then(|tier| b_info.map(|info| info.has_tier(tier)))
                .unwrap_or(false);

            // Priority: tier match > in database > unknown
            match (a_tier_match, b_tier_match) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => {
                    match (a_info.is_some(), b_info.is_some()) {
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => a.id.cmp(&b.id),
                    }
                }
            }
        });

        // Dedupe by model ID (keep first occurrence which has correct provider)
        let mut seen = std::collections::HashSet::new();
        models.retain(|m| seen.insert(m.id.clone()));

        self.tier_picker_models = models;
        self.update_tier_picker_filter();
        self.tier_picker_loading = false;
    }

    /// Handle key in unified config mode.
    fn handle_config_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> anyhow::Result<Option<SendAction>> {
        let state = &mut self.config_state;

        // If in edit mode, handle text input
        if state.is_editing() {
            return self.handle_config_edit_key(code);
        }

        // Tab/Shift+Tab to switch sections
        if code == KeyCode::Tab {
            if modifiers.contains(KeyModifiers::SHIFT) {
                state.section = state.section.prev();
            } else {
                state.section = state.section.next();
            }
            state.selected = 0;
            return Ok(None);
        }

        // Navigation within section
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                state.selected = state.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let row_count = state.row_count(&self.config);
                if state.selected + 1 < row_count {
                    state.selected += 1;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                return self.handle_config_enter();
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                // Delete provider (only in Providers section)
                if state.section == ConfigSection::Providers {
                    if let Some(id) = state.provider_ids.get(state.selected).cloned() {
                        self.config.provider.remove(&id);
                        state.load_providers(&self.config);
                        if state.selected >= state.provider_ids.len() && state.selected > 0 {
                            state.selected -= 1;
                        }
                        if let Err(e) = self.config.save() {
                            tracing::error!("Failed to save config: {}", e);
                        }
                    }
                }
            }
            KeyCode::Esc => {
                if !self.needs_setup {
                    self.mode = AppMode::Chat;
                } else if self.config.is_configured() {
                    // Setup is done - at least one provider configured
                    self.needs_setup = false;
                    self.mode = AppMode::Chat;
                    return Ok(Some(SendAction::NewSession));
                }
            }
            _ => {}
        }

        Ok(None)
    }

    /// Handle Enter in config screen - edit/toggle the selected item.
    fn handle_config_enter(&mut self) -> anyhow::Result<Option<SendAction>> {
        let state = &mut self.config_state;

        match state.section {
            ConfigSection::Providers => {
                // Last row is "Add provider..."
                if state.selected >= state.provider_ids.len() {
                    // Open provider type selector
                    let items: Vec<String> = Provider::ALL
                        .iter()
                        .map(|p| p.display_name().to_string())
                        .collect();
                    state.edit_mode = ConfigEditMode::List { items, selected: 0 };
                } else if let Some(id) = state.provider_ids.get(state.selected).cloned() {
                    // For Copilot, start OAuth flow
                    if id == "copilot" {
                        return Ok(Some(SendAction::StartCopilotAuth));
                    }
                    // Check if this is a custom provider (has base_url)
                    let instance = self.config.provider.get(&id);
                    if let Some(inst) = instance {
                        if inst.base_url.is_some() {
                            // Custom provider - open full edit form
                            state.edit_mode = ConfigEditMode::CustomProvider {
                                focused_field: 0,
                                name: id.clone(),
                                base_url: inst.base_url.clone().unwrap_or_default(),
                                api_key: inst.api_key.clone().unwrap_or_default(),
                                cursor: id.chars().count(),
                                editing_existing: true,
                                original_name: Some(id),
                            };
                            return Ok(None);
                        }
                    }
                    // Regular provider - just edit API key
                    let current_key = self.config.provider
                        .get(&id)
                        .and_then(|p| p.api_key.clone())
                        .unwrap_or_default();
                    state.start_text_edit(&current_key);
                }
            }
            ConfigSection::Models => {
                match state.selected {
                    0 => { self.model_fetch_rx = self.open_tier_model_picker(ModelTier::Advanced); }
                    1 => { self.model_fetch_rx = self.open_tier_model_picker(ModelTier::Balanced); }
                    2 => { self.model_fetch_rx = self.open_tier_model_picker(ModelTier::Fast); }
                    3 => { self.model_fetch_rx = self.open_tier_model_picker(ModelTier::ImageGeneration); }
                    _ => {}
                }
            }
            ConfigSection::Preferences => {
                match state.selected {
                    0 => {
                        // Edit max_iterations
                        state.start_text_edit(&self.config.max_iterations.to_string());
                    }
                    1 => {
                        // Toggle load balance strategy
                        use crate::config::LoadBalanceStrategy;
                        self.config.preferences.load_balance_strategy = match self.config.preferences.load_balance_strategy {
                            LoadBalanceStrategy::RoundRobin => LoadBalanceStrategy::FailoverOnly,
                            LoadBalanceStrategy::FailoverOnly => LoadBalanceStrategy::Random,
                            LoadBalanceStrategy::Random => LoadBalanceStrategy::RoundRobin,
                        };
                        if let Err(e) = self.config.save() {
                            tracing::error!("Failed to save config: {}", e);
                        }
                    }
                    2 => {
                        // Toggle warn_on_model_change
                        self.config.preferences.warn_on_model_change = !self.config.preferences.warn_on_model_change;
                        if let Err(e) = self.config.save() {
                            tracing::error!("Failed to save config: {}", e);
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(None)
    }

    /// Handle key input when editing in config screen.
    fn handle_config_edit_key(&mut self, code: KeyCode) -> anyhow::Result<Option<SendAction>> {
        let state = &mut self.config_state;

        // Handle list selection mode
        if let ConfigEditMode::List { items, selected } = &mut state.edit_mode {
            match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if *selected + 1 < items.len() {
                        *selected += 1;
                    }
                }
                KeyCode::Enter => {
                    // Add provider of selected type
                    let idx = *selected;
                    if let Some(provider_type) = Provider::ALL.get(idx) {
                        // For Copilot, start OAuth
                        if *provider_type == Provider::Copilot {
                            state.edit_mode = ConfigEditMode::None;
                            return Ok(Some(SendAction::StartCopilotAuth));
                        }
                        // For Custom provider, need name + base_url + api_key
                        if *provider_type == Provider::Custom {
                            state.edit_mode = ConfigEditMode::CustomProvider {
                                focused_field: 0,
                                name: String::new(),
                                base_url: String::new(),
                                api_key: String::new(),
                                cursor: 0,
                                editing_existing: false,
                                original_name: None,
                            };
                            return Ok(None);
                        }
                        state.edit_mode = ConfigEditMode::None;
                        // For others, try auto-detect API key
                        let provider_id = provider_type.short_name().to_string();
                        if let Some((key, var_name)) = detect_api_key_from_env(*provider_type) {
                            // Auto-detected - create provider
                            let mut instance = crate::config::ProviderInstance::new(*provider_type);
                            instance.api_key = Some(key);
                            self.config.upsert_provider(&provider_id, instance);
                            state.load_providers(&self.config);
                            state.key_from_env = true;
                            state.key_env_var_name = Some(var_name.to_string());
                            self.auto_populate_models_if_empty();
                            if let Err(e) = self.config.save() {
                                tracing::error!("Failed to save config: {}", e);
                            }
                        } else {
                            // Need manual API key entry
                            let instance = crate::config::ProviderInstance::new(*provider_type);
                            self.config.upsert_provider(&provider_id, instance);
                            state.load_providers(&self.config);
                            // Find the new provider in the list and select it
                            if let Some(pos) = state.provider_ids.iter().position(|id| id == &provider_id) {
                                state.selected = pos;
                            }
                            // Start editing API key
                            state.start_text_edit("");
                        }
                    }
                }
                KeyCode::Esc => {
                    state.edit_mode = ConfigEditMode::None;
                }
                _ => {}
            }
            return Ok(None);
        }

        // Handle custom provider edit mode
        if let ConfigEditMode::CustomProvider {
            focused_field,
            name,
            base_url,
            api_key,
            cursor,
            editing_existing,
            original_name,
        } = &mut state.edit_mode
        {
            // Helper macro to get focused input
            macro_rules! focused_input {
                ($field:expr, $name:expr, $base_url:expr, $api_key:expr) => {
                    match $field {
                        0 => $name,
                        1 => $base_url,
                        _ => $api_key,
                    }
                };
            }
            match code {
                KeyCode::Tab | KeyCode::Down => {
                    // Move to next field
                    *focused_field = (*focused_field + 1).min(2);
                    let new_input = focused_input!(*focused_field, name, base_url, api_key);
                    *cursor = new_input.chars().count();
                }
                KeyCode::BackTab | KeyCode::Up => {
                    // Move to previous field
                    *focused_field = focused_field.saturating_sub(1);
                    let new_input = focused_input!(*focused_field, name, base_url, api_key);
                    *cursor = new_input.chars().count();
                }
                KeyCode::Backspace => {
                    if *cursor > 0 {
                        *cursor -= 1;
                        let input = focused_input!(*focused_field, name, base_url, api_key);
                        let byte_idx = char_to_byte_index(input, *cursor);
                        input.remove(byte_idx);
                    }
                }
                KeyCode::Delete => {
                    let input = focused_input!(*focused_field, name, base_url, api_key);
                    let char_count = input.chars().count();
                    if *cursor < char_count {
                        let byte_idx = char_to_byte_index(input, *cursor);
                        input.remove(byte_idx);
                    }
                }
                KeyCode::Left => {
                    *cursor = cursor.saturating_sub(1);
                }
                KeyCode::Right => {
                    let input = focused_input!(*focused_field, name, base_url, api_key);
                    let char_count = input.chars().count();
                    *cursor = (*cursor + 1).min(char_count);
                }
                KeyCode::Home => {
                    *cursor = 0;
                }
                KeyCode::End => {
                    let input = focused_input!(*focused_field, name, base_url, api_key);
                    *cursor = input.chars().count();
                }
                KeyCode::Char(c) => {
                    let input = focused_input!(*focused_field, name, base_url, api_key);
                    let byte_idx = char_to_byte_index(input, *cursor);
                    input.insert(byte_idx, c);
                    *cursor += 1;
                }
                KeyCode::Enter => {
                    // Validate and save
                    if name.is_empty() {
                        // Name is required, flash error or just don't save
                        return Ok(None);
                    }
                    if base_url.is_empty() {
                        // Base URL is required for custom provider
                        return Ok(None);
                    }
                    // Clone values before moving out of the borrowed state
                    let name_val = name.clone();
                    let base_url_val = base_url.clone();
                    let api_key_val = if api_key.is_empty() { None } else { Some(api_key.clone()) };
                    let was_editing = *editing_existing;
                    let orig_name = original_name.clone();

                    // Reset edit mode
                    self.config_state.edit_mode = ConfigEditMode::None;

                    // If editing existing and name changed, remove old entry
                    if was_editing {
                        if let Some(ref old_name) = orig_name {
                            if old_name != &name_val {
                                self.config.provider.remove(old_name);
                            }
                        }
                    }

                    // Create new provider instance
                    let mut instance = crate::config::ProviderInstance::new(Provider::Custom);
                    instance.base_url = Some(base_url_val);
                    instance.api_key = api_key_val;
                    self.config.upsert_provider(&name_val, instance);
                    self.config_state.load_providers(&self.config);
                    self.auto_populate_models_if_empty();
                    if let Err(e) = self.config.save() {
                        tracing::error!("Failed to save config: {}", e);
                    }
                    return Ok(None);
                }
                KeyCode::Esc => {
                    self.config_state.edit_mode = ConfigEditMode::None;
                }
                _ => {}
            }
            return Ok(None);
        }

        // Handle text edit mode
        if let Some((input, cursor)) = state.edit_text_mut() {
            match code {
                KeyCode::Backspace => {
                    if *cursor > 0 {
                        *cursor -= 1;
                        let byte_idx = char_to_byte_index(input, *cursor);
                        input.remove(byte_idx);
                    }
                }
                KeyCode::Delete => {
                    let char_count = input.chars().count();
                    if *cursor < char_count {
                        let byte_idx = char_to_byte_index(input, *cursor);
                        input.remove(byte_idx);
                    }
                }
                KeyCode::Left => {
                    *cursor = cursor.saturating_sub(1);
                }
                KeyCode::Right => {
                    let char_count = input.chars().count();
                    *cursor = (*cursor + 1).min(char_count);
                }
                KeyCode::Home => {
                    *cursor = 0;
                }
                KeyCode::End => {
                    *cursor = input.chars().count();
                }
                KeyCode::Char(c) => {
                    let byte_idx = char_to_byte_index(input, *cursor);
                    input.insert(byte_idx, c);
                    *cursor += 1;
                }
                KeyCode::Enter => {
                    // Save the edited value
                    return self.save_config_edit();
                }
                KeyCode::Esc => {
                    state.cancel_edit();
                }
                _ => {}
            }
        }

        Ok(None)
    }

    /// Save the current edit in config screen.
    fn save_config_edit(&mut self) -> anyhow::Result<Option<SendAction>> {
        // Extract needed values from state first to avoid borrow issues
        let (value, section, selected) = {
            let state = &self.config_state;
            let value = match &state.edit_mode {
                ConfigEditMode::Text { input, .. } => input.clone(),
                _ => return Ok(None),
            };
            (value, state.section, state.selected)
        };

        match section {
            ConfigSection::Providers => {
                // Save API key for selected provider
                let id = self.config_state.provider_ids.get(selected).cloned();
                if let Some(id) = id {
                    if let Some(instance) = self.config.provider.get_mut(&id) {
                        instance.api_key = if value.is_empty() { None } else { Some(value) };
                    }
                    self.auto_populate_models_if_empty();
                }
            }
            ConfigSection::Models => {
                // Parse comma-separated model list
                let models: Vec<String> = value
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                match selected {
                    0 => self.config.models.advanced = models,
                    1 => self.config.models.balanced = models,
                    2 => self.config.models.fast = models,
                    3 => self.config.models.image_generation = models,
                    _ => {}
                }
            }
            ConfigSection::Preferences => {
                if selected == 0 {
                    // Parse max_iterations
                    if let Ok(val) = value.parse::<usize>() {
                        self.config.max_iterations = val;
                    }
                }
            }
        }

        self.config_state.cancel_edit();
        if let Err(e) = self.config.save() {
            tracing::error!("Failed to save config: {}", e);
        }

        Ok(None)
    }

    /// Auto-populate models from the first configured provider if models are empty.
    fn auto_populate_models_if_empty(&mut self) {
        if self.config.models.has_any_models() {
            return;
        }

        // Find first valid provider
        for (id, instance) in &self.config.provider {
            if !instance.is_valid(id) {
                continue;
            }
            let provider_type = instance.resolve_type(id);
            let provider_str = provider_type.aither_provider();

            if provider_str == "unknown" {
                // Custom/OpenRouter - use preset models
                self.config.models.advanced = vec!["claude-opus-4".to_string()];
                self.config.models.balanced = vec!["claude-sonnet-4".to_string()];
                self.config.models.fast = vec!["claude-haiku-4".to_string()];
                return;
            }

            // Get models from aither-models
            use aither_models::ModelTier as AitherTier;
            for info in aither_models::models_for_provider(provider_str) {
                if info.outdated {
                    continue;
                }
                let model_id = info.id.to_string();
                if info.has_tier(AitherTier::Flagship) && self.config.models.advanced.is_empty() {
                    self.config.models.advanced.push(model_id.clone());
                }
                if info.has_tier(AitherTier::Balanced) && self.config.models.balanced.is_empty() {
                    self.config.models.balanced.push(model_id.clone());
                }
                if info.has_tier(AitherTier::Fast) && self.config.models.fast.is_empty() {
                    self.config.models.fast.push(model_id);
                }
            }
            return;
        }
    }

    /// Handle key in model select popup.
    fn handle_model_select_key(&mut self, code: KeyCode) -> anyhow::Result<Option<SendAction>> {
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.model_select_index = self.model_select_index.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.model_select_index + 1 < self.model_select_list.len() {
                    self.model_select_index += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(model) = self.model_select_list.get(self.model_select_index).cloned() {
                    self.mode = AppMode::Chat;
                    return Ok(Some(SendAction::SwitchModel(model)));
                }
            }
            KeyCode::Esc => {
                self.mode = AppMode::Chat;
            }
            _ => {}
        }
        Ok(None)
    }

    /// Collect all unique models from config for model select.
    fn collect_all_models(&self) -> Vec<String> {
        let mut models = Vec::new();
        for m in &self.config.models.advanced {
            if !models.contains(m) {
                models.push(m.clone());
            }
        }
        for m in &self.config.models.balanced {
            if !models.contains(m) {
                models.push(m.clone());
            }
        }
        for m in &self.config.models.fast {
            if !models.contains(m) {
                models.push(m.clone());
            }
        }
        models
    }

    /// Handle paste (Ctrl+V) - check for image or text in clipboard.
    fn handle_paste(&mut self) -> anyhow::Result<()> {
        if self.clipboard_rx.is_some() {
            self.status = Some("Clipboard paste already in progress".to_string());
            return Ok(());
        }

        let (tx, rx) = async_channel::bounded(1);
        self.clipboard_rx = Some(rx);
        self.pending_attachment_jobs = self.pending_attachment_jobs.saturating_add(1);
        self.status = Some("Reading clipboard...".to_string());

        spawn(async move {
            let result = match unblock(|| read_clipboard()).await {
                Ok(paste) => {
                    let (image, text) = paste.into_parts();
                    if let Some(image) = image {
                        Ok(ClipboardPasteResult::image(image))
                    } else if let Some(text) = text {
                        if let Some(url) = is_image_url(&text) {
                            download_image(&url)
                                .await
                                .map(ClipboardPasteResult::image)
                                .map_err(|e| e.to_string())
                        } else {
                            Ok(ClipboardPasteResult::text(text))
                        }
                    } else {
                        Ok(ClipboardPasteResult::empty())
                    }
                }
                Err(e) => Err(e.to_string()),
            };

            let _ = tx.send(result).await;
        })
        .detach();

        Ok(())
    }

    /// Prepare a message for sending, returning the action.
    fn prepare_message(&mut self) -> SendAction {
        // Add to history before clearing (only if non-empty and not just whitespace)
        if !self.input.trim().is_empty() {
            self.history.push(self.input.clone());
            self.history.save();
        }

        let content = std::mem::take(&mut self.input);
        let attachments = std::mem::take(&mut self.attachments);
        self.cursor = 0;
        self.input_char_count = 0;

        let msg = PendingMessage {
            content,
            attachments,
            queued_at: jiff::Timestamp::now(),
        };

        if self.processing {
            self.queue.enqueue(msg);
            SendAction::Queued
        } else {
            SendAction::Send(msg)
        }
    }

    /// Called when starting to process a message.
    pub fn start_processing(&mut self, msg: &PendingMessage) {
        self.processing = true;
        self.thinking_start = None;

        // Add user message to chat
        self.chat.push_user(&msg.content, msg.attachments.clone());

        // Add to session history
        if let Some(session) = self.session_manager.current_mut() {
            session.history.push(ChatMessage::user(&msg.content));
        }

        // Scroll to bottom
        self.scroll = usize::MAX;
        self.input_visible = true;

        // Show loading indicator (will be replaced when first response arrives)
        self.chat.start_loading();
    }

    /// Handle an agent event. Returns true if stream ended.
    pub fn handle_agent_event(&mut self, result: Result<AgentEvent, AgentError>) -> bool {
        match result {
            Ok(event) => {
                match event {
                    AgentEvent::Text(chunk) => {
                        // End loading/thinking state and start assistant text
                        self.chat.end_loading();
                        self.thinking_start = None;
                        self.chat.push_text(&chunk);
                        self.scroll = usize::MAX;
                        self.input_visible = true;
                    }
                    AgentEvent::Reasoning(_) => {
                        // Start or update thinking indicator
                        if self.thinking_start.is_none() {
                            self.thinking_start = Some(std::time::Instant::now());
                            self.chat.start_thinking();
                        }
                        // Update thinking time
                        if let Some(start) = self.thinking_start {
                            let secs = start.elapsed().as_secs();
                            self.chat.update_thinking(secs);
                        }
                    }
                    AgentEvent::ToolCallStart { id, arguments, .. } => {
                        self.chat.end_loading();
                        self.chat.end_text();
                        let display = parse_tool_display(&arguments);
                        self.chat.push_tool(id, display);
                    }
                    AgentEvent::ToolCallEnd { id, result, .. } => {
                        let feedback = parse_tool_result(&result);
                        self.chat.update_tool_with_feedback(&id, feedback);
                    }
                    AgentEvent::Complete { .. } => {
                        // Text was already streamed via AgentEvent::Text events
                        // Just finalize the message
                        self.chat.end_loading();
                        self.chat.end_text();
                        self.scroll = usize::MAX;
                        self.input_visible = true;
                        // Signal that processing is complete
                        return true;
                    }
                    AgentEvent::Usage(u) => {
                        // Accumulate token usage
                        if let Some(ref mut usage) = self.usage {
                            usage.accumulate(&u);
                        } else {
                            self.usage = Some(u);
                        }
                        // Usage typically comes at the end - finalize and signal completion
                        self.chat.end_loading();
                        self.chat.end_text();
                        self.scroll = usize::MAX;
                        self.input_visible = true;
                        return true;
                    }
                    _ => {}
                }
                false
            }
            Err(e) => {
                tracing::error!("Agent error: {}", e);
                // Display error to user
                self.chat.end_loading();
                self.chat.push_error(&format!("{}", e));
                self.scroll = usize::MAX;
                self.input_visible = true;
                true
            }
        }
    }

    /// Called when agent stream ends.
    pub fn finish_processing(&mut self) {
        self.processing = false;

        // Save assistant response to history
        let assistant_content = self
            .chat
            .elements()
            .iter()
            .rev()
            .find_map(|e| {
                if let ChatElement::AssistantText { content, .. } = e {
                    Some(content.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        if let Some(session) = self.session_manager.current_mut() {
            session.history.push(ChatMessage::assistant(&assistant_content));
        }

        // Note: History is saved asynchronously in src/bin/may.rs after finish_processing returns
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
            .map(|s| s.id.to_string())
            .unwrap_or_else(|| "none".to_string())
    }

    /// Get model name for display.
    pub fn model_display(&self) -> String {
        // Show the actual model being used - no fallback
        self.current_model
            .clone()
            .unwrap_or_else(|| "No model".to_string())
    }

    /// Set the current model being used.
    pub fn set_current_model(&mut self, model: &str) {
        self.current_model = Some(model.to_string());
        // Also update context window from model info
        if let Some(info) = aither_models::lookup(model) {
            self.context_window = Some(info.context_window);
        }
    }

    /// Get context usage information for display.
    /// Returns (prompt_tokens, context_window) if available.
    pub fn context_usage(&self) -> Option<(u32, u32)> {
        let usage = self.usage.as_ref()?;
        let prompt_tokens = usage.prompt_tokens?;
        let context_window = self.context_window?;
        Some((prompt_tokens, context_window))
    }

    /// Update completion state based on current input.
    fn update_completion_state(&mut self) {
        // Check for slash command at start
        if self.input.starts_with('/') {
            let query = &self.input[1..]; // Skip the /
            if self.completion.mode == CompletionMode::Slash {
                self.completion.update_query(query);
            } else {
                self.completion.start_slash(query);
            }
            return;
        }

        // Check for @ file mention - find the last @ that starts a mention
        if let Some(at_char_pos) = self.find_active_mention() {
            // Convert char index to byte index for safe slicing
            let at_byte_pos = char_to_byte_index(&self.input, at_char_pos);
            let query = &self.input[at_byte_pos + 1..]; // +1 is safe since @ is ASCII
            if self.completion.mode == CompletionMode::File {
                self.completion.update_query(query);
            } else {
                // Set up directories for file search
                let workdir = std::env::current_dir().unwrap_or_default();
                let sandbox = self
                    .session_manager
                    .current()
                    .map(|s| s.sandbox_dir.clone())
                    .unwrap_or_else(|| workdir.clone());
                self.completion.set_directories(workdir, sandbox);
                self.completion.start_file(query);
            }
            return;
        }

        // No completion trigger found
        if self.completion.is_active() {
            self.completion.cancel();
        }
    }

    /// Find the position of an active @ mention (one that's being typed).
    fn find_active_mention(&self) -> Option<usize> {
        // Look for @ that's not followed by whitespace and is at cursor or before it
        let input_bytes: Vec<char> = self.input.chars().collect();
        let mut last_at = None;

        for (i, &c) in input_bytes.iter().enumerate() {
            if c == '@' {
                // Check if this @ is the start of a mention (not in middle of word)
                let prev_is_space = i == 0 || input_bytes[i - 1].is_whitespace();
                if prev_is_space {
                    last_at = Some(i);
                }
            } else if c.is_whitespace() && last_at.is_some() {
                // Space ends the current mention
                last_at = None;
            }
        }

        // Only return if cursor is within the mention
        if let Some(pos) = last_at {
            if self.cursor > pos {
                return Some(pos);
            }
        }
        None
    }

    /// Accept a completion and insert it into the input.
    fn accept_completion(&mut self, insert: &str) {
        match self.completion.mode {
            CompletionMode::Slash => {
                // Replace the entire input with the command
                self.input = insert.to_string();
                self.input_char_count = self.input.chars().count();
                self.cursor = self.input_char_count;
            }
            CompletionMode::File => {
                // Replace from @ to cursor with the file path
                if let Some(at_pos) = self.find_active_mention() {
                    // Build new input: before @ + @path + space
                    let before: String = self.input.chars().take(at_pos).collect();
                    let after: String = self.input.chars().skip(self.cursor).collect();
                    self.input = format!("{}@{} {}", before, insert, after);
                    self.input_char_count = self.input.chars().count();
                    // Position cursor after the inserted path + space
                    self.cursor = at_pos + 1 + insert.chars().count() + 1;
                }
            }
            CompletionMode::None => {}
        }
        self.completion.cancel();
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
    /// Message was queued (agent is busy).
    Queued,
    /// Send this message to the agent.
    Send(PendingMessage),
    /// Start GitHub Copilot OAuth authentication.
    StartCopilotAuth,
    /// Open Copilot auth URL in browser and copy code to clipboard.
    OpenCopilotAuthUrl { url: String, code: String },
    /// Switch to a different model for this session.
    SwitchModel(String),
    /// Compact the conversation history.
    Compact,
    /// MCP server management actions.
    McpAdd { name: String, command: String, args: Vec<String> },
    McpRemove { name: String },
    McpReconnect { name: String },
    /// Skill management actions.
    SkillInstall { repo: String },
    SkillDelete { dir_name: String },
}
