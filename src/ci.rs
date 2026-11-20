//! CI durations: what each job, step and test of a session PR's checks took,
//! the baselines jobs are compared with, and the finding raised when a job
//! runs abnormally long.
//!
//! Forge CI durations — GitHub Actions workflow runs and GitLab pipelines —
//! come from the forge's REST API; local `[[projects]] checks` are timed by
//! May itself. Everything is stored in the chat database and reported by
//! the `ci` command; an abnormal job also becomes one alert to the waiting
//! agent and one warning to the user.

pub mod github;
pub mod gitlab;
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
    /// GitLab could not be read.
    #[error("GitLab CI: {0}")]
    GitLab(String),
    /// The repository a `ci` call reads could not be resolved.
    #[error("{0}")]
    Resolve(String),
    /// A template did not render.
    #[error("template: {0}")]
    Render(#[from] askama::Error),
    /// The `[ci]` config could not be read.
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
}
