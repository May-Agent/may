//! Event reducer for top-level `AppEvent` handling.

pub(super) mod ask_user;
pub(super) mod manage;
pub(super) mod navigation;

use super::app::{App, SendAction};
use super::error::AppResult;
use super::event::AppEvent;
use super::state::{AppMode, StatusNotice};

/// Reduce a single UI event into state transitions and optional actions.
///
/// # Errors
///
/// Returns [`TuiError`](crate::tui::error::TuiError) when the underlying
/// handler fails — e.g. a database write for a UI action errors.
pub fn reduce_event(app: &mut App, event: AppEvent) -> AppResult<Option<SendAction>> {
    match event {
        AppEvent::Key(code, modifiers) => app.handle_key(code, modifiers),
        AppEvent::Tick => {
            // Sweep a transient status notice whose deadline passed; the tick
            // cadence bounds how long an expired notice can linger.
            StatusNotice::clear_expired(&mut app.status, std::time::Instant::now());
            if app.processing {
                app.spinner_frame = app.spinner_frame.wrapping_add(1);
                if let Some(start) = app.thinking_start {
                    let secs = start.elapsed().as_secs();
                    app.chat.update_thinking(secs);
                }
                // Bind running terminal calls to their jobs and poll live
                // output + file-diff previews.
                app.poll_live_output();
            }
            // The jobs list and subagent runs arrive as `session_background`
            // events while anything runs; nothing to poll for them here.
            // Same cadence for an open job/subagent detail view.
            if let AppMode::JobDetail(state) = &app.mode
                && state.refreshed.is_none_or(|t| t.elapsed().as_secs() >= 1)
            {
                app.refresh_job_detail();
            }
            Ok(None)
        }
        AppEvent::Resize(_, _) => Ok(None),
        AppEvent::Paste(text) => Ok(app.handle_bracketed_paste(text)),
        AppEvent::Mouse(mouse) => app.handle_mouse(mouse),

        AppEvent::SessionsLoaded(result) => {
            app.status = None;
            match result {
                Ok(sessions) => {
                    app.session_list = sessions;
                    app.session_selected = 0;
                    app.session_search.clear();
                    app.session_scroll = 0;
                    app.mode = AppMode::SessionSelect;
                }
                Err(e) => {
                    app.chat
                        .push_error(&format!("Failed to load sessions: {e}"));
                }
            }
            Ok(None)
        }
        AppEvent::SessionDeleted {
            deleted: _,
            was_current,
            result,
        } => {
            app.status = None;
            match result {
                Ok(sessions) => {
                    app.session_list = sessions;
                    if app.session_selected >= app.session_list.len() && app.session_selected > 0 {
                        app.session_selected -= 1;
                    }
                    if was_current {
                        app.mode = AppMode::Chat;
                        return Ok(Some(SendAction::NewSession));
                    }
                }
                Err(e) => {
                    app.chat
                        .push_error(&format!("Failed to delete session: {e}"));
                }
            }
            Ok(None)
        }

        AppEvent::SkillsLoaded(result) => {
            app.status = None;
            match result {
                Ok(skills) => {
                    app.skill_list = skills;
                    app.skill_selected = 0;
                    app.mode = AppMode::SkillManage;
                }
                Err(e) => {
                    app.chat.push_error(&format!("Failed to load skills: {e}"));
                }
            }
            Ok(None)
        }
        AppEvent::SkillInstalled(result) => {
            app.status = None;
            match result {
                Ok(skills) => {
                    app.skill_list = skills;
                    if app.skill_selected >= app.skill_list.len() && app.skill_selected > 0 {
                        app.skill_selected = app.skill_list.len().saturating_sub(1);
                    }
                }
                Err(e) => {
                    app.chat
                        .push_error(&format!("Failed to install skill: {e}"));
                }
            }
            Ok(None)
        }
        AppEvent::SkillDeleted(result) => {
            app.status = None;
            match result {
                Ok(skills) => {
                    app.skill_list = skills;
                    if app.skill_selected >= app.skill_list.len() && app.skill_selected > 0 {
                        app.skill_selected -= 1;
                    }
                }
                Err(e) => {
                    app.chat.push_error(&format!("Failed to delete skill: {e}"));
                }
            }
            Ok(None)
        }
        AppEvent::ProfilesLoaded(result) => {
            match result {
                Ok(profiles) => {
                    app.profiles = profiles;
                }
                Err(e) => {
                    app.chat
                        .push_error(&format!("Failed to load profiles: {e}"));
                }
            }
            Ok(None)
        }
        AppEvent::ProfileSessionsLoaded(result) => {
            app.status = None;
            match result {
                Ok(ps) => {
                    app.profile_sessions = Some(ps);
                    app.build_session_list_from_profile();
                    app.session_selected = 0;
                    app.session_search.clear();
                    app.session_scroll = 0;
                    // Only switch to SessionSelect if not already in it
                    // (refresh from pin/archive should keep current mode)
                    if !matches!(app.mode, AppMode::SessionSelect) {
                        app.mode = AppMode::SessionSelect;
                    }
                }
                Err(e) => {
                    app.chat
                        .push_error(&format!("Failed to load profile sessions: {e}"));
                }
            }
            Ok(None)
        }
        AppEvent::ConfigSaved(result) => {
            if let Err(error) = result {
                app.chat
                    .push_error(&format!("Failed to save config: {error}"));
                app.set_status("Config save failed");
            }
            Ok(None)
        }
        AppEvent::ImageDecoded { key, state } => {
            app.handle_image_decoded(key, state);
            Ok(None)
        }

        AppEvent::SidebarTasks {
            session_id,
            content,
            title,
        } => {
            let current = app
                .session_manager
                .current()
                .is_some_and(|session| session.id.as_str() == session_id);
            if current {
                let missing = app.sidebar.set_tasks(&session_id, content.as_deref());
                app.sidebar.title = title;
                app.request_issue_lookups(session_id, missing);
            }
            Ok(None)
        }
        AppEvent::SidebarIssues { session_id, issues } => {
            app.sidebar.set_issues(&session_id, issues);
            Ok(None)
        }
        AppEvent::BackgroundLoaded { jobs, subagents } => {
            app.jobs = jobs;
            app.subagents = subagents;
            let row_count = jobs_rows(app).len();
            if app.jobs_selected >= row_count && app.jobs_selected > 0 {
                app.jobs_selected = row_count.saturating_sub(1);
            }
            Ok(None)
        }
        AppEvent::Status(message) => {
            app.set_status(message);
            Ok(None)
        }
        AppEvent::JobKilled { pid, ok } => {
            app.set_status(if ok {
                format!("Killed job {pid}")
            } else {
                format!("Job {pid} already finished")
            });
            Ok(None)
        }
        AppEvent::JobDetailLoaded { job, output } => {
            if let AppMode::JobDetail(state) = &mut app.mode
                && let super::state::JobDetailTarget::Job(pid) = state.target
                && job.as_ref().is_none_or(|loaded| loaded.pid == pid)
            {
                state.job = job;
                state.output = output;
            }
            Ok(None)
        }
        AppEvent::TerminalOutputs(deltas) => {
            for delta in deltas {
                // A call that already ended dropped its binding at
                // ToolCallEnd — the final result is authoritative, so a
                // delta arriving late is dropped rather than appended.
                let Some(read) = app.live_reads.get_mut(&delta.call_id) else {
                    continue;
                };
                read.cursor = delta.cursor;
                read.pending.extend_from_slice(&delta.bytes);
                let complete = super::app::utf8_complete_len(&read.pending);
                let bytes: Vec<u8> = read.pending.drain(..complete).collect();
                if !matches!(delta.status, aither_sandbox::JobStatus::Running) {
                    app.live_reads.remove(&delta.call_id);
                }
                if !bytes.is_empty() {
                    app.chat
                        .append_tool_output(&delta.call_id, &String::from_utf8_lossy(&bytes));
                }
            }
            Ok(None)
        }
        AppEvent::BtwDelta(text) => {
            if let crate::tui::state::AppMode::Btw(state) = &mut app.mode {
                state.answer.push_str(&text);
            }
            Ok(None)
        }
        AppEvent::BtwFinished => {
            if let crate::tui::state::AppMode::Btw(state) = &mut app.mode {
                state.loading = false;
                state.done = true;
            }
            Ok(None)
        }
        AppEvent::BtwFailed(error) => {
            if let crate::tui::state::AppMode::Btw(state) = &mut app.mode {
                state.loading = false;
                state.done = true;
                state.error = Some(error);
            }
            Ok(None)
        }
        AppEvent::BashFinished(outcome) => {
            use std::fmt::Write as _;

            let outcome = *outcome;
            let mut output = outcome.stdout.clone();
            if !outcome.stderr.is_empty() {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&outcome.stderr);
            }
            if let Some(error) = &outcome.error {
                output.clone_from(error);
            }
            let status = if outcome.error.is_some() || outcome.exit_code != 0 {
                super::ui::chat::ToolStatus::Failed(format!("exit {}", outcome.exit_code))
            } else {
                super::ui::chat::ToolStatus::Done
            };
            app.chat
                .update_tool_with_output(&outcome.call_id, status, Some(&output));
            if outcome.had_secret {
                app.chat.push_notice(crate::secrets::SECRET_WARNING);
            }
            app.scroll = usize::MAX;

            // The agent sees the same masked transcript the TUI shows.
            let (masked_command, _) = app.secrets.mask(&outcome.command);
            let mut content = format!("! {masked_command}\n");
            if let Some(error) = &outcome.error {
                let _ = write!(content, "\n<error>{error}</error>");
            } else {
                if !outcome.stdout.is_empty() {
                    let _ = write!(content, "\n<stdout>\n{}\n</stdout>", outcome.stdout);
                }
                if !outcome.stderr.is_empty() {
                    let _ = write!(content, "\n<stderr>\n{}\n</stderr>", outcome.stderr);
                }
                if outcome.exit_code != 0 {
                    let _ = write!(content, "\n<exit>{}</exit>", outcome.exit_code);
                }
            }
            let msg = super::queue::PendingMessage {
                content,
                hidden_context_xml: None,
                attachments: Vec::new(),
                queued_at: jiff::Timestamp::now(),
                delivery_mode: crate::session::PendingUserMessageMode::Steer,
                display: Some(outcome.call_id),
            };
            Ok(Some(if app.processing {
                SendAction::EnqueuePending(msg)
            } else {
                SendAction::Send(msg)
            }))
        }
        AppEvent::SecretStored { key, result } => {
            match result {
                Ok(()) => {
                    // Refresh the in-memory snapshot so new bash runs mask
                    // and inject the secret just stored.
                    app.secrets = crate::secrets::Secrets::load(
                        &crate::secrets::SecretStore::new(app.session_manager.may_dir()),
                        &app.config,
                    );
                    app.chat.push_notice(&format!(
                        "Secret {key} stored in the OS keychain — injected into shell environments"
                    ));
                }
                Err(error) => app
                    .chat
                    .push_error(&format!("Failed to store secret: {error}")),
            }
            Ok(None)
        }
    }
}

