use anyhow::Result;
use crossterm::event::KeyCode;

use crate::tui::app::{App, SendAction};
use crate::tui::state::AppMode;

pub(crate) fn handle_session_select_key(
    app: &mut App,
    code: KeyCode,
) -> Result<Option<SendAction>> {
    let filtered = app.filtered_sessions();
    let filtered_len = filtered.len();

    match code {
        KeyCode::Esc => {
            if !app.session_search.is_empty() {
                app.session_search.clear();
                app.session_selected = 0;
                app.session_scroll = 0;
            } else {
                app.mode = AppMode::Chat;
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
                && let Some(summary) = app.session_list.get(*original_idx) {
                    let id = summary.id.clone();
                    app.mode = AppMode::Chat;
                    return Ok(Some(SendAction::LoadSession(id)));
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
                        return Ok(None);
                    }
                    'j' => {
                        if app.session_selected + 1 < filtered_len {
                            app.session_selected += 1;
                        }
                        return Ok(None);
                    }
                    'n' => {
                        app.mode = AppMode::Chat;
                        return Ok(Some(SendAction::NewSession));
                    }
                    'd' => {
                        if let Some((original_idx, _)) = filtered.get(app.session_selected)
                            && let Some(summary) = app.session_list.get(*original_idx) {
                                let deleted_id = summary.id.as_str().to_string();
                                let was_current = app
                                    .session_manager
                                    .current()
                                    .map(|s| s.id.as_str() == deleted_id)
                                    .unwrap_or(false);

                                let tx = app.event_tx.clone();
                                let db = app.session_manager.db().clone();
                                let store = app.session_manager.store().clone();
                                executor_core::spawn(async move {
                                    // Delete from both DB and filesystem
                                    if let Err(e) = db.delete_session(&deleted_id).await {
                                        let _ = tx
                                            .send(crate::tui::event::AppEvent::SessionDeleted {
                                                was_current,
                                                result: Err(anyhow::Error::from(e)),
                                            })
                                            .await;
                                        return;
                                    }
                                    let _ = store.delete_session(&deleted_id).await;
                                    let result =
                                        crate::session::list_summaries_from_db(&db).await;
                                    let _ = tx
                                        .send(crate::tui::event::AppEvent::SessionDeleted {
                                            was_current,
                                            result: result.map_err(anyhow::Error::from),
                                        })
                                        .await;
                                })
                                .detach();
                                app.status = Some("Deleting session...".to_string());
                            }
                        return Ok(None);
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

    Ok(None)
}

pub(crate) fn handle_help_key(app: &mut App, code: KeyCode) -> Result<Option<SendAction>> {
    match code {
        KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
            app.mode = AppMode::Chat;
        }
        _ => {}
    }
    Ok(None)
}

pub(crate) fn handle_tool_detail_key(app: &mut App, code: KeyCode) -> Result<Option<SendAction>> {
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
    Ok(None)
}

pub(crate) fn handle_file_picker_key(app: &mut App, code: KeyCode) -> Result<Option<SendAction>> {
    let Some(picker) = app.file_picker.as_mut() else {
        app.mode = AppMode::Chat;
        return Ok(None);
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

    Ok(None)
}
