//! CI durations: what each job, step and test of a session PR's checks took,
//! the baselines jobs are compared with, and the finding raised when a job
//! runs abnormally long.
//!
//! GitHub Actions durations come from its API through the `gh` CLI; local
//! `[[projects]] checks` are timed by May itself. Everything is stored in
//! the chat database and reported by the `ci` command; an abnormal job also
//! becomes one alert to the waiting agent and one warning to the user.

pub mod github;
pub mod logs;
pub mod record;
pub mod report;
pub mod timing;

/// Why CI durations could not be recorded or read.
#[derive(Debug, thiserror::Error)]
pub enum CiError {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] crate::db::ChatDbError),
    /// GitHub could not be read.
    #[error("GitHub Actions: {0}")]
    Github(String),
    /// A template did not render.
    #[error("template: {0}")]
    Render(#[from] askama::Error),
    /// The `[ci]` config could not be read.
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
}
