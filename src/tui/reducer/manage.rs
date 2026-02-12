use anyhow::Result;
use crossterm::event::KeyCode;

use crate::tui::app::{App, SendAction};
use crate::tui::state::{CopilotAuthStatus, McpAddMode, SkillAddMode, SshAddMode};

pub(crate) fn handle_copilot_auth_key(app: &mut App, code: KeyCode) -> Result<Option<SendAction>> {
    match code {
        KeyCode::Esc => {
            app.copilot_auth_status = None;
            app.mode = crate::tui::state::AppMode::Config;
        }
        KeyCode::Enter => {
            if let Some(CopilotAuthStatus::WaitingForUser {
                user_code,
                verification_uri,
            }) = &app.copilot_auth_status
            {
                return Ok(Some(SendAction::OpenCopilotAuthUrl {
                    url: verification_uri.clone(),
                    code: user_code.clone(),
                }));
            }
            if matches!(
                app.copilot_auth_status,
                Some(CopilotAuthStatus::Success | CopilotAuthStatus::Failed(_))
            ) {
                app.copilot_auth_status = None;
                app.mode = crate::tui::state::AppMode::Config;
            }
        }
        _ => {}
    }
    Ok(None)
}

pub(crate) fn handle_workspace_confirm_key(
    app: &mut App,
    code: KeyCode,
) -> Result<Option<SendAction>> {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            let mode = std::mem::replace(&mut app.mode, crate::tui::state::AppMode::Chat);
            if let crate::tui::state::AppMode::WorkspaceConfirm(request) = mode {
                request.respond(true).unwrap_or_else(|_| {
                    panic!("{}", super::super::app::ERR_TOOL_RESPONSE_SEND.trim())
                });
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            let mode = std::mem::replace(&mut app.mode, crate::tui::state::AppMode::Chat);
            if let crate::tui::state::AppMode::WorkspaceConfirm(request) = mode {
                request.respond(false).unwrap_or_else(|_| {
                    panic!("{}", super::super::app::ERR_TOOL_RESPONSE_SEND.trim())
                });
            }
        }
        _ => {}
    }
    Ok(None)
}

pub(crate) fn handle_mcp_key(app: &mut App, code: KeyCode) -> Result<Option<SendAction>> {
    match app.mcp_add_mode {
        McpAddMode::None => match code {
            KeyCode::Esc => {
                app.mode = crate::tui::state::AppMode::Chat;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if app.mcp_selected > 0 {
                    app.mcp_selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.mcp_selected + 1 < app.mcp_servers.len() {
                    app.mcp_selected += 1;
                }
            }
            KeyCode::Char('a') => {
                app.mcp_add_mode = McpAddMode::Name;
                app.mcp_add_name.clear();
                app.mcp_add_command.clear();
            }
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                if let Some(server) = app.mcp_servers.get(app.mcp_selected) {
                    let name = server.name.clone();
                    return Ok(Some(SendAction::McpRemove { name }));
                }
            }
            KeyCode::Char('r') => {
                if let Some(server) = app.mcp_servers.get(app.mcp_selected) {
                    let name = server.name.clone();
                    return Ok(Some(SendAction::McpReconnect { name }));
                }
            }
            _ => {}
        },
        McpAddMode::Name => match code {
            KeyCode::Esc => {
                app.mcp_add_mode = McpAddMode::None;
            }
            KeyCode::Enter => {
                if !app.mcp_add_name.is_empty() {
                    app.mcp_add_mode = McpAddMode::Command;
                }
            }
            KeyCode::Backspace => {
                app.mcp_add_name.pop();
            }
            KeyCode::Char(c) => {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    app.mcp_add_name.push(c);
                }
            }
            _ => {}
        },
        McpAddMode::Command => match code {
            KeyCode::Esc => {
                app.mcp_add_mode = McpAddMode::Name;
            }
            KeyCode::Enter => {
                if !app.mcp_add_command.is_empty() {
                    let parts: Vec<&str> = app.mcp_add_command.split_whitespace().collect();
                    if let Some((command, args)) = parts.split_first() {
                        let name = std::mem::take(&mut app.mcp_add_name);
                        let command_str = (*command).to_string();
                        let args_vec: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
                        app.mcp_add_mode = McpAddMode::None;
                        app.mcp_add_command.clear();
                        return Ok(Some(SendAction::McpAdd {
                            name,
                            command: command_str,
                            args: args_vec,
                        }));
                    }
                }
            }
            KeyCode::Backspace => {
                app.mcp_add_command.pop();
            }
            KeyCode::Char(c) => {
                app.mcp_add_command.push(c);
            }
            _ => {}
        },
    }
    Ok(None)
}

