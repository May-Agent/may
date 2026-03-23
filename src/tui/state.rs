//! State types for TUI modes and config editing.

use crate::MayConfig;
use crate::ask_user::{AskUserRequest, Question};
use crate::config::ProviderType;
use crate::workspace_request::WorkspaceRequest;

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
    /// Permission approval dialog for elevated terminal modes.
    PermissionConfirm(PermissionConfirmState),
    /// Workspace access confirmation dialog
    WorkspaceConfirm(WorkspaceConfirmState),
    /// MCP server management overlay
    McpManage,
    /// SSH server management overlay
    SshManage,
    /// Ask user dialog
    AskUser(AskUserState),
    /// Skill management popup
    SkillManage,
    /// Profile switcher popup
    ProfileSelect,
}

/// Per-question interaction state.
pub struct AskUserQuestionState {
    /// Cursor position in the option list (includes extra items: skip, custom).
    pub cursor: usize,
    /// Selected option indices (for the predefined options).
    pub selections: Vec<usize>,
    /// Whether the user is typing a custom answer.
    pub custom_mode: bool,
    /// Custom answer text buffer.
    pub custom_text: String,
    /// Whether this question was skipped.
    pub skipped: bool,
}

impl Default for AskUserQuestionState {
    fn default() -> Self {
        Self::new()
    }
}

impl AskUserQuestionState {
    #[must_use] 
    pub const fn new() -> Self {
        Self {
            cursor: 0,
            selections: Vec::new(),
            custom_mode: false,
            custom_text: String::new(),
            skipped: false,
        }
    }
}

/// State for the `ask_user` interaction.
impl std::fmt::Debug for AskUserState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskUserState")
            .field("question_count", &self.questions.len())
            .field("current_question", &self.current_question)
            .finish()
    }
}

pub struct AskUserState {
    /// Stable persisted request identifier.
    pub request_id: String,
    /// The original request (holds the response channel).
    pub request: AskUserRequest,
    /// Normalized list of questions.
    pub questions: Vec<Question>,
    /// Current question index.
    pub current_question: usize,
    /// Per-question state.
    pub question_states: Vec<AskUserQuestionState>,
}

impl AskUserState {
    #[must_use] 
    pub fn new(request_id: String, request: AskUserRequest, questions: Vec<Question>) -> Self {
        let states = questions
            .iter()
            .map(|_| AskUserQuestionState::new())
            .collect();
        Self {
            request_id,
            request,
            questions,
            current_question: 0,
            question_states: states,
        }
    }

    /// Current question being displayed.
    #[must_use] 
    pub fn current(&self) -> Option<&Question> {
        self.questions.get(self.current_question)
    }

    /// Current question's mutable state.
    pub fn current_state_mut(&mut self) -> Option<&mut AskUserQuestionState> {
        self.question_states.get_mut(self.current_question)
    }

    /// Current question's state.
    #[must_use] 
    pub fn current_state(&self) -> Option<&AskUserQuestionState> {
        self.question_states.get(self.current_question)
    }

    /// Total number of items in the list for current question:
    /// options + "Skip" + "Custom answer"
    #[must_use] 
    pub fn current_item_count(&self) -> usize {
        self.current().map_or(0, |q| q.option.len() + 2)
    }

    /// Whether there are multiple questions.
    #[must_use] 
    pub const fn is_multi(&self) -> bool {
        self.questions.len() > 1
    }
}

#[derive(Debug)]
pub struct WorkspaceConfirmState {
    pub request_id: String,
    pub request: WorkspaceRequest,
}

