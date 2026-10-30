//! MCP / SSH / skill add popups.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use super::super::super::app::App;
use super::super::super::state::{McpAddMode, SkillAddMode, SshAddMode};
use super::centered_rect;

pub(super) fn render_mcp_popup(frame: &mut Frame, area: Rect, app: &App) {
    let popup_area = centered_rect(70, 60, area);

    // Clear background
    frame.render_widget(Clear, popup_area);

    // Title based on mode
    let title = match app.mcp_add_mode {
        McpAddMode::None => " MCP Servers ",
        McpAddMode::Name => " Add MCP Server - Enter Name ",
        McpAddMode::Command => " Add MCP Server - Enter Command ",
    };

    // Build content based on mode
    let content: Vec<Line> = match app.mcp_add_mode {
        McpAddMode::None => {
            let mut lines = Vec::new();

            if app.mcp_servers.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "No MCP servers configured.",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Press 'a' to add a new server.",
                    Style::default().fg(Color::Yellow),
                )));
            } else {
                // List servers
                for (idx, server) in app.mcp_servers.iter().enumerate() {
                    let selected = idx == app.mcp_selected;
                    let prefix = if selected { "> " } else { "  " };

                    let status_color = if server.status.is_connected() {
                        Color::Green
                    } else {
                        Color::Red
                    };

                    let line = Line::from(vec![
                        Span::styled(
                            prefix,
                            if selected {
                                Style::default().fg(Color::Yellow)
                            } else {
                                Style::default()
                            },
                        ),
                        Span::styled(
                            &server.name,
                            if selected {
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(ratatui::style::Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::White)
                            },
                        ),
                        Span::styled(" - ", Style::default().fg(Color::DarkGray)),
                        Span::styled(server.status.to_string(), Style::default().fg(status_color)),
                    ]);
                    lines.push(line);

                    // Show command/URL on second line if selected
                    if selected {
                        lines.push(Line::from(Span::styled(
                            format!("    {}", server.source),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
            }

            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("[a] ", Style::default().fg(Color::Cyan)),
                Span::styled("Add", Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled("[d] ", Style::default().fg(Color::Cyan)),
                Span::styled("Delete", Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled("[r] ", Style::default().fg(Color::Cyan)),
                Span::styled("Reconnect", Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled("[Esc] ", Style::default().fg(Color::Cyan)),
                Span::styled("Close", Style::default().fg(Color::White)),
            ]));

            lines
        }
        McpAddMode::Name => {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Server name (alphanumeric, -, _):",
                    Style::default().fg(Color::White),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    format!("> {}_", app.mcp_add_name),
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "[Enter] Continue  [Esc] Cancel",
                    Style::default().fg(Color::DarkGray),
                )),
            ]
        }
        McpAddMode::Command => {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("Name: {}", app.mcp_add_name),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Command (program and arguments):",
                    Style::default().fg(Color::White),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    format!("> {}_", app.mcp_add_command),
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Example: npx -y @modelcontextprotocol/server-filesystem /path",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Shell quoting is supported.",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "[Enter] Add Server  [Esc] Back",
                    Style::default().fg(Color::DarkGray),
                )),
            ]
        }
    };

    let widget = Paragraph::new(content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Magenta))
                .title(title)
                .title_alignment(ratatui::layout::Alignment::Center),
        )
        .alignment(ratatui::layout::Alignment::Left);

    frame.render_widget(widget, popup_area);

    // IME anchor on the `_` marker of the active `> {text}_` content row.
    let caret = match app.mcp_add_mode {
        McpAddMode::Name => Some((app.mcp_add_name.as_str(), 3)),
        McpAddMode::Command => Some((app.mcp_add_command.as_str(), 5)),
        McpAddMode::None => None,
    };
    if let Some((text, row)) = caret {
        let caret_x =
            popup_area.x + 3 + u16::try_from(UnicodeWidthStr::width(text)).unwrap_or(u16::MAX);
        frame.set_cursor_position((
            caret_x.min(popup_area.right().saturating_sub(2)),
            popup_area.y + 1 + row,
        ));
    }
}

