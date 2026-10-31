//! Event handling for the TUI.

use std::time::Duration;

use async_channel::{Receiver, Sender};
use blocking::unblock;
use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseEvent};
use executor_core::spawn;

use crate::profile::{ProfileInfo, ProfileSessions};
use crate::{SessionSummary, SkillInfo};

use super::error::TuiError;

const ERR_TUI_EVENT_POLL: &str = include_str!("../texts/event_poll_failed.txt");

/// Application events.
#[derive(Debug)]
pub enum AppEvent {
    /// Key press
    Key(KeyCode, KeyModifiers),
    /// Tick (for polling)
    Tick,
    /// Terminal resize
    Resize(u16, u16),
    /// Pasted text (from bracketed paste)
    Paste(String),
    /// Mouse event (for wheel scrolling in alternate screen)
    Mouse(MouseEvent),

    // ─── Async operation results ───────────────────────────────────────
    /// Session list loaded from background task
    SessionsLoaded(Result<Vec<SessionSummary>, TuiError>),
    /// Session deleted from background task (includes refreshed list)
    SessionDeleted {
        /// The session removed from the database, when deletion got that far.
        deleted: Option<crate::session::SessionId>,
        /// Whether the deleted session was the active one.
        was_current: bool,
        /// Refreshed session list after the delete, or the failure.
        result: Result<Vec<SessionSummary>, TuiError>,
    },

    // ─── Skill operation results ───────────────────────────────────────
    /// Skills list loaded from background task
    SkillsLoaded(Result<Vec<SkillInfo>, TuiError>),
    /// Skill installed from GitHub
    SkillInstalled(Result<Vec<SkillInfo>, TuiError>),
    /// Skill deleted
    SkillDeleted(Result<Vec<SkillInfo>, TuiError>),

    // ─── Profile operation results ──────────────────────────────────
    /// Profile list loaded (for `ProfileSelect` mode)
    ProfilesLoaded(Result<Vec<ProfileInfo>, TuiError>),
    /// Profile sessions loaded (pinned/regular/archived groups)
    ProfileSessionsLoaded(Result<ProfileSessions, TuiError>),

    /// Background config save completed.
    ConfigSaved(Result<(), TuiError>),

    // ─── Image loading ────────────────────────────────────────────────
    /// Image bytes decoded into a renderable state on a worker thread.
    /// `state=None` means fetch or decode failed for `key`.
    ImageDecoded {
        /// Cache key the decoded image belongs to.
        key: String,
        /// Ready-to-render protocol state; `None` when fetch/decode failed.
        state: Option<super::ui::image::ImageState>,
    },

    // ─── Sidebar ─────────────────────────────────────────────────────
    /// The session's `tasks.md`, as the daemon served it (`None`: no file).
    SidebarTasks {
        /// Session the document belongs to.
        session_id: String,
        /// Document content.
        content: Option<String>,
        /// Session title as stored (the daemon generates it).
        title: Option<String>,
    },
    /// Lookups of issues the session's tasks link.
    SidebarIssues {
        /// Session the tasks belong to.
        session_id: String,
        /// The issues found.
        issues: Vec<crate::tui::sidebar::IssueBrief>,
    },

    // ─── Background jobs ─────────────────────────────────────────────
    /// The session's jobs and subagent runs, as the daemon reported them.
    BackgroundLoaded {
        /// Terminal jobs.
        jobs: Vec<aither_sandbox::JobInfo>,
        /// Subagent runs in flight.
        subagents: Vec<crate::agent::SubagentRunSnapshot>,
    },
    /// A background request finished with a message for the status line.
    Status(String),
    /// The daemon dropped turn frames this client missed: the session's
    /// history must be reloaded.
    HistoryStale,
    /// A kill request finished; `true` when the job accepted it.
    JobKilled {
        /// Process ID the kill targeted.
        pid: u32,
        /// Whether the registry reported the job killed.
        ok: bool,
    },
    /// Output fetch for the job open in `AppMode::JobDetail` finished.
    JobDetailLoaded {
        /// Refreshed job info, when the pid still exists.
        job: Option<aither_sandbox::JobInfo>,
        /// Buffered `(stdout, stderr)`, when the registry still serves it.
        output: Option<(Vec<u8>, Vec<u8>)>,
    },
    /// Incremental terminal-output poll for running tool calls.
    TerminalOutputs(Vec<TerminalOutputDelta>),

    // ─── /btw side question ─────────────────────────────────────────
    /// An answer chunk arrived for the `/btw` overlay.
    BtwDelta(String),
    /// The `/btw` request finished streaming.
    BtwFinished,
    /// The `/btw` request failed.
    BtwFailed(String),

    // ─── bash mode / secrets ───────────────────────────────────────
    /// A bash-mode command finished; output is already masked.
    BashFinished(Box<BashOutcome>),
    /// A `/secret` write to the keychain completed.
    SecretStored {
        /// Env name that was stored.
        key: String,
        /// Store outcome.
        result: Result<(), String>,
    },
}