/// Rows of the jobs panel: running subagents first, then registry jobs
/// (running before finished).
pub fn jobs_rows(app: &App) -> Vec<JobRow> {
    let mut rows: Vec<JobRow> = app
        .subagents
        .iter()
        .cloned()
        .map(JobRow::Subagent)
        .collect();
    let mut jobs: Vec<aither_sandbox::JobInfo> = app.jobs.clone();
    jobs.sort_by_key(|job| !matches!(job.status, aither_sandbox::job_registry::JobStatus::Running));
    rows.extend(jobs.into_iter().map(JobRow::Job));
    rows
}

/// One selectable row in the jobs panel.
#[derive(Debug)]
pub enum JobRow {
    /// A `subagent` IPC call in flight, with live counters.
    Subagent(crate::agent::SubagentRunSnapshot),
    /// A background terminal job from the registry.
    Job(aither_sandbox::JobInfo),
}

impl JobRow {
    /// `JobDetailTarget` selecting this row's detail view.
    #[must_use]
    pub const fn detail_target(&self) -> super::state::JobDetailTarget {
        match self {
            Self::Subagent(run) => super::state::JobDetailTarget::Subagent(run.run_id),
            Self::Job(job) => super::state::JobDetailTarget::Job(job.pid),
        }
    }
}
