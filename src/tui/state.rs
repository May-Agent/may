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
    /// `ChatGPT` subscription OAuth authentication
    ChatgptAuth,
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
    /// Rewind picker: restore the conversation (and optionally the code)
    /// to the point before a user prompt
    Rewind(RewindState),
    /// Background jobs and running subagents viewer
    Jobs,
    /// Detail view of one background job or subagent run
    JobDetail(JobDetailState),
    /// `/btw` side-question overlay: answered outside conversation history
    Btw(BtwState),
}

/// `/btw` overlay state: type a question, stream a one-off answer.
#[derive(Debug, Clone, Default)]
pub struct BtwState {
    /// Question text being typed, then the submitted question.
    pub input: String,
    /// Answer streamed back so far.
    pub answer: String,
    /// A request is in flight.
    pub loading: bool,
    /// The request finished (answer complete) or failed.
    pub done: bool,
    /// Failure text when the request failed.
    pub error: Option<String>,
    /// Scroll offset (lines) for long answers.
    pub scroll: u16,
}

/// What the job-detail view is showing.
#[derive(Debug, Clone, Copy)]
pub enum JobDetailTarget {
    /// A background terminal job — output is fetched live by pid.
    Job(u32),
    /// A tracked `subagent` run — stats refresh from the tracker.
    Subagent(u64),
}

/// State for the job/subagent detail view.
#[derive(Debug)]
pub struct JobDetailState {
    /// Which entry the view follows.
    pub target: JobDetailTarget,
    /// Latest job info for a `Job` target; `None` once the pid is gone.
    pub job: Option<aither_sandbox::JobInfo>,
    /// Latest buffered `(stdout, stderr)` for a `Job` target.
    pub output: Option<(Vec<u8>, Vec<u8>)>,
    /// Latest snapshot for a `Subagent` target; kept after the run ends.
    pub subagent: Option<crate::agent::SubagentRunSnapshot>,
    /// Whether the followed entry has finished.
    pub finished: bool,
    /// Scroll offset into the rendered body.
    pub scroll: usize,
    /// Last time the view refreshed its data.
    pub refreshed: Option<std::time::Instant>,
}

impl JobDetailState {
    /// Empty detail state for `target`.
    #[must_use]
    pub const fn new(target: JobDetailTarget) -> Self {
        Self {
            target,
            job: None,
            output: None,
            subagent: None,
            finished: false,
            scroll: 0,
            refreshed: None,
        }
    }
}

/// One rewindable prompt: a user message plus the code changes the turn
/// it started is known to have made.
#[derive(Debug, Clone)]
pub struct RewindPrompt {
    /// Position of the persisted user message in session history; the
    /// rewind cut point.
    pub history_index: usize,
    /// Full prompt text (restored into the input box on rewind).
    pub content: String,
    /// Attachments carried by the prompt (restored with the text).
    pub attachments: Vec<crate::tui::queue::Attachment>,
    /// Files changed by the turn this prompt started (all roots), when
    /// the turn's tool calls are still in the stream.
    pub files: usize,
    /// Total added lines.
    pub additions: usize,
    /// Total deleted lines.
    pub deletions: usize,
}

/// Rewind picker state: first pick a prompt, then the restoration scope.
#[derive(Debug)]
pub struct RewindState {
    /// User prompts, oldest first.
    pub targets: Vec<RewindPrompt>,
    /// Cursor position; `targets.len()` is the trailing `(current)` entry,
    /// which selects nothing and simply closes the picker.
    pub selected: usize,
    /// Prompt the scope submenu is open for, if any.
    pub options_for: Option<usize>,
    /// Cursor position in the scope submenu (0 = conversation only,
    /// 1 = conversation and code).
    pub option_selected: usize,
}

/// Per-question interaction state.
#[derive(Debug)]
pub struct AskUserQuestionState {
    /// Cursor position in the option list (includes the extra custom item).
    pub cursor: usize,
    /// Selected option indices (for the predefined options).
    pub selections: Vec<usize>,
    /// Whether the user is typing a custom answer.
    pub custom_mode: bool,
    /// Custom answer text buffer.
    pub custom_text: String,
}

impl Default for AskUserQuestionState {
    fn default() -> Self {
        Self::new()
    }
}

impl AskUserQuestionState {
    /// Whether the user chose an option or wrote a text. Nothing else
    /// answers a question.
    #[must_use]
    pub fn is_answered(&self) -> bool {
        !self.selections.is_empty() || !self.custom_text.trim().is_empty()
    }

