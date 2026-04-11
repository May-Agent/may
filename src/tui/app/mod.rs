//! Application state and logic.

mod completion;
mod config;
mod model_picker;

use std::cell::Cell;
use std::path::PathBuf;

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
use super::history::InputHistory;
use super::queue::{Attachment, LARGE_PASTE_THRESHOLD, MessageQueue, PendingMessage};
use super::ui::chat::{ChatStream, ToolDisplay, clamp_column_by_width};
use super::ui::file_picker::FilePicker;

pub const ERR_TOOL_RESPONSE_SEND: &str =
    include_str!("../../texts/tool_response_send_failed.txt");

/// Convert a character index to a byte index in a string.
pub(super) fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(i, _)| i)
}

/// Agent event from aither-agent.
pub use aither_agent::{AgentError, AgentEvent};

use super::state::{
    ActiveTextInput, AppMode, ConfigEditMode, ConfigSection, ConfigState, CopilotAuthStatus,
    McpAddMode, PasteFilter, SelectionPoint, SkillAddMode, SshAddMode, TextSelection,
    detect_api_key_from_env,
};
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
    pub content_height: Cell<usize>,
    /// View height (set during rendering for scroll normalization)
    pub view_height: Cell<usize>,
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
    /// When thinking started (for thinking models)
    pub thinking_start: Option<std::time::Instant>,
    /// Copilot OAuth authentication status
    pub copilot_auth_status: Option<CopilotAuthStatus>,
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
    /// Receiver for clipboard paste resolution (oneshot)
    pub clipboard_rx: Option<Receiver<Result<ClipboardPasteResult, TuiError>>>,
    /// Pending paste results waiting to be handled
    pub pending_paste_results: std::collections::VecDeque<Result<ClipboardPasteResult, TuiError>>,
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
    /// Mouse/text selection in rendered chat content
    pub selection: Option<TextSelection>,
    /// Plain text render cache used for selection copy
    pub rendered_plain_lines: Vec<String>,
    /// Top padding rows added when viewport is taller than rendered content
    pub render_top_padding: usize,
}

impl App {
    /// Create a new application.
    /// `image_support` should be detected before entering raw mode using `TerminalImageSupport::detect()`.
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
            Some(format!("Auto-configured from env: {}", names.join(", ")))
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
            selection: None,
            rendered_plain_lines: Vec::new(),
            render_top_padding: 0,
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
            let data = super::ui::image::fetch_image(&url_owned).await;
            if let Some(ref bytes) = data {
                tracing::debug!(
                    "Image fetch succeeded: {} ({} bytes)",
                    url_owned,
                    bytes.len()
                );
            } else {
                tracing::debug!("Image fetch returned None: {}", url_owned);
            }

