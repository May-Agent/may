/// Error type for TUI operations.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// Failed to load config.
    #[error("failed to load config: {0}")]
    LoadConfig(#[source] may_config::config::ConfigError),
    /// Config error.
    #[error("config error: {0}")]
    Config(#[from] may_config::config::ConfigError),
    /// Session error.
    #[error("session error: {0}")]
    Session(#[from] may_client::session::SessionError),
    /// Database error.
    #[error("database error: {0}")]
    Database(#[from] may_db::ChatDbError),
    /// The May daemon refused or could not be reached.
    #[error(transparent)]
    Daemon(#[from] may_client::client::DaemonClientError),
    /// Session storage error.
    #[error("session storage error: {0}")]
    Store(#[from] may_client::session::StoreError),
    /// Skill error.
    #[error("skill error: {0}")]
    Skill(#[from] may_client::skill::SkillError),
    /// Clipboard error.
    #[error("clipboard error: {0}")]
    Clipboard(#[from] crate::clipboard::ImageError),
    /// Image manager not initialized.
    #[error("image manager not initialized")]
    MissingImageManager,
    /// Text manager not initialized.
    #[error("text manager not initialized")]
    MissingTextManager,
    /// Clipboard paste result contained no image or text.
    #[error("clipboard paste result contained no image or text")]
    EmptyPasteResult,
    /// Failed to copy selection to clipboard.
    #[error("failed to copy selection to clipboard")]
    ClipboardCopyFailed,
}

/// Result alias for TUI operations.
pub type AppResult<T> = Result<T, TuiError>;
