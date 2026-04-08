#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("failed to load config: {0}")]
    LoadConfig(#[source] crate::config::ConfigError),
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("session error: {0}")]
    Session(#[from] crate::session::SessionError),
    #[error("database error: {0}")]
    Database(#[from] crate::db::ChatDbError),
    #[error("session storage error: {0}")]
    Store(#[from] crate::session::StoreError),
    #[error("skill error: {0}")]
    Skill(#[from] crate::skill::SkillError),
    #[error("clipboard error: {0}")]
    Clipboard(#[from] crate::tui::clipboard::ImageError),
    #[error("image manager not initialized")]
    MissingImageManager,
    #[error("text manager not initialized")]
    MissingTextManager,
    #[error("clipboard paste result contained no image or text")]
    EmptyPasteResult,
    #[error("failed to copy selection to clipboard")]
    ClipboardCopyFailed,
}

pub type AppResult<T> = Result<T, TuiError>;