    /// Create empty question state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cursor: 0,
            selections: Vec::new(),
            custom_mode: false,
            custom_text: String::new(),
        }
    }
}

/// State for the `ask_user` interaction.
impl std::fmt::Debug for AskUserState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskUserState")
            .field("question_count", &self.questions.len())
            .field("current_question", &self.current_question)
            .finish_non_exhaustive()
    }
}

/// Live `ask_user` interaction state (request id, question states, focus).
pub struct AskUserState {
    /// Stable persisted request identifier.
    pub request_id: String,
    /// The prompt a run is blocked on (holds the response channel); `None`
    /// for an open question, which no run waits on.
    pub request: Option<AskUserRequest>,
    /// Normalized list of questions.
    pub questions: Vec<Question>,
    /// Current question index.
    pub current_question: usize,
    /// Per-question state.
    pub question_states: Vec<AskUserQuestionState>,
}

impl AskUserState {
    /// Build interaction state for `request` and its `questions`.
    #[must_use]
    pub fn new(request_id: String, request: AskUserRequest, questions: Vec<Question>) -> Self {
        Self::build(request_id, Some(request), questions)
    }

    /// Build interaction state for the open question `open`.
    #[must_use]
    pub fn for_open_question(open: crate::session::PersistedAskUserRequest) -> Self {
        let questions = open
            .questions
            .into_iter()
            .map(|question| Question {
                section: question.section,
                question: question.question,
                option: question.option,
                multi_select: question.multi_select,
            })
            .collect();
        Self::build(open.id, None, questions)
    }

    fn build(
        request_id: String,
        request: Option<AskUserRequest>,
        questions: Vec<Question>,
    ) -> Self {
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
    /// options + "Custom answer"
    #[must_use]
    pub fn current_item_count(&self) -> usize {
        self.current().map_or(0, |q| q.option.len() + 1)
    }

    /// Whether there are multiple questions.
    #[must_use]
    pub const fn is_multi(&self) -> bool {
        self.questions.len() > 1
    }
}

/// Pending workspace-access confirmation shown in the TUI.
#[derive(Debug)]
pub struct WorkspaceConfirmState {
    /// Persisted request id.
    pub request_id: String,
    /// The request being confirmed.
    pub request: WorkspaceRequest,
}

/// Pending permission confirmation shown in the TUI.
#[derive(Debug)]
pub struct PermissionConfirmState {
    /// ID of the pending permission request being confirmed.
    pub request_id: String,
    /// The permission request being confirmed.
    pub request: crate::session::PersistedPermissionRequestPayload,
}

/// Status of Copilot OAuth authentication flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopilotAuthStatus {
    /// Requesting device code from GitHub
    RequestingCode,
    /// Waiting for user to authorize in browser
    WaitingForUser {
        /// User code shown for device authorization.
        user_code: String,
        /// URL the user must visit to authorize.
        verification_uri: String,
    },
    /// Successfully authenticated
    Success,
    /// Authentication failed
    Failed(String),
}

/// Status of `ChatGPT` OAuth authentication flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatgptAuthStatus {
    /// Talking to the `OpenAI` auth service (requesting a device code).
    Working,
    /// Browser flow: waiting for the user to authorize; the loopback listener
    /// on localhost:1455 will receive the callback.
    WaitingForCallback {
        /// Loopback URL shown to the user while awaiting the callback.
        url: String,
    },
    /// Device flow (SSH/headless): user authorizes on the verification page.
    WaitingForUser {
        /// User code shown for device authorization.
        user_code: String,
        /// URL the user must visit to authorize.
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
#[derive(Debug)]
pub enum ActiveTextInput<'a> {
    /// Input with cursor position tracking (insert at cursor)
    Cursored {
        /// The buffer being edited.
        input: &'a mut String,
        /// Cursor position within the buffer.
        cursor: &'a mut usize,
    },
    /// Simple input (append at end) with character filter
    Simple {
        /// The buffer being edited.
        input: &'a mut String,
        /// Filter deciding which characters are accepted.
        filter: PasteFilter,
    },
}

/// A visual cursor position in the rendered chat viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPoint {
    /// Line in the rendered viewport.
    pub line: usize,
    /// Column in the rendered viewport.
    pub column: usize,
}

