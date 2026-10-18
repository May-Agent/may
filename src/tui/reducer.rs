//! Event reducer for top-level `AppEvent` handling.

pub(super) mod ask_user;
pub(super) mod manage;
pub(super) mod navigation;

use super::app::{App, SendAction};
use super::error::AppResult;
use super::event::AppEvent;
use super::state::AppMode;

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
            if app.processing {
                app.spinner_frame = app.spinner_frame.wrapping_add(1);
                app.chat.tick_loading();
                if let Some(start) = app.thinking_start {
                    let secs = start.elapsed().as_secs();
                    app.chat.update_thinking(secs);
                }
                // Bind running terminal calls to their jobs and poll live
                // output + file-diff previews.
                app.poll_live_output();
            }
            // Live-refresh the jobs list once per second while the panel is
            // open — and while anything runs in the background so the status
            // bar reflects it. Also refresh while the agent processes so
            // live-output binding sees fresh jobs.
            let jobs_wanted = matches!(app.mode, AppMode::Jobs | AppMode::JobDetail(_))
                || app.processing
                || app.has_running_background();
            if jobs_wanted
                && app
                    .jobs_refreshed
                    .is_none_or(|t| t.elapsed().as_secs() >= 1)
            {
                app.refresh_jobs();
            }
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
                app.status = Some("Config save failed".to_string());
            }
            Ok(None)
        }
        AppEvent::ImageDecoded { key, state } => {
            app.handle_image_decoded(key, state);
            Ok(None)
        }

        AppEvent::JobsLoaded(jobs) => {
            app.jobs = jobs;
            let row_count = jobs_rows(app).len();
            if app.jobs_selected >= row_count && app.jobs_selected > 0 {
                app.jobs_selected = row_count.saturating_sub(1);
            }
            Ok(None)
        }
        AppEvent::JobKilled { pid, ok } => {
            app.status = Some(if ok {
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
    }
}

/// Rows of the jobs panel: running subagents first, then registry jobs
/// (running before finished).
pub fn jobs_rows(app: &App) -> Vec<JobRow> {
    let mut rows: Vec<JobRow> = app
        .subagent_tracker
        .running()
        .into_iter()
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