#[derive(Debug)]
pub struct PermissionConfirmState {
    pub request_id: String,
    pub request: crate::session::PersistedPermissionRequestPayload,
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
    #[must_use] 
    pub fn filter(self, s: &str) -> String {
        match self {
            Self::Any => s.to_string(),
            Self::Identifier => s
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect(),
            Self::GitHubRepo => s
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

/// A visual cursor position in the rendered chat viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPoint {
    pub line: usize,
    pub column: usize,
}

/// Drag selection in rendered chat content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    pub anchor: SelectionPoint,
    pub cursor: SelectionPoint,
}

impl TextSelection {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    #[must_use]
    pub fn normalized(&self) -> (SelectionPoint, SelectionPoint) {
        if (self.anchor.line, self.anchor.column) <= (self.cursor.line, self.cursor.column) {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
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

/// SSH add server mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SshAddMode {
    /// Not adding a server
    #[default]
    None,
    /// Entering server name
    Name,
    /// Entering SSH target
    Target,
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
    pub const ALL: &'static [Self] = &[
        Self::Providers,
        Self::Models,
        Self::Preferences,
    ];

    /// Display name for the section tab.
    #[must_use] 
    pub const fn name(self) -> &'static str {
        match self {
            Self::Providers => "Providers",
            Self::Models => "Models",
            Self::Preferences => "Preferences",
        }
    }

    /// Next section (Tab).
    #[must_use] 
    pub const fn next(self) -> Self {
        match self {
            Self::Providers => Self::Models,
            Self::Models => Self::Preferences,
            Self::Preferences => Self::Providers,
        }
    }

    /// Previous section (Shift+Tab).
    #[must_use] 
    pub const fn prev(self) -> Self {
        match self {
            Self::Providers => Self::Preferences,
            Self::Models => Self::Providers,
            Self::Preferences => Self::Models,
        }
    }
}

/// What field is being edited in the config screen.
#[derive(Debug, Clone, Default)]
pub enum ConfigEditMode {
    /// Not editing, just navigating
    #[default]
    None,
    /// Editing a text field (provider API key, base URL, etc.)
    Text { input: String, cursor: usize },
    /// Selecting from a list (provider type, model list)
    List { items: Vec<String>, selected: usize },
    /// Editing a custom provider (name, `base_url`, `api_key`)
    CustomProvider {
        /// Which field is focused (0=name, `1=base_url`, `2=api_key`)
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
    #[must_use] 
    pub const fn row_count(&self, _config: &MayConfig) -> usize {
        match self.section {
            ConfigSection::Providers => self.provider_ids.len() + 1,
            ConfigSection::Models => 4,
            ConfigSection::Preferences => 3,
        }
    }

    /// Check if currently editing a text field.
    #[must_use] 
    pub const fn is_editing(&self) -> bool {
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
    #[must_use] 
    pub fn edit_text(&self) -> Option<(&str, usize)> {
        match &self.edit_mode {
            ConfigEditMode::Text { input, cursor } => Some((input, *cursor)),
            _ => None,
        }
    }

    /// Get mutable edit text.
    pub const fn edit_text_mut(&mut self) -> Option<(&mut String, &mut usize)> {
        match &mut self.edit_mode {
            ConfigEditMode::Text { input, cursor } => Some((input, cursor)),
            _ => None,
        }
    }
}

/// Try to detect API key from environment variables for the given provider.
#[must_use] 
pub fn detect_api_key_from_env(provider: ProviderType) -> Option<(String, &'static str)> {
    let env_vars = match provider {
        ProviderType::Anthropic => &["ANTHROPIC_API_KEY", "CLAUDE_API_KEY"][..],
        ProviderType::OpenAI => &["OPENAI_API_KEY"][..],
        ProviderType::Google => &["GOOGLE_API_KEY", "GEMINI_API_KEY"][..],
        ProviderType::OpenRouter => &["OPENROUTER_API_KEY"][..],
        ProviderType::DeepSeek => &["DEEPSEEK_API_KEY"][..],
        ProviderType::Groq => &["GROQ_API_KEY"][..],
        ProviderType::Together => &["TOGETHER_API_KEY"][..],
        ProviderType::Mistral => &["MISTRAL_API_KEY"][..],
        ProviderType::XAI => &["XAI_API_KEY", "GROK_API_KEY"][..],
        ProviderType::Copilot | ProviderType::Ollama | ProviderType::Custom => return None,
    };

    for var_name in env_vars {
        if let Ok(key) = std::env::var(var_name)
            && !key.is_empty()
        {
            return Some((key, *var_name));
        }
    }
    None
}
