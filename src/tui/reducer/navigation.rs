use crossterm::event::KeyCode;

use crate::tui::app::{App, SendAction};
use crate::tui::error::TuiError;
use crate::tui::state::AppMode;

pub fn handle_session_select_key(app: &mut App, code: KeyCode) -> Option<SendAction> {
    let filtered = app.filtered_sessions();
    let filtered_len = filtered.len();

    match code {
        KeyCode::Esc => {
            if app.session_search.is_empty() {
                app.mode = AppMode::Chat;
            } else {
                app.session_search.clear();
                app.session_selected = 0;
                app.session_scroll = 0;
            }
        }
        KeyCode::Up => {
            if app.session_selected > 0 {
                app.session_selected -= 1;
            }
        }
        KeyCode::Down => {
            if app.session_selected + 1 < filtered_len {
                app.session_selected += 1;
            }
        }
        KeyCode::Enter => {
            if let Some((original_idx, _)) = filtered.get(app.session_selected)
                && let Some(summary) = app.session_list.get(*original_idx)
            {
                let id = summary.id.clone();
                app.mode = AppMode::Chat;
                return Some(SendAction::LoadSession(id));
            }
        }
        KeyCode::Tab => {
            // Cycle through profiles (if multiple exist)
            if app.profiles.len() > 1 && app.session_search.is_empty() {
                let current_idx = app
                    .profiles
                    .iter()
                    .position(|p| Some(p.id.as_str()) == app.current_profile_id.as_deref())
                    .unwrap_or(0);
                let next_idx = (current_idx + 1) % app.profiles.len();
                set_active_profile(app, app.profiles[next_idx].id.clone());
                app.archived_expanded = false;
                app.refresh_profile_sessions();
                app.set_status("Switching profile...");
            }
        }
        KeyCode::BackTab => {
            // Cycle profiles backward
            if app.profiles.len() > 1 && app.session_search.is_empty() {
                let current_idx = app
                    .profiles
                    .iter()
                    .position(|p| Some(p.id.as_str()) == app.current_profile_id.as_deref())
                    .unwrap_or(0);
                let next_idx = if current_idx == 0 {
                    app.profiles.len() - 1
                } else {
                    current_idx - 1
                };
                set_active_profile(app, app.profiles[next_idx].id.clone());
                app.archived_expanded = false;
                app.refresh_profile_sessions();
                app.set_status("Switching profile...");
            }
        }
        KeyCode::Backspace => {
            app.session_search.pop();
            app.session_selected = 0;
            app.session_scroll = 0;
        }
        KeyCode::Char(c) => {
            if app.session_search.is_empty() {
                match c {
                    'k' => {
                        if app.session_selected > 0 {
                            app.session_selected -= 1;
                        }
                        return None;
                    }
                    'j' => {
                        if app.session_selected + 1 < filtered_len {
                            app.session_selected += 1;
                        }
                        return None;
                    }
                    'n' => {
                        app.mode = AppMode::Chat;
                        return Some(SendAction::NewSession);
                    }
                    'p' => {
                        // Toggle pin/unpin for selected session
                        toggle_pin(app);
                        return None;
                    }
                    'a' => {
                        // Toggle archive/unarchive for selected session
                        toggle_archive(app);
                        return None;
                    }
                    'e' => {
                        // Toggle archived section expansion
                        if app.profile_sessions.is_some() {
                            app.archived_expanded = !app.archived_expanded;
                            app.build_session_list_from_profile();
                            // Clamp selection if it went out of range
                            if app.session_selected >= app.session_list.len() {
                                app.session_selected = app.session_list.len().saturating_sub(1);
                            }
                        }
                        return None;
                    }
                    'd' => {
                        if let Some((original_idx, _)) = filtered.get(app.session_selected)
                            && let Some(summary) = app.session_list.get(*original_idx)
                        {
                            let deleted_id = summary.id.as_str().to_string();
                            let was_current = app
                                .session_manager
                                .current()
                                .is_some_and(|s| s.id.as_str() == deleted_id);

                            let tx = app.event_tx.clone();
                            let fallback_profile = app.current_profile_id.clone();
                            let client = app.btw_daemon.clone()?;
                            executor_core::spawn(async move {
                                // The daemon removes the row and the session
                                // directory.
                                let profile_id = client
                                    .get::<crate::client::WireSession>(&format!(
                                        "/api/sessions/{deleted_id}"
                                    ))
                                    .await
                                    .ok()
                                    .and_then(|session| session.profile_id);
                                if let Err(error) = client
                                    .delete::<serde_json::Value>(&format!(
                                        "/api/sessions/{deleted_id}"
                                    ))
                                    .await
                                {
                                    let _ = tx
                                        .send(crate::tui::event::AppEvent::SessionDeleted {
                                            deleted: None,
                                            was_current,
                                            result: Err(TuiError::from(error)),
                                        })
                                        .await;
                                    return;
                                }
                                let result = async {
                                    let Some(pid) = profile_id.or(fallback_profile) else {
                                        return Ok(Vec::new());
                                    };
                                    let sessions: crate::profile::ProfileSessions = client
                                        .get(&format!("/api/profiles/{pid}/sessions"))
                                        .await
                                        .map_err(TuiError::from)?;
                                    Ok(sessions
                                        .pinned
                                        .into_iter()
                                        .flat_map(|item| match item {
                                            crate::profile::PinnedItem::Session(summary) => {
                                                vec![*summary]
                                            }
                                            crate::profile::PinnedItem::Folder {
                                                sessions, ..
                                            } => sessions,
                                        })
                                        .chain(sessions.regular)
                                        .chain(sessions.archived)
                                        .collect::<Vec<_>>())
                                }
                                .await;
                                let _ = tx
                                    .send(crate::tui::event::AppEvent::SessionDeleted {
                                        deleted: Some(crate::session::SessionId::from_string(
                                            deleted_id,
                                        )),
                                        was_current,
                                        result,
                                    })
                                    .await;
                            })
                            .detach();
                            app.set_status("Deleting session...");
                        }
                        return None;
                    }
                    _ => {}
                }
            }
            app.session_search.push(c);
            app.session_selected = 0;
            app.session_scroll = 0;
        }
        _ => {}
    }

    None
}

