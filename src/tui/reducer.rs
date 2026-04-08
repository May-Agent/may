//! Event reducer for top-level `AppEvent` handling.

pub(super) mod ask_user;
pub(super) mod manage;
pub(super) mod navigation;

use super::app::{App, SendAction};
use super::error::AppResult;
use super::event::AppEvent;
use super::state::AppMode;

/// Reduce a single UI event into state transitions and optional actions.
pub fn reduce_event(app: &mut App, event: AppEvent) -> AppResult<Option<SendAction>> {
    match event {
        AppEvent::Key(code, modifiers) => app.handle_key(code, modifiers),
        AppEvent::Tick => {
            if app.processing {
                app.chat.tick_loading();
                if let Some(start) = app.thinking_start {
                    let secs = start.elapsed().as_secs();
                    app.chat.update_thinking(secs);
                }
            }
            Ok(None)
        }
        AppEvent::Resize(_, _) => Ok(None),
        AppEvent::Paste(text) => app.handle_bracketed_paste(text),
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
        AppEvent::ImageFetchResult { url, data } => {
            app.handle_image_fetch_result(url, data);
            Ok(None)
        }
    }
}
