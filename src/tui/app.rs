//! Application state and logic.

use std::cell::Cell;
use std::path::PathBuf;

use anyhow::Context;
use async_channel::Receiver;
use blocking::unblock;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use executor_core::spawn;

use crate::config::{ModelTier, ProviderType, get_model_tier};
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
use super::event::{AppEvent, EventSender};
use super::history::InputHistory;
use super::queue::{Attachment, LARGE_PASTE_THRESHOLD, MessageQueue, PendingMessage};
use super::ui::chat::{ChatElement, ChatStream, ToolDisplay, clamp_column_by_width};
use super::ui::file_picker::FilePicker;

pub const ERR_TOOL_RESPONSE_SEND: &str = include_str!("../texts/tool_response_send_failed.txt");

/// Convert a character index to a byte index in a string.
fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map_or(s.len(), |(i, _)| i)
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
    pub clipboard_rx: Option<Receiver<Result<ClipboardPasteResult, anyhow::Error>>>,
    /// Pending paste results waiting to be handled
    pub pending_paste_results:
        std::collections::VecDeque<Result<ClipboardPasteResult, anyhow::Error>>,
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
    ) -> anyhow::Result<Self> {
        let mut config = MayConfig::load().await.context("Failed to load config")?;
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
            let result = config.save().await.map_err(anyhow::Error::from);
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
    pub fn handle_event(&mut self, event: AppEvent) -> anyhow::Result<Option<SendAction>> {
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
    pub(super) fn handle_bracketed_paste(
        &mut self,
        text: String,
    ) -> anyhow::Result<Option<SendAction>> {
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
    ) -> anyhow::Result<Option<SendAction>> {
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
                        result.map_err(anyhow::Error::from),
                    ))
                    .await;
            } else {
                let result = crate::session::list_summaries_from_db(&db).await;
                let _ = tx
                    .send(AppEvent::SessionsLoaded(
                        result.map_err(anyhow::Error::from),
                    ))
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
                    result.map_err(anyhow::Error::from),
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
    ) -> anyhow::Result<Option<SendAction>> {
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

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) -> anyhow::Result<Option<SendAction>> {
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

    fn copy_selection_to_clipboard(&mut self) -> anyhow::Result<bool> {
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
            return Err(anyhow::anyhow!("failed to copy selection to clipboard"));
        }

        Ok(true)
    }

    /// Execute a slash command directly (from menu selection only).
    fn execute_slash_command(&mut self, cmd: &str) -> anyhow::Result<Option<SendAction>> {
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
                        .send(AppEvent::SkillsLoaded(result.map_err(anyhow::Error::from)))
                        .await;
                })
                .detach();
                self.status = Some("Loading skills...".to_string());
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Handle key in tier model picker mode.
    fn handle_tier_model_picker_key(
        &mut self,
        code: KeyCode,
    ) -> anyhow::Result<Option<SendAction>> {
        match code {
            KeyCode::Esc => {
                // Cancel and go back to config
                self.mode = AppMode::Config;
                self.tier_picker_query.clear();
            }
            KeyCode::Enter => {
                // Select model and add to tier
                if let Some(&idx) = self.tier_picker_filtered.get(self.tier_picker_selected)
                    && let Some(model) = self.tier_picker_models.get(idx)
                    && let Some(tier) = self.tier_picker_tier
                {
                    self.config
                        .models
                        .add_model(tier, &model.id, &model.provider_id);
                    self.persist_config_background();
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
    pub fn open_tier_model_picker(
        &mut self,
        tier: ModelTier,
    ) -> Option<Receiver<Vec<FetchedModel>>> {
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
    pub fn enqueue_paste_result(&mut self, result: Result<ClipboardPasteResult, anyhow::Error>) {
        self.pending_paste_results.push_back(result);
    }

    /// Dequeue the next pending paste result.
    pub fn dequeue_paste_result(&mut self) -> Option<Result<ClipboardPasteResult, anyhow::Error>> {
        self.pending_paste_results.pop_front()
    }

    /// Check if any attachment jobs are still pending.
    pub fn has_pending_attachments(&self) -> bool {
        self.pending_attachment_jobs > 0 || !self.pending_paste_results.is_empty()
    }

    /// Handle a completed paste result (clipboard or bracketed paste).
    pub async fn handle_paste_result(
        &mut self,
        result: Result<ClipboardPasteResult, anyhow::Error>,
    ) -> anyhow::Result<()> {
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

        models.sort_by(|a, b| {
            let a_info = aither_models::lookup(&a.id);
            let b_info = aither_models::lookup(&b.id);

            // Check tier match
            let a_tier_match = current_tier
                .and_then(|tier| a_info.map(|info| get_model_tier(info.id()) == tier))
                .unwrap_or(false);
            let b_tier_match = current_tier
                .and_then(|tier| b_info.map(|info| get_model_tier(info.id()) == tier))
                .unwrap_or(false);

            // Priority: tier match > in database > unknown
            match (a_tier_match, b_tier_match) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => match (a_info.is_some(), b_info.is_some()) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => a.id.cmp(&b.id),
                },
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
                if state.section == ConfigSection::Providers
                    && let Some(id) = state.provider_ids.get(state.selected).cloned()
                {
                    self.config.provider.remove(&id);
                    state.load_providers(&self.config);
                    if state.selected >= state.provider_ids.len() && state.selected > 0 {
                        state.selected -= 1;
                    }
                    self.persist_config_background();
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
                    let items: Vec<String> = ProviderType::ALL
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
                    if let Some(inst) = instance
                        && inst.base_url.is_some()
                    {
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
                    // Regular provider - just edit API key
                    let current_key = self
                        .config
                        .provider
                        .get(&id)
                        .and_then(|p| p.api_key.clone())
                        .unwrap_or_default();
                    state.start_text_edit(&current_key);
                }
            }
            ConfigSection::Models => match state.selected {
                0 => {
                    self.model_fetch_rx = self.open_tier_model_picker(ModelTier::Advanced);
                }
                1 => {
                    self.model_fetch_rx = self.open_tier_model_picker(ModelTier::Balanced);
                }
                2 => {
                    self.model_fetch_rx = self.open_tier_model_picker(ModelTier::Fast);
                }
                3 => {
                    self.model_fetch_rx = self.open_tier_model_picker(ModelTier::ImageGeneration);
                }
                _ => {}
            },
            ConfigSection::Preferences => {
                match state.selected {
                    0 => {
                        // Edit max_iterations
                        state.start_text_edit(&self.config.max_iterations.to_string());
                    }
                    1 => {
                        // Toggle load balance strategy
                        use crate::config::LoadBalanceStrategy;
                        self.config.preferences.load_balance_strategy =
                            match self.config.preferences.load_balance_strategy {
                                LoadBalanceStrategy::RoundRobin => {
                                    LoadBalanceStrategy::FailoverOnly
                                }
                                LoadBalanceStrategy::FailoverOnly => LoadBalanceStrategy::Random,
                                LoadBalanceStrategy::Random => LoadBalanceStrategy::RoundRobin,
                            };
                        self.persist_config_background();
                    }
                    2 => {
                        // Toggle warn_on_model_change
                        self.config.preferences.warn_on_model_change =
                            !self.config.preferences.warn_on_model_change;
                        self.persist_config_background();
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
                    if let Some(provider_type) = ProviderType::ALL.get(idx) {
                        // For Copilot, start OAuth
                        if *provider_type == ProviderType::Copilot {
                            state.edit_mode = ConfigEditMode::None;
                            return Ok(Some(SendAction::StartCopilotAuth));
                        }
                        // For Custom provider, need name + base_url + api_key
                        if *provider_type == ProviderType::Custom {
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
                            self.persist_config_background();
                        } else {
                            // Need manual API key entry
                            let instance = crate::config::ProviderInstance::new(*provider_type);
                            self.config.upsert_provider(&provider_id, instance);
                            state.load_providers(&self.config);
                            // Find the new provider in the list and select it
                            if let Some(pos) =
                                state.provider_ids.iter().position(|id| id == &provider_id)
                            {
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
                    let api_key_val = if api_key.is_empty() {
                        None
                    } else {
                        Some(api_key.clone())
                    };
                    let was_editing = *editing_existing;
                    let orig_name = original_name.clone();

                    // Reset edit mode
                    self.config_state.edit_mode = ConfigEditMode::None;

                    // If editing existing and name changed, remove old entry
                    if was_editing
                        && let Some(ref old_name) = orig_name
                        && old_name != &name_val
                    {
                        self.config.provider.remove(old_name);
                    }

                    // Create new provider instance
                    let mut instance = crate::config::ProviderInstance::new(ProviderType::Custom);
                    instance.base_url = Some(base_url_val);
                    instance.api_key = api_key_val;
                    self.config.upsert_provider(&name_val, instance);
                    self.config_state.load_providers(&self.config);
                    self.auto_populate_models_if_empty();
                    self.persist_config_background();
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
        self.persist_config_background();

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
            for info in aither_models::all_models() {
                if info.is_deprecated() {
                    continue;
                }
                let model_id = info.id().to_string();
                if !provider_type.can_serve_model(&model_id) {
                    continue;
                }
                let tier = get_model_tier(&model_id);
                if tier == ModelTier::Advanced && self.config.models.advanced.is_empty() {
                    self.config.models.advanced.push(model_id.clone());
                }
                if tier == ModelTier::Balanced && self.config.models.balanced.is_empty() {
                    self.config.models.balanced.push(model_id.clone());
                }
                if tier == ModelTier::Fast && self.config.models.fast.is_empty() {
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
            let result = match unblock(read_clipboard).await {
                Ok(paste) => {
                    let (image, text) = paste.into_parts();
                    if let Some(image) = image {
                        Ok(ClipboardPasteResult::image(image))
                    } else if let Some(text) = text {
                        if let Some(url) = is_image_url(&text) {
                            download_image(&url)
                                .await
                                .map(ClipboardPasteResult::image)
                                .map_err(anyhow::Error::from)
                        } else {
                            Ok(ClipboardPasteResult::text(text))
                        }
                    } else {
                        Ok(ClipboardPasteResult::empty())
                    }
                }
                Err(e) => Err(anyhow::Error::from(e)),
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
            session
                .history
                .push(ChatMessage::assistant(&assistant_content));
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
            .current().map_or_else(|| "none".to_string(), |s| s.id.to_string())
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
                    .current().map_or_else(|| workdir.clone(), |s| s.sandbox_dir.clone());
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
        if let Some(pos) = last_at
            && self.cursor > pos
        {
            return Some(pos);
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
                    self.input = format!("{before}@{insert} {after}");
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
