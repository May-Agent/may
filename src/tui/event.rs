//! Event handling for the TUI.

use std::time::Duration;

use async_channel::{Receiver, Sender};
use blocking::unblock;
use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseEvent};
use executor_core::spawn;

use crate::profile::{ProfileInfo, ProfileSessions};
use crate::session::import::ForeignSessionSummary;
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
    /// Installed and connected ACP agents listed for the `/connect` picker
    AcpAgentsLoaded(Result<crate::client::acp::AcpAgentsResponse, TuiError>),
    /// A `/connect` request finished
    AcpConnectFinished(Result<super::connect::ConnectOutcome, TuiError>),
    /// Claude Code transcripts listed for the `/resume-claude` picker
    ResumeSessionsLoaded(Vec<ForeignSessionSummary>),
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
    /// The daemon pushed new profile sessions (archive, working state).
    ProfileSessionsPushed(ProfileSessions),

    /// Background config save completed.
    ConfigSaved(Result<(), TuiError>),

    /// The session's stored reasoning effort arrived from the daemon.
    EffortLoaded(Option<String>),
    /// A `/effort` update finished; Ok carries the applied token.
    EffortApplied(Result<Option<String>, TuiError>),

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
    /// The session's PRs, as the daemon served them.
    SidebarPrs {
        /// Session the PRs belong to.
        session_id: String,
        /// PR list entries.
        prs: Vec<crate::client::WirePr>,
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
    /// The session's blocking state, as the daemon reported it on the
    /// `session_runtime` stream.
    SessionRuntime(Box<crate::client::SessionRuntimeEvent>),
    /// The session's worktree index coverage, from the `session_index` stream.
    IndexCoverage(crate::client::SessionIndexSnapshot),
    /// A turn started in the daemon (this client's or another's), carrying
    /// the stored user-message row id — the prompt-mark key file rewind
    /// checkpoints hang on.
    TurnStarted {
        /// Row id of the user message the turn answers.
        message_id: i32,
    },
    /// The daemon withdrew the latest turn: it was stopped before any tool
    /// call or response text, so its message left the conversation.
    TurnWithdrawn {
        /// The withdrawn message text.
        content: String,
        /// Whether this client sent it and gets it back in its input.
        own: bool,
    },
    /// The daemon dropped turn frames this client missed: the session's
    /// history must be reloaded.
    HistoryStale,
    /// The user's global sleep state, from the `sleep` event.
    Sleep(crate::sleep::SleepStatus),
    /// A popup of some session waits on the user, from the
    /// `user_notification` event.
    UserNotification(Box<crate::notification::UserNotification>),
    /// A `session_suggestion` frame arrived: `text` is the new ghost text,
    /// `None` clears it.
    PromptSuggestion {
        /// The aux request that produced the update.
        request_id: u64,
        /// Session the suggestion belongs to.
        session_id: String,
        /// The suggested prompt, or `None` to clear it.
        text: Option<String>,
    },
    /// A `session_recap` frame arrived: `ready` carries the card's
    /// summary, `error` reports a failed generation.
    Recap {
        /// Session the recap belongs to.
        session_id: String,
        /// `ready` or `error`.
        state: String,
        /// The catch-up summary when `state` is `ready`.
        summary: Option<String>,
        /// The suggested next step, when the recap named one.
        next_action: Option<String>,
    },
    /// A `session_warning` frame arrived: shown to the user in the stream.
    Warning {
        /// Session the warning belongs to.
        session_id: String,
        /// The warning text.
        message: String,
    },
    /// A usage frame carried the context window the daemon resolved for the
    /// session's model.
    ContextWindow {
        /// Session the window belongs to.
        session_id: String,
        /// The window in tokens.
        window: u32,
    },
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

    // ─── /progress ─────────────────────────────────────────────────
    /// The `/progress` report arrived.
    ProgressLoaded(crate::daemon::ProgressReport),
    /// The `/progress` request failed.
    ProgressFailed(String),

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
            Self::AcpAgentsLoaded(_) => "acp_agents_loaded",
            Self::AcpConnectFinished(_) => "acp_connect_finished",
            Self::ResumeSessionsLoaded(_) => "resume_sessions_loaded",
            Self::SessionDeleted { .. } => "session_deleted",
            Self::SkillsLoaded(_) => "skills_loaded",
            Self::SkillInstalled(_) => "skill_installed",
            Self::SkillDeleted(_) => "skill_deleted",
            Self::ProfilesLoaded(_) => "profiles_loaded",
            Self::ProfileSessionsLoaded(_) => "profile_sessions_loaded",
            Self::ProfileSessionsPushed(_) => "profile_sessions_pushed",
            Self::ConfigSaved(_) => "config_saved",
            Self::EffortLoaded(_) => "effort_loaded",
            Self::EffortApplied(_) => "effort_applied",
            Self::ImageDecoded { .. } => "image_decoded",
            Self::SidebarTasks { .. } => "sidebar_tasks",
            Self::SidebarPrs { .. } => "sidebar_prs",
            Self::SidebarIssues { .. } => "sidebar_issues",
            Self::BackgroundLoaded { .. } => "background_loaded",
            Self::Status(_) => "status",
            Self::SessionRuntime(_) => "session_runtime",
            Self::IndexCoverage(_) => "index_coverage",
            Self::TurnStarted { .. } => "turn_started",
            Self::TurnWithdrawn { .. } => "turn_withdrawn",
            Self::HistoryStale => "history_stale",
            Self::Sleep(_) => "sleep",
            Self::UserNotification(_) => "user_notification",
            Self::PromptSuggestion { .. } => "prompt_suggestion",
            Self::Recap { .. } => "recap",
            Self::Warning { .. } => "warning",
            Self::ContextWindow { .. } => "context_window",
            Self::JobKilled { .. } => "job_killed",
            Self::JobDetailLoaded { .. } => "job_detail_loaded",
            Self::TerminalOutputs(_) => "terminal_outputs",
            Self::BtwDelta(_) => "btw_delta",
            Self::BtwFinished => "btw_finished",
            Self::BtwFailed(_) => "btw_failed",
            Self::ProgressLoaded(_) => "progress_loaded",
            Self::ProgressFailed(_) => "progress_failed",
            Self::BashFinished(_) => "bash_finished",
            Self::SecretStored { .. } => "secret_stored",
        }
    }
}