/// Toggle pin/unpin for the currently selected session.
fn toggle_pin(app: &mut App) {
    let filtered = app.filtered_sessions();
    let Some((original_idx, _)) = filtered.get(app.session_selected) else {
        return;
    };
    let Some(summary) = app.session_list.get(*original_idx) else {
        return;
    };

    let session_id = summary.id.as_str().to_string();
    let is_pinned = summary.pinned;
    let tx = app.event_tx.clone();
    let Some(client) = app.btw_daemon.clone() else {
        return;
    };
    let profile_id = app.current_profile_id.clone();

    executor_core::spawn(async move {
        let action = if is_pinned { "unpin" } else { "pin" };
        if let Err(e) = client
            .post::<_, serde_json::Value>(
                &format!("/api/sessions/{session_id}/{action}"),
                &serde_json::json!({}),
            )
            .await
        {
            tracing::warn!("Failed to toggle pin for session {}: {}", session_id, e);
            return;
        }
        // Reload profile sessions to reflect the change
        if let Some(pid) = profile_id {
            let result = client
                .get::<crate::profile::ProfileSessions>(&format!("/api/profiles/{pid}/sessions"))
                .await;
            let _ = tx
                .send(crate::tui::event::AppEvent::ProfileSessionsLoaded(
                    result.map_err(TuiError::from),
                ))
                .await;
        }
    })
    .detach();
    app.set_status(if is_pinned {
        "Unpinning session..."
    } else {
        "Pinning session..."
    });
}

/// Toggle archive/unarchive for the currently selected session.
fn toggle_archive(app: &mut App) {
    let filtered = app.filtered_sessions();
    let Some((original_idx, _)) = filtered.get(app.session_selected) else {
        return;
    };
    let Some(summary) = app.session_list.get(*original_idx) else {
        return;
    };

    let session_id = summary.id.as_str().to_string();
    let is_archived = summary.archived;
    let tx = app.event_tx.clone();
    let Some(client) = app.btw_daemon.clone() else {
        return;
    };
    let profile_id = app.current_profile_id.clone();

    executor_core::spawn(async move {
        let action = if is_archived { "unarchive" } else { "archive" };
        if let Err(e) = client
            .post::<_, serde_json::Value>(
                &format!("/api/sessions/{session_id}/{action}"),
                &serde_json::json!({}),
            )
            .await
        {
            tracing::warn!("Failed to toggle archive for session {}: {}", session_id, e);
            return;
        }
        // Reload profile sessions to reflect the change
        if let Some(pid) = profile_id {
            let result = client
                .get::<crate::profile::ProfileSessions>(&format!("/api/profiles/{pid}/sessions"))
                .await;
            let _ = tx
                .send(crate::tui::event::AppEvent::ProfileSessionsLoaded(
                    result.map_err(TuiError::from),
                ))
                .await;
        }
    })
    .detach();
    app.set_status(if is_archived {
        "Unarchiving session..."
    } else {
        "Archiving session..."
    });
}