/// Drag selection in rendered chat content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    /// Fixed end of the selection.
    pub anchor: SelectionPoint,
    /// Moving end of the selection.
    pub cursor: SelectionPoint,
}

impl TextSelection {
    /// Whether the selection covers zero cells.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    /// (start, end) ordering of the selection ends.
    #[must_use]
    pub fn normalized(&self) -> (SelectionPoint, SelectionPoint) {
        if (self.anchor.line, self.anchor.column) <= (self.cursor.line, self.cursor.column) {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
}

/// A status-line notice. A notice with a deadline is transient and clears
/// itself once the deadline passes; one without persists until replaced.
#[derive(Debug, Clone)]
pub struct StatusNotice {
    /// Text shown in the status line.
    pub text: String,
    /// Deadline after which the notice clears; `None` means persistent.
    pub expires_at: Option<std::time::Instant>,
}

impl StatusNotice {
    /// A notice that stays until something replaces or clears it.
    pub fn persistent(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            expires_at: None,
        }
    }

    /// A notice that clears itself `lifetime` after creation.
    pub fn transient(text: impl Into<String>, lifetime: std::time::Duration) -> Self {
        Self {
            text: text.into(),
            expires_at: Some(std::time::Instant::now() + lifetime),
        }
    }

    /// Whether the notice's deadline passed at `now`.
    #[must_use]
    pub fn expired(&self, now: std::time::Instant) -> bool {
        self.expires_at.is_some_and(|deadline| now >= deadline)
    }

    /// Drop the notice in `slot` once its deadline has passed. Event-loop
    /// ticks call this so a transient notice disappears without user input.
    pub fn clear_expired(slot: &mut Option<Self>, now: std::time::Instant) {
        if slot.as_ref().is_some_and(|notice| notice.expired(now)) {
            *slot = None;
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
    /// Provider list section.
    #[default]
    Providers,
    /// Model selection section.
    Models,
    /// Preferences section.
    Preferences,
}

impl ConfigSection {
    /// All sections for iteration.
    pub const ALL: &'static [Self] = &[Self::Providers, Self::Models, Self::Preferences];

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
    Text {
        /// Current text being edited.
        input: String,
        /// Cursor position within `input`, in characters.
        cursor: usize,
    },
    /// Selecting from a list (provider type, model list)
    List {
        /// Selectable items.
        items: Vec<String>,
        /// Index of the highlighted item.
        selected: usize,
    },
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

/// Which setting the config screen's text edit writes to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConfigTextTarget {
    /// The selected row's primary value: API key, model list, preference.
    #[default]
    Primary,
    /// The selected provider's cache lifetime.
    CacheLifetime,
}

/// Unified config state for all sections.
#[derive(Debug)]
pub struct ConfigState {
    /// Current section (Tab to switch)
    pub section: ConfigSection,
    /// Selected row within current section
    pub selected: usize,
    /// Edit mode (text input, list selection, or none)
    pub edit_mode: ConfigEditMode,
    /// Setting the active text edit writes to
    pub text_target: ConfigTextTarget,
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
            text_target: ConfigTextTarget::default(),
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
            ConfigSection::Models => 5,
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
        self.text_target = ConfigTextTarget::Primary;
        let len = initial.len();
        self.edit_mode = ConfigEditMode::Text {
            input: initial.to_string(),
            cursor: len,
        };
    }

    /// Cancel editing.
    pub fn cancel_edit(&mut self) {
        self.text_target = ConfigTextTarget::Primary;
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
        ProviderType::Copilot
        | ProviderType::Chatgpt
        | ProviderType::Ollama
        | ProviderType::LMStudio
        | ProviderType::Custom => return None,
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
#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::StatusNotice;

    #[test]
    fn transient_notice_clears_after_expiry() {
        let mut status = Some(StatusNotice::transient(
            "Copied selection",
            Duration::from_secs(2),
        ));

        // Before the deadline the notice stays.
        StatusNotice::clear_expired(&mut status, Instant::now());
        assert!(status.is_some());

        // Past the deadline the same sweep drops it.
        StatusNotice::clear_expired(&mut status, Instant::now() + Duration::from_secs(3));
        assert!(status.is_none());
    }

    #[test]
    fn persistent_notice_does_not_expire() {
        let mut status = Some(StatusNotice::persistent("^C Press Ctrl+C again to exit"));

        StatusNotice::clear_expired(&mut status, Instant::now() + Duration::from_secs(3600));
        assert!(status.is_some());
    }
}