/// Render SSH server management popup.
pub(super) fn render_ssh_popup(frame: &mut Frame, area: Rect, app: &App) {
    let popup_area = centered_rect(70, 60, area);
    frame.render_widget(Clear, popup_area);

    let title = match app.ssh_add_mode {
        SshAddMode::None => " SSH Servers ",
        SshAddMode::Name => " Add SSH Server - Enter Name ",
        SshAddMode::Target => " Add SSH Server - Enter Target ",
    };

    let content: Vec<Line> = match app.ssh_add_mode {
        SshAddMode::None => {
            let mut lines = Vec::new();
            if app.config.ssh_servers.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "No SSH servers configured.",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Press 'a' to add a server.",
                    Style::default().fg(Color::Yellow),
                )));
            } else {
                for (idx, server) in app.config.ssh_servers.iter().enumerate() {
                    let selected = idx == app.ssh_selected;
                    let prefix = if selected { "> " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::styled(
                            prefix,
                            if selected {
                                Style::default().fg(Color::Yellow)
                            } else {
                                Style::default()
                            },
                        ),
                        Span::styled(
                            &server.name,
                            if selected {
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(ratatui::style::Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::White)
                            },
                        ),
                        Span::styled(" -> ", Style::default().fg(Color::DarkGray)),
                        Span::styled(&server.target, Style::default().fg(Color::Cyan)),
                    ]));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("[a] ", Style::default().fg(Color::Cyan)),
                Span::styled("Add", Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled("[d] ", Style::default().fg(Color::Cyan)),
                Span::styled("Delete", Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled("[Esc] ", Style::default().fg(Color::Cyan)),
                Span::styled("Close", Style::default().fg(Color::White)),
            ]));
            lines
        }
        SshAddMode::Name => vec![
            Line::from(""),
            Line::from(Span::styled(
                "Server name (alphanumeric, -, _):",
                Style::default().fg(Color::White),
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!("> {}_", app.ssh_add_name),
                Style::default().fg(Color::Yellow),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "[Enter] Continue  [Esc] Cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ],
        SshAddMode::Target => vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("Name: {}", app.ssh_add_name),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "SSH target (user@host):",
                Style::default().fg(Color::White),
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!("> {}_", app.ssh_add_target),
                Style::default().fg(Color::Yellow),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "[Enter] Save  [Esc] Back",
                Style::default().fg(Color::DarkGray),
            )),
        ],
    };

    let widget = Paragraph::new(content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Magenta))
                .title(title)
                .title_alignment(ratatui::layout::Alignment::Center),
        )
        .alignment(ratatui::layout::Alignment::Left);

    frame.render_widget(widget, popup_area);

    let caret = match app.ssh_add_mode {
        SshAddMode::Name => Some((app.ssh_add_name.as_str(), 3)),
        SshAddMode::Target => Some((app.ssh_add_target.as_str(), 5)),
        SshAddMode::None => None,
    };
    if let Some((text, row)) = caret {
        let caret_x =
            popup_area.x + 3 + u16::try_from(UnicodeWidthStr::width(text)).unwrap_or(u16::MAX);
        frame.set_cursor_position((
            caret_x.min(popup_area.right().saturating_sub(2)),
            popup_area.y + 1 + row,
        ));
    }
}