pub fn handle_profile_select_key(app: &mut App, code: KeyCode) -> Option<SendAction> {
    let profile_count = app.profiles.len();

    match code {
        KeyCode::Esc => {
            app.mode = AppMode::Chat;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.profile_selected > 0 {
                app.profile_selected -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.profile_selected + 1 < profile_count {
                app.profile_selected += 1;
            }
        }
        KeyCode::Enter => {
            if let Some(profile) = app.profiles.get(app.profile_selected) {
                set_active_profile(app, profile.id.clone());
                app.archived_expanded = false;
                // Load sessions for selected profile then switch to SessionSelect
                app.load_session_list();
            }
        }
        _ => {}
    }

    None
}

fn set_active_profile(app: &mut App, profile_id: String) {
    app.current_profile_id = Some(profile_id.clone());
    app.session_manager.set_current_profile(profile_id);
}

pub fn handle_help_key(app: &mut App, code: KeyCode) -> Option<SendAction> {
    match code {
        KeyCode::Esc | KeyCode::Char('?' | 'q') => {
            app.mode = AppMode::Chat;
        }
        KeyCode::Up | KeyCode::Char('k') => app.help_scroll = app.help_scroll.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => app.help_scroll = app.help_scroll.saturating_add(1),
        KeyCode::PageUp => app.help_scroll = app.help_scroll.saturating_sub(10),
        KeyCode::PageDown => app.help_scroll = app.help_scroll.saturating_add(10),
        _ => {}
    }
    None
}

/// Number of restoration scopes the rewind picker offers.
const REWIND_OPTIONS: usize = 2;

/// Rewind picker: pick a prompt, then the restoration scope.
pub fn handle_rewind_key(app: &mut App, code: KeyCode) -> Option<SendAction> {
    let AppMode::Rewind(state) = &mut app.mode else {
        return None;
    };
    if let Some(prompt_index) = state.options_for {
        // Scope submenu: conversation only, or conversation and code.
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                state.option_selected = state.option_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if state.option_selected + 1 < REWIND_OPTIONS {
                    state.option_selected += 1;
                }
            }
            KeyCode::Enter => {
                let restore_code = state.option_selected == 1;
                let history_index = state.targets[prompt_index].history_index;
                app.mode = AppMode::Chat;
                return Some(SendAction::Rewind {
                    history_index,
                    restore_code,
                });
            }
            KeyCode::Esc => {
                state.options_for = None;
                state.option_selected = 0;
            }
            _ => {}
        }
        return None;
    }
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            state.selected = state.selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            // `targets.len()` is the trailing "(current)" entry.
            if state.selected < state.targets.len() {
                state.selected += 1;
            }
        }
        KeyCode::Enter => {
            if state.selected == state.targets.len() {
                // "(current)": no rewind, just close.
                app.mode = AppMode::Chat;
            } else {
                state.options_for = Some(state.selected);
                state.option_selected = 0;
            }
        }
        KeyCode::Esc => {
            app.mode = AppMode::Chat;
        }
        _ => {}
    }
    None
}

pub fn handle_tool_detail_key(app: &mut App, code: KeyCode) -> Option<SendAction> {
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = AppMode::Chat;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.tool_detail_scroll = app.tool_detail_scroll.saturating_add(1);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.tool_detail_scroll = app.tool_detail_scroll.saturating_sub(1);
        }
        KeyCode::Char('G') => {
            app.tool_detail_scroll = usize::MAX;
        }
        KeyCode::Char('g') => {
            app.tool_detail_scroll = 0;
        }
        _ => {}
    }
    None
}