pub(crate) fn handle_ssh_key(app: &mut App, code: KeyCode) -> Result<Option<SendAction>> {
    match app.ssh_add_mode {
        SshAddMode::None => match code {
            KeyCode::Esc => {
                app.mode = crate::tui::state::AppMode::Chat;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if app.ssh_selected > 0 {
                    app.ssh_selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.ssh_selected + 1 < app.config.ssh_servers.len() {
                    app.ssh_selected += 1;
                }
            }
            KeyCode::Char('a') => {
                app.ssh_add_mode = SshAddMode::Name;
                app.ssh_add_name.clear();
                app.ssh_add_target.clear();
            }
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                if let Some(server) = app.config.ssh_servers.get(app.ssh_selected) {
                    return Ok(Some(SendAction::SshRemove {
                        name: server.name.clone(),
                    }));
                }
            }
            _ => {}
        },
        SshAddMode::Name => match code {
            KeyCode::Esc => app.ssh_add_mode = SshAddMode::None,
            KeyCode::Enter => {
                if !app.ssh_add_name.is_empty() {
                    app.ssh_add_mode = SshAddMode::Target;
                }
            }
            KeyCode::Backspace => {
                app.ssh_add_name.pop();
            }
            KeyCode::Char(c) => {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    app.ssh_add_name.push(c);
                }
            }
            _ => {}
        },
        SshAddMode::Target => match code {
            KeyCode::Esc => app.ssh_add_mode = SshAddMode::Name,
            KeyCode::Enter => {
                let target = app.ssh_add_target.trim().to_string();
                let name = app.ssh_add_name.trim().to_string();
                if !name.is_empty() && !target.is_empty() && target.contains('@') {
                    app.ssh_add_mode = SshAddMode::None;
                    app.ssh_add_name.clear();
                    app.ssh_add_target.clear();
                    return Ok(Some(SendAction::SshAdd { name, target }));
                }
            }
            KeyCode::Backspace => {
                app.ssh_add_target.pop();
            }
            KeyCode::Char(c) => {
                app.ssh_add_target.push(c);
            }
            _ => {}
        },
    }
    Ok(None)
}

pub(crate) fn handle_skill_key(app: &mut App, code: KeyCode) -> Result<Option<SendAction>> {
    match app.skill_add_mode {
        SkillAddMode::None => match code {
            KeyCode::Esc => {
                if !app.skill_search.is_empty() {
                    app.skill_search.clear();
                    app.skill_selected = 0;
                    app.skill_scroll = 0;
                } else {
                    app.mode = crate::tui::state::AppMode::Chat;
                    app.skill_search.clear();
                    app.skill_scroll = 0;
                }
            }
            KeyCode::Up => {
                if app.skill_selected > 0 {
                    app.skill_selected -= 1;
                    if app.skill_selected < app.skill_scroll {
                        app.skill_scroll = app.skill_selected;
                    }
                }
            }
            KeyCode::Down => {
                let filtered_len = app.filtered_skills().len();
                if app.skill_selected + 1 < filtered_len {
                    app.skill_selected += 1;
                }
            }
            KeyCode::Backspace => {
                app.skill_search.pop();
                app.skill_selected = 0;
                app.skill_scroll = 0;
            }
            KeyCode::Char('a') if app.skill_search.is_empty() => {
                app.skill_add_mode = SkillAddMode::GitHubRepo;
                app.skill_github_input.clear();
            }
            KeyCode::Char('d') if app.skill_search.is_empty() => {
                let filtered = app.filtered_skills();
                if let Some(skill) = filtered.get(app.skill_selected) {
                    let dir_name = skill.dir_name.clone();
                    return Ok(Some(SendAction::SkillDelete { dir_name }));
                }
            }
            KeyCode::Delete => {
                let filtered = app.filtered_skills();
                if let Some(skill) = filtered.get(app.skill_selected) {
                    let dir_name = skill.dir_name.clone();
                    return Ok(Some(SendAction::SkillDelete { dir_name }));
                }
            }
            KeyCode::Enter => {
                let filtered = app.filtered_skills();
                if let Some(skill) = filtered.get(app.skill_selected) {
                    let prefix = format!("Use {} skill, ", skill.name);
                    let prefix_len = prefix.chars().count();
                    app.input.insert_str(0, &prefix);
                    app.cursor += prefix_len;
                    app.mode = crate::tui::state::AppMode::Chat;
                    app.skill_search.clear();
                    app.skill_scroll = 0;
                }
            }
            KeyCode::Char(c) => {
                app.skill_search.push(c);
                app.skill_selected = 0;
                app.skill_scroll = 0;
            }
            _ => {}
        },
        SkillAddMode::GitHubRepo => match code {
            KeyCode::Esc => {
                app.skill_add_mode = SkillAddMode::None;
                app.skill_github_input.clear();
            }
            KeyCode::Enter => {
                if !app.skill_github_input.is_empty() {
                    let input = std::mem::take(&mut app.skill_github_input);
                    app.skill_add_mode = SkillAddMode::None;
                    return Ok(Some(SendAction::SkillInstall { repo: input }));
                }
            }
            KeyCode::Backspace => {
                app.skill_github_input.pop();
            }
            KeyCode::Char(c) => {
                if c.is_alphanumeric() || c == '-' || c == '_' || c == '/' {
                    app.skill_github_input.push(c);
                }
            }
            _ => {}
        },
    }
    Ok(None)
}
