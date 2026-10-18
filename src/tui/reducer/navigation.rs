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
                app.status = Some("Switching profile...".to_string());
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
                app.status = Some("Switching profile...".to_string());
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
                            let db = app.session_manager.db().clone();
                            let store = app.session_manager.store().clone();
                            executor_core::spawn(async move {
                                // Delete from both DB and filesystem
                                let profile_id = match db.get_session(&deleted_id).await {
                                    Ok(session) => session.and_then(|session| session.profile_id),
                                    Err(error) => {
                                        let _ = tx
                                            .send(crate::tui::event::AppEvent::SessionDeleted {
                                                deleted: None,
                                                was_current,
                                                result: Err(TuiError::from(error)),
                                            })
                                            .await;
                                        return;
                                    }
                                };
                                if let Err(e) = db.delete_session(&deleted_id).await {
                                    let _ = tx
                                        .send(crate::tui::event::AppEvent::SessionDeleted {
                                            deleted: None,
                                            was_current,
                                            result: Err(TuiError::from(e)),
                                        })
                                        .await;
                                    return;
                                }
                                let _ = store
                                    .delete_session(&deleted_id, profile_id.as_deref())
                                    .await;
                                let result = crate::session::list_summaries_from_db(&db).await;
                                let _ = tx
                                    .send(crate::tui::event::AppEvent::SessionDeleted {
                                        deleted: Some(crate::session::SessionId::from_string(
                                            deleted_id,
                                        )),
                                        was_current,
                                        result: result.map_err(TuiError::from),
                                    })
                                    .await;
                            })
                            .detach();
                            app.status = Some("Deleting session...".to_string());
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
    let db = app.session_manager.db().clone();
    let profile_id = app.current_profile_id.clone();

    executor_core::spawn(async move {
        let result = if is_pinned {
            db.unpin_session(&session_id).await
        } else {
            db.pin_session(&session_id).await
        };
        if let Err(e) = result {
            tracing::warn!("Failed to toggle pin for session {}: {}", session_id, e);
            return;
        }
        // Reload profile sessions to reflect the change
        if let Some(pid) = profile_id {
            let result = crate::profile::query_profile_sessions(&db, &pid).await;
            let _ = tx
                .send(crate::tui::event::AppEvent::ProfileSessionsLoaded(
                    result.map_err(TuiError::from),
                ))
                .await;
        }
    })
    .detach();
    app.status = Some(if is_pinned {
        "Unpinning session...".to_string()
    } else {
        "Pinning session...".to_string()
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
    let db = app.session_manager.db().clone();
    let profile_id = app.current_profile_id.clone();

    executor_core::spawn(async move {
        let result = if is_archived {
            db.unarchive_session(&session_id).await
        } else {
            db.archive_session(&session_id).await
        };
        if let Err(e) = result {
            tracing::warn!("Failed to toggle archive for session {}: {}", session_id, e);
            return;
        }
        // Reload profile sessions to reflect the change
        if let Some(pid) = profile_id {
            let result = crate::profile::query_profile_sessions(&db, &pid).await;
            let _ = tx
                .send(crate::tui::event::AppEvent::ProfileSessionsLoaded(
                    result.map_err(TuiError::from),
                ))
                .await;
        }
    })
    .detach();
    app.status = Some(if is_archived {
        "Unarchiving session...".to_string()
    } else {
        "Archiving session...".to_string()
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
        _ => {}
    }
    None
}

/// Rewind picker: select a checkpoint and restore files to it.
pub fn handle_rewind_key(app: &mut App, code: KeyCode) -> Option<SendAction> {
    let AppMode::Rewind(state) = &mut app.mode else {
        return None;
    };
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            state.selected = state.selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if state.selected + 1 < state.targets.len() {
                state.selected += 1;
            }
        }
        KeyCode::Enter => {
            let target = state.targets[state.selected].tool_call_id.clone();
            app.mode = AppMode::Chat;
            if let Some(handle) = &app.file_diffs {
                handle.request_rewind(&target);
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