pub fn handle_file_picker_key(app: &mut App, code: KeyCode) -> Option<SendAction> {
    let Some(picker) = app.file_picker.as_mut() else {
        app.mode = AppMode::Chat;
        return None;
    };

    match code {
        KeyCode::Esc => {
            app.file_picker = None;
            app.mode = AppMode::Chat;
        }
        KeyCode::Enter => {
            if let Some(path) = picker.confirm() {
                app.attachments
                    .push(crate::tui::queue::Attachment::FileReference { path });
            }
            app.file_picker = None;
            app.mode = AppMode::Chat;
        }
        KeyCode::Up => picker.select_prev(),
        KeyCode::Down => picker.select_next(),
        KeyCode::Backspace => picker.delete_char(),
        KeyCode::Char(c) => picker.insert_char(c),
        _ => {}
    }

    None
}

/// Jobs panel: navigate, open the selected row's detail, stop, refresh, close.
pub fn handle_jobs_key(app: &mut App, code: KeyCode) -> Option<SendAction> {
    let row_count = super::jobs_rows(app).len();
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.jobs_selected = app.jobs_selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.jobs_selected + 1 < row_count {
                app.jobs_selected += 1;
            }
        }
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
            let rows = super::jobs_rows(app);
            if let Some(row) = rows.get(app.jobs_selected) {
                app.open_job_detail(row.detail_target());
            }
        }
        KeyCode::Char('x') => {
            let rows = super::jobs_rows(app);
            match rows.get(app.jobs_selected) {
                Some(super::JobRow::Job(job)) => match job.status {
                    aither_sandbox::JobStatus::Running => app.kill_job(job.pid),
                    _ => app.set_status(format!("Job {} already finished", job.pid)),
                },
                Some(super::JobRow::Subagent(run)) => app.cancel_subagent(run),
                None => {}
            }
        }
        KeyCode::Char('r') => app.refresh_jobs(),
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = AppMode::Chat;
        }
        _ => {}
    }
    None
}

/// Keys inside the `/btw` overlay: type a question, Enter asks it, then the
/// answer scrolls; Esc/q always closes the overlay back to chat.
pub fn handle_btw_key(app: &mut App, code: KeyCode) -> Option<SendAction> {
    let AppMode::Btw(state) = &mut app.mode else {
        return None;
    };
    let awaiting_question = !state.loading && !state.done;
    match code {
        KeyCode::Esc | KeyCode::Char('q') if !awaiting_question => {
            app.mode = AppMode::Chat;
        }
        KeyCode::Esc => {
            app.mode = AppMode::Chat;
        }
        KeyCode::Enter if awaiting_question => app.submit_btw(),
        KeyCode::Char(c) if awaiting_question => state.input.push(c),
        KeyCode::Backspace if awaiting_question => {
            state.input.pop();
        }
        KeyCode::Up | KeyCode::Char('k') if !awaiting_question => {
            state.scroll = state.scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') if !awaiting_question => {
            state.scroll = state.scroll.saturating_add(1);
        }
        _ => {}
    }
    None
}

/// Zoomed image overlay: Esc closes it; a click outside the image is handled
/// by the mouse handler.
pub fn handle_image_view_key(app: &mut App, code: KeyCode) -> Option<SendAction> {
    if code == KeyCode::Esc {
        app.mode = AppMode::Chat;
        app.image_view_area = None;
    }
    None
}

/// Job detail view: scroll output, stop a running job, refresh, back to Jobs.
pub fn handle_job_detail_key(app: &mut App, code: KeyCode) -> Option<SendAction> {
    let AppMode::JobDetail(state) = &mut app.mode else {
        return None;
    };
    match code {
        KeyCode::Esc | KeyCode::Char('q' | 'h') | KeyCode::Left => {
            app.mode = AppMode::Jobs;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.scroll = state.scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.scroll += 1;
        }
        KeyCode::Char('g') => state.scroll = 0,
        KeyCode::Char('G') => state.scroll = usize::MAX,
        KeyCode::Char('r') => app.refresh_job_detail(),
        KeyCode::Char('x') => {
            if let crate::tui::state::JobDetailTarget::Job(pid) = state.target {
                match state.job.as_ref().map(|j| &j.status) {
                    Some(aither_sandbox::JobStatus::Running) => app.kill_job(pid),
                    Some(_) => app.set_status(format!("Job {pid} already finished")),
                    None => {}
                }
            } else if let Some(run) = state.subagent.clone() {
                app.cancel_subagent(&run);
            }
        }
        _ => {}
    }
    None
}