/// Render the skill management popup.
pub(super) fn render_skill_popup(frame: &mut Frame, area: Rect, app: &App) {
    let popup_area = centered_rect(80, 70, area);

    frame.render_widget(Clear, popup_area);

    let title = match app.skill_add_mode {
        SkillAddMode::None => " Skills ",
        SkillAddMode::GitHubRepo => " Install Skill from GitHub ",
    };

    // Render the border block
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta))
        .title(title)
        .title_alignment(ratatui::layout::Alignment::Center);
    let inner_area = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    match app.skill_add_mode {
        SkillAddMode::None => {
            // Split inner area: search (2 lines) | skills list | footer (2 lines)
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(2), // Search input
                    Constraint::Min(1),    // Skill list
                    Constraint::Length(2), // Footer hints
                ])
                .split(inner_area);

            // Render search input
            let search_line = if app.skill_search.is_empty() {
                Line::from(Span::styled(
                    "Type to search...",
                    Style::default().fg(Color::DarkGray),
                ))
            } else {
                Line::from(vec![
                    Span::styled("Search: ", Style::default().fg(Color::Cyan)),
                    Span::styled(&app.skill_search, Style::default().fg(Color::Yellow)),
                    Span::styled("_", Style::default().fg(Color::Yellow)),
                ])
            };
            frame.render_widget(Paragraph::new(vec![search_line, Line::from("")]), chunks[0]);
            let caret_x = chunks[0].x
                + 8
                + u16::try_from(UnicodeWidthStr::width(app.skill_search.as_str()))
                    .unwrap_or(u16::MAX);
            frame.set_cursor_position((
                caret_x.min(chunks[0].right().saturating_sub(1)),
                chunks[0].y,
            ));

            // Render skill list
            let filtered = app.filtered_skills();
            let total = filtered.len();
            let mut skill_lines = Vec::new();

            if filtered.is_empty() {
                if app.skill_list.is_empty() {
                    skill_lines.push(Line::from(Span::styled(
                        "No skills installed.",
                        Style::default().fg(Color::DarkGray),
                    )));
                    skill_lines.push(Line::from(""));
                    skill_lines.push(Line::from(Span::styled(
                        "Press 'a' to install from GitHub.",
                        Style::default().fg(Color::Yellow),
                    )));
                } else {
                    skill_lines.push(Line::from(Span::styled(
                        "No matching skills.",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            } else {
                // Calculate visible height for skills (each selected skill takes 2 lines)
                let list_height = chunks[1].height as usize;
                // Account for scroll indicators (1 line each if needed)
                let visible_height = list_height.saturating_sub(2);

                // Calculate scroll to keep selected item visible
                let scroll = if app.skill_selected >= app.skill_scroll + visible_height {
                    app.skill_selected.saturating_sub(visible_height - 1)
                } else {
                    app.skill_scroll
                };

                // Show scroll indicator at top if scrolled
                if scroll > 0 {
                    skill_lines.push(Line::from(Span::styled(
                        format!("↑ {scroll} more above"),
                        Style::default().fg(Color::DarkGray),
                    )));
                }

                for (idx, skill) in filtered
                    .iter()
                    .enumerate()
                    .skip(scroll)
                    .take(visible_height)
                {
                    let selected = idx == app.skill_selected;
                    let prefix = if selected { "> " } else { "  " };

                    let source_str = match &skill.source {
                        crate::skill::SkillSource::Bundled => String::new(),
                        crate::skill::SkillSource::GitHub { owner, repo } => {
                            format!(" ({owner}/{repo})")
                        }
                        crate::skill::SkillSource::LocalPath { path } => {
                            format!(" (local:{path})")
                        }
                    };

                    let line = Line::from(vec![
                        Span::styled(
                            prefix,
                            if selected {
                                Style::default().fg(Color::Yellow)
                            } else {
                                Style::default()
                            },
                        ),
                        Span::styled(
                            &skill.name,
                            if selected {
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(ratatui::style::Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::White)
                            },
                        ),
                        Span::styled(source_str, Style::default().fg(Color::DarkGray)),
                    ]);
                    skill_lines.push(line);

                    if selected {
                        skill_lines.push(Line::from(Span::styled(
                            format!("    {}", skill.description),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }

                // Show scroll indicator at bottom if more items
                let shown_end = scroll + visible_height;
                if shown_end < total {
                    skill_lines.push(Line::from(Span::styled(
                        format!("↓ {} more below", total - shown_end),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            frame.render_widget(Paragraph::new(skill_lines), chunks[1]);

            // Render footer hints (always visible)
            let footer_line = if app.skill_search.is_empty() {
                Line::from(vec![
                    Span::styled("[Enter] ", Style::default().fg(Color::Cyan)),
                    Span::styled("Use", Style::default().fg(Color::White)),
                    Span::raw("  "),
                    Span::styled("[a] ", Style::default().fg(Color::Cyan)),
                    Span::styled("Add", Style::default().fg(Color::White)),
                    Span::raw("  "),
                    Span::styled("[d] ", Style::default().fg(Color::Cyan)),
                    Span::styled("Delete", Style::default().fg(Color::White)),
                    Span::raw("  "),
                    Span::styled("[Esc] ", Style::default().fg(Color::Cyan)),
                    Span::styled("Close", Style::default().fg(Color::White)),
                ])
            } else {
                Line::from(vec![
                    Span::styled("[Enter] ", Style::default().fg(Color::Cyan)),
                    Span::styled("Use", Style::default().fg(Color::White)),
                    Span::raw("  "),
                    Span::styled("[Del] ", Style::default().fg(Color::Cyan)),
                    Span::styled("Delete", Style::default().fg(Color::White)),
                    Span::raw("  "),
                    Span::styled("[Esc] ", Style::default().fg(Color::Cyan)),
                    Span::styled("Clear search", Style::default().fg(Color::White)),
                ])
            };
            frame.render_widget(Paragraph::new(vec![Line::from(""), footer_line]), chunks[2]);
        }
        SkillAddMode::GitHubRepo => {
            let content = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Enter repository (owner/repo):",
                    Style::default().fg(Color::White),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    format!("> {}_", app.skill_github_input),
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Example: anthropics/skills",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "[Enter] Install  [Esc] Cancel",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            frame.render_widget(Paragraph::new(content), inner_area);
            let caret_x = inner_area.x
                + 2
                + u16::try_from(UnicodeWidthStr::width(app.skill_github_input.as_str()))
                    .unwrap_or(u16::MAX);
            frame.set_cursor_position((
                caret_x.min(inner_area.right().saturating_sub(1)),
                inner_area.y + 3,
            ));
        }
    }
}