impl AppEvent {
    /// Stable name for logs and jank reports.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Key(..) => "key",
            Self::Tick => "tick",
            Self::Resize(..) => "resize",
            Self::Paste(_) => "paste",
            Self::Mouse(_) => "mouse",
            Self::SessionsLoaded(_) => "sessions_loaded",
            Self::SessionDeleted { .. } => "session_deleted",
            Self::SkillsLoaded(_) => "skills_loaded",
            Self::SkillInstalled(_) => "skill_installed",
            Self::SkillDeleted(_) => "skill_deleted",
            Self::ProfilesLoaded(_) => "profiles_loaded",
            Self::ProfileSessionsLoaded(_) => "profile_sessions_loaded",
            Self::ConfigSaved(_) => "config_saved",
            Self::ImageDecoded { .. } => "image_decoded",
            Self::SidebarTasks { .. } => "sidebar_tasks",
            Self::SidebarIssues { .. } => "sidebar_issues",
            Self::BackgroundLoaded { .. } => "background_loaded",
            Self::Status(_) => "status",
            Self::HistoryStale => "history_stale",
            Self::JobKilled { .. } => "job_killed",
            Self::JobDetailLoaded { .. } => "job_detail_loaded",
            Self::TerminalOutputs(_) => "terminal_outputs",
            Self::BtwDelta(_) => "btw_delta",
            Self::BtwFinished => "btw_finished",
            Self::BtwFailed(_) => "btw_failed",
            Self::BashFinished(_) => "bash_finished",
            Self::SecretStored { .. } => "secret_stored",
        }
    }
}

/// Masked result of a bash-mode command, ready for display and the agent.
#[derive(Debug)]
pub struct BashOutcome {
    /// Chat element id (`bash-*`) showing the run.
    pub call_id: String,
    /// The raw command as typed (unmasked — the run already happened).
    pub command: String,
    /// Captured stdout (masked).
    pub stdout: String,
    /// Captured stderr (masked).
    pub stderr: String,
    /// Process exit code; `-1` when the signal had none.
    pub exit_code: i32,
    /// A secret was detected in the command or its output.
    pub had_secret: bool,
    /// Spawn/capture failure message, when the command never ran.
    pub error: Option<String>,
}

/// One incremental output read bound to a running terminal tool call.
#[derive(Debug)]
pub struct TerminalOutputDelta {
    /// Tool call the output belongs to.
    pub call_id: String,
    /// Buffer cursor after this read; the caller stores it for the next poll.
    pub cursor: usize,
    /// Output bytes appended since the previous cursor.
    pub bytes: Vec<u8>,
    /// Job status at read time — a finished job unbinds the call.
    pub status: aither_sandbox::JobStatus,
}

/// Sender type for sending events back to the main loop.
pub type EventSender = Sender<AppEvent>;

/// Event handler that polls terminal events.
#[derive(Debug)]
pub struct EventHandler {
    rx: Receiver<AppEvent>,
    tx: EventSender,
}

impl EventHandler {
    /// Create a new event handler.
    ///
    /// # Panics
    ///
    /// Panics if the spawned terminal-event polling thread fails to start.
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = async_channel::unbounded();
        let tx_clone = tx.clone();

        // Spawn event polling task
        spawn(async move {
            loop {
                // Poll with timeout for tick events
                let has_event = unblock(|| event::poll(Duration::from_millis(50)))
                    .await
                    .unwrap_or_else(|e| panic!("{}: {}", ERR_TUI_EVENT_POLL.trim(), e));
                if has_event {
                    let evt = unblock(event::read)
                        .await
                        .unwrap_or_else(|e| panic!("{}: {}", ERR_TUI_EVENT_POLL.trim(), e));
                    let app_event = match evt {
                        Event::Key(key) => AppEvent::Key(key.code, key.modifiers),
                        Event::Resize(w, h) => AppEvent::Resize(w, h),
                        Event::Paste(text) => AppEvent::Paste(text),
                        Event::Mouse(mouse) => AppEvent::Mouse(mouse),
                        _ => continue,
                    };
                    if tx_clone.send(app_event).await.is_err() {
                        break;
                    }
                } else if tx_clone.send(AppEvent::Tick).await.is_err() {
                    break;
                }
            }
        })
        .detach();

        Self { rx, tx }
    }

    /// Get a clone of the sender for sending events from background tasks.
    #[must_use]
    pub fn sender(&self) -> EventSender {
        self.tx.clone()
    }

    /// Get the next event.
    pub async fn next(&mut self) -> Option<AppEvent> {
        self.rx.recv().await.ok()
    }

    /// Events queued behind the receiver — a growing backlog during a
    /// scroll burst means the loop cannot keep up with input.
    #[must_use]
    pub fn backlog(&self) -> usize {
        self.rx.len()
    }
}

impl Default for EventHandler {
    fn default() -> Self {
        Self::new()
    }
}