/// Masked result of a bash-mode command, ready for display and the agent.
pub use crate::bash::BashOutcome;

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

    /// A queued key, paste, mouse, or resize, skipping coalesced ticks.
    ///
    /// The main loop takes this before waiting on a ready agent event so a
    /// thinking stream cannot starve typing and Esc. Ticks are heartbeats;
    /// dropping a backlog of them is correct.
    #[must_use]
    pub fn try_recv_input(&self) -> Option<AppEvent> {
        loop {
            match self.rx.try_recv() {
                Ok(AppEvent::Tick) => {}
                Ok(event) => return Some(event),
                Err(_) => return None,
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (EventHandler, EventSender) {
        let (tx, rx) = async_channel::unbounded();
        (EventHandler { rx, tx: tx.clone() }, tx)
    }

    #[test]
    fn try_recv_input_skips_ticks_and_returns_a_key() {
        let (handler, tx) = pair();
        tx.send_blocking(AppEvent::Tick).unwrap();
        tx.send_blocking(AppEvent::Tick).unwrap();
        tx.send_blocking(AppEvent::Key(KeyCode::Char('a'), KeyModifiers::NONE))
            .unwrap();
        let event = handler.try_recv_input().expect("key");
        assert!(matches!(event, AppEvent::Key(KeyCode::Char('a'), _)));
        assert!(handler.try_recv_input().is_none());
    }

    #[test]
    fn try_recv_input_is_none_when_only_ticks_are_queued() {
        let (handler, tx) = pair();
        tx.send_blocking(AppEvent::Tick).unwrap();
        tx.send_blocking(AppEvent::Tick).unwrap();
        assert!(handler.try_recv_input().is_none());
    }

    #[test]
    fn try_recv_input_returns_paste_ahead_of_later_keys() {
        let (handler, tx) = pair();
        tx.send_blocking(AppEvent::Paste("hi".into())).unwrap();
        tx.send_blocking(AppEvent::Key(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(
            handler.try_recv_input(),
            Some(AppEvent::Paste(text)) if text == "hi"
        ));
        assert!(matches!(
            handler.try_recv_input(),
            Some(AppEvent::Key(KeyCode::Esc, _))
        ));
    }
}
