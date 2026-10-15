/// Error type for TUI operations.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// Failed to load config.
    #[error("failed to load config: {0}")]
    LoadConfig(#[source] crate::config::ConfigError),
    /// Config error.
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    /// Session error.
    #[error("session error: {0}")]
    Session(#[from] crate::session::SessionError),
    /// Database error.
    #[error("database error: {0}")]
    Database(#[from] crate::db::ChatDbError),
    /// Session storage error.
    #[error("session storage error: {0}")]
    Store(#[from] crate::session::StoreError),
    /// Skill error.
    #[error("skill error: {0}")]
    Skill(#[from] crate::skill::SkillError),
    /// Clipboard error.
    #[error("clipboard error: {0}")]
    Clipboard(#[from] crate::tui::clipboard::ImageError),
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