            if let Err(e) = tx
                .send(AppEvent::ImageFetchResult {
                    url: url_owned,
                    data,
                })
                .await
            {
                tracing::warn!("Failed to send ImageFetchResult event: {}", e);
            }
        })
        .detach();

        false
    }

    /// Handle image fetch completion event.
    pub(super) fn handle_image_fetch_result(&mut self, url: String, data: Option<Vec<u8>>) {
        self.image_fetching.remove(&url);

        let Some(bytes) = data else {
            tracing::debug!("Image fetch failed for URL: {}", url);
            return;
        };

        tracing::debug!("Image fetched: url={}, {} bytes", url, bytes.len());
        let Some(ref support) = self.image_support else {
            tracing::debug!("No image support, not caching: {}", url);
            return;
        };

        if let Some(state) = support.create_image_state_from_bytes(&bytes) {
            tracing::debug!("Image cached successfully: {}", url);
            self.image_cache.insert(url, state);
        } else {
            tracing::debug!("Failed to create image state from bytes: {}", url);
        }
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
    }

    /// Initialize for a session (sets up image manager and status).
    ///
    /// Takes the session ID and a slice of messages to avoid borrow conflicts
    /// with the session manager.
    pub fn init_session(
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
        self.background_tasks = 0;
        self.selected_tool = None;
        self.selected_tool_index = 0;
        self.tool_detail_scroll = 0;
        self.quit_pending = false;
        self.thinking_start = None;
        self.content_height.set(0);
        self.view_height.set(0);
        self.selection = None;
        self.rendered_plain_lines.clear();
        self.render_top_padding = 0;

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

        let session_dir = self
            .session_manager
            .current()
            .map(|session| session.sandbox_dir.clone())
            .unwrap_or_else(|| {
                panic!(
                    "current session missing while initializing session {}",
                    session_id.as_str()
                )
            });
        self.image_manager = Some(ImageManager::new(&session_dir));
        self.text_manager = Some(TextManager::new(&session_dir));

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
    pub fn handle_event(&mut self, event: AppEvent) -> AppResult<Option<SendAction>> {
        super::reducer::reduce_event(self, event)
    }

    /// Get the currently active text input field, if any.
    const fn active_text_input(&mut self) -> Option<ActiveTextInput<'_>> {
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
            AppMode::FilePicker => {
                // FilePicker is handled separately (has its own insert_str method)
            }
            _ => {}
        }
        None
    }

    /// Handle bracketed paste event.
    pub(super) fn handle_bracketed_paste(&mut self, text: String) -> AppResult<Option<SendAction>> {
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
                        self.status = Some("Copied selection".to_string());
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
                    self.status = Some("^C Press Ctrl+C again to exit".to_string());
                    return Ok(None);
                }
                KeyCode::Char('q') => return Ok(Some(SendAction::Quit)),
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
            AppMode::Chat => self.handle_chat_key(code, modifiers),
            AppMode::SessionSelect => {
                super::reducer::navigation::handle_session_select_key(self, code)
            }
            AppMode::FilePicker => super::reducer::navigation::handle_file_picker_key(self, code),
            AppMode::Help => super::reducer::navigation::handle_help_key(self, code),
            AppMode::ToolDetail => super::reducer::navigation::handle_tool_detail_key(self, code),
            AppMode::Config => self.handle_config_key(code, modifiers),
            AppMode::ModelSelect => self.handle_model_select_key(code),
            AppMode::TierModelPicker => self.handle_tier_model_picker_key(code),
            AppMode::CopilotAuth => super::reducer::manage::handle_copilot_auth_key(self, code),
            AppMode::PermissionConfirm(_) => {
                super::reducer::manage::handle_permission_confirm_key(self, code)
            }
            AppMode::WorkspaceConfirm(_) => {
                super::reducer::manage::handle_workspace_confirm_key(self, code)
            }
            AppMode::McpManage => super::reducer::manage::handle_mcp_key(self, code),
            AppMode::SshManage => super::reducer::manage::handle_ssh_key(self, code),
            AppMode::AskUser(_) => super::reducer::ask_user::handle_ask_user_key(self, code),
            AppMode::SkillManage => super::reducer::manage::handle_skill_key(self, code),
            AppMode::ProfileSelect => {
                super::reducer::navigation::handle_profile_select_key(self, code)
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
        self.status = Some("Loading sessions...".to_string());
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
                PinnedItem::Session(s) => list.push(s.clone()),
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

    /// Handle key in chat mode (unified input + navigation).
    fn handle_chat_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> AppResult<Option<SendAction>> {
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
                    return Ok(Some(self.prepare_message(PendingUserMessageMode::Steer)));
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
                } else if let Some(entry) = self.history.next() {
                    self.input = entry.to_string();
                    self.input_char_count = self.input.chars().count();
                    self.cursor = self.input_char_count;
                }
            }
            // PageUp/PageDown: native terminal scroll handles this now
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
                if self.completion.is_active()
                    && let Some(item) = self.completion.complete()
                {
                    self.accept_completion(&item.insert.clone());
                    return Ok(None);
                }
                if self.processing && (!self.input.is_empty() || !self.attachments.is_empty()) {
                    if self.has_pending_attachments() {
                        self.status = Some("Waiting for attachments to finish...".to_string());
                        return Ok(None);
                    }
                    return Ok(Some(self.prepare_message(PendingUserMessageMode::Queue)));
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

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) -> AppResult<Option<SendAction>> {
        if !matches!(self.mode, AppMode::Chat) {
            return Ok(None);
        }

        const SCROLL_STEP_LINES: usize = 1;

        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if self.scroll == usize::MAX {
                    let content_h = self.content_height.get();
                    let view_h = self.view_height.get();
                    let max_scroll = content_h.saturating_sub(view_h);
                    self.scroll = max_scroll.saturating_sub(SCROLL_STEP_LINES);
                } else {
                    self.scroll = self.scroll.saturating_sub(SCROLL_STEP_LINES);
                }
            }
            MouseEventKind::ScrollDown => {
                if self.scroll != usize::MAX {
                    let content_h = self.content_height.get();
                    let view_h = self.view_height.get();
                    let max_scroll = content_h.saturating_sub(view_h);
                    let next = self.scroll.saturating_add(SCROLL_STEP_LINES);
                    self.scroll = if next > max_scroll { usize::MAX } else { next };
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = self.mouse_to_selection_point(mouse).map(|p| TextSelection {
                    anchor: p,
                    cursor: p,
                });
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
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if self.copy_selection_to_clipboard()? {
                    self.status = Some("Copied selection".to_string());
                }
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
            let content_h = self.content_height.get();
            let view_h = self.view_height.get();
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

    fn copy_selection_to_clipboard(&mut self) -> AppResult<bool> {
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
    fn execute_slash_command(&mut self, cmd: &str) -> AppResult<Option<SendAction>> {
        match cmd {
            "/session" => {
                self.load_session_list();
                Ok(None)
            }
            "/new" => Ok(Some(SendAction::NewSession)),
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
                Ok(None)
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
                if let Some(current) = self
                    .current_model
                    .as_deref()
                    .or(self.config.last_model.as_deref())
                    && let Some(idx) = self.model_select_list.iter().position(|m| m == current)
                {
                    self.model_select_index = idx;
                }
                self.mode = AppMode::ModelSelect;
                Ok(None)
            }
            "/quit" => Ok(Some(SendAction::Quit)),
            "/compact" => Ok(Some(SendAction::Compact(None))),
            "/mcp" => {
                self.mode = AppMode::McpManage;
                Ok(None)
            }
            "/ssh" => {
                self.mode = AppMode::SshManage;
                Ok(None)
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
                self.status = Some("Loading skills...".to_string());
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Queue a paste result to be handled by the main loop.
    pub fn enqueue_paste_result(&mut self, result: Result<ClipboardPasteResult, TuiError>) {
        self.pending_paste_results.push_back(result);
    }

    /// Dequeue the next pending paste result.
    pub fn dequeue_paste_result(&mut self) -> Option<Result<ClipboardPasteResult, TuiError>> {
        self.pending_paste_results.pop_front()
    }

    /// Check if any attachment jobs are still pending.
    pub fn has_pending_attachments(&self) -> bool {
        self.pending_attachment_jobs > 0 || !self.pending_paste_results.is_empty()
    }

    /// Handle a completed paste result (clipboard or bracketed paste).
    pub async fn handle_paste_result(
        &mut self,
        result: Result<ClipboardPasteResult, TuiError>,
    ) -> AppResult<()> {
        let result = match result {
            Ok(result) => result,
            Err(e) => {
                self.status = Some(format!("Clipboard paste failed: {e}"));
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
                .ok_or(TuiError::MissingImageManager)?;
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
                    .ok_or(TuiError::MissingTextManager)?;
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

        Err(TuiError::EmptyPasteResult)
    }

    /// Handle paste (Ctrl+V) - check for image or text in clipboard.
    fn handle_paste(&mut self) -> AppResult<()> {
        if self.clipboard_rx.is_some() {
            self.status = Some("Clipboard paste already in progress".to_string());
            return Ok(());
        }

        let (tx, rx) = async_channel::bounded(1);
        self.clipboard_rx = Some(rx);
        self.pending_attachment_jobs = self.pending_attachment_jobs.saturating_add(1);
        self.status = Some("Reading clipboard...".to_string());

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

        Ok(())
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
        if let Some(rest) = trimmed.strip_prefix("/compact")
            && attachments.is_empty()
        {
            let focus = rest.trim();
            if focus.is_empty() {
                return SendAction::Compact(None);
            }
            return SendAction::Compact(Some(focus.to_string()));
        }

        let msg = PendingMessage {
            content,
            hidden_context_xml: None,
            attachments,
            queued_at: jiff::Timestamp::now(),
            delivery_mode,
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

        // Add user message to chat
        self.chat.push_user(&msg.content, msg.attachments.clone());

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
                    AgentEvent::ToolCallStart {
                        id,
                        name,
                        arguments,
                    } => {
                        self.chat.end_loading();
                        self.chat.end_text();
                        let display = match parse_tool_start(&name, &arguments) {
                            ToolStartKind::Terminal { command, mode } => ToolDisplay::Terminal {
                                command,
                                mode,
                                status: super::ui::chat::ToolStatus::Running,
                                output: None,
                            },
                            ToolStartKind::Generic { name, arguments } => ToolDisplay::Terminal {
                                command: format!("{name} {arguments}"),
                                mode: aither_sandbox::TerminalMode::Sandboxed,
                                status: super::ui::chat::ToolStatus::Running,
                                output: None,
                            },
                        };
                        self.chat.push_tool(id, display);
                    }
                    AgentEvent::ToolCallEnd { id, result, .. } => {
                        let feedback = parse_tool_end(&result);
                        match feedback {
                            ToolEndKind::Output(out) => {
                                self.chat.update_tool_with_output(
                                    &id,
                                    super::ui::chat::ToolStatus::Done,
                                    None,
                                    Some(out),
                                );
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
                    }
                    AgentEvent::RunStart { .. }
                    | AgentEvent::TurnComplete { .. }
                    | AgentEvent::Checkpoint { .. } => {
                        // Lifecycle events are currently used for runtime coordination,
                        // not directly rendered in the TUI timeline.
                    }
                    _ => {}
                }
                false
            }
            Err(e) => {
                tracing::error!("Agent error: {}", e);
                // Display error to user
                self.chat.end_loading();
                self.chat.push_error(&format!("{e}"));
                self.scroll = usize::MAX;
                self.input_visible = true;
                true
            }
        }
    }

    /// Called when agent stream ends.
    pub fn finish_processing(&mut self) {
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
        self.current_model
            .clone()
            .unwrap_or_else(|| "No model".to_string())
    }

    /// Set the current model being used.
    pub fn set_current_model(&mut self, model: &str) {
        self.current_model = Some(model.to_string());
        // Also update context window from model info
        if let Some(info) = aither_models::lookup(model) {
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
    /// Submit an ask-user response.
    RespondAskUser {
        request_id: String,
        request: crate::ask_user::AskUserRequest,
        answers: Vec<crate::ask_user::QuestionAnswer>,
    },
    /// Start GitHub Copilot OAuth authentication.
    StartCopilotAuth,
    /// Open Copilot auth URL in browser and copy code to clipboard.
    OpenCopilotAuthUrl {
        url: String,
        code: String,
    },
    /// Switch to a different model for this session.
    SwitchModel(String),
    /// Compact the conversation history with optional focus hint.
    Compact(Option<String>),
    /// MCP server management actions.
    McpAdd {
        name: String,
        command: String,
        args: Vec<String>,
    },
    McpRemove {
        name: String,
    },
    McpReconnect {
        name: String,
    },
    /// SSH server management actions.
    SshAdd {
        name: String,
        target: String,
    },
    SshRemove {
        name: String,
    },
    /// Submit a workspace approval response.
    RespondWorkspace {
        request_id: String,
        request: crate::workspace_request::WorkspaceRequest,
        approved: bool,
    },
    /// Submit a permission approval response.
    RespondPermission {
        request_id: String,
        approved: bool,
    },
    /// Skill management actions.
    SkillInstall {
        repo: String,
    },
    SkillDelete {
        dir_name: String,
    },
}
