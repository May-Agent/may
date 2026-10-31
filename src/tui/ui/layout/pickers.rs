//! Session / profile / file picker overlays.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use super::super::super::app::App;

/// Render session selection popup with profile sections.
pub(super) fn render_session_popup(frame: &mut Frame, area: Rect, app: &App) {
    let popup_area = area;
    frame.render_widget(Clear, popup_area);

    let filtered = app.filtered_sessions();
    let filtered_len = filtered.len();
    let current_id = app.session_manager.current().map(|s| s.id.as_str());
    let has_profile = app.profile_sessions.is_some();

    // Build title with profile name
    let profile_label = app
        .current_profile_name()
        .map(|n| format!(" [{n}]"))
        .unwrap_or_default();
    let title = if app.session_search.is_empty() {
        format!("Sessions{} ({})", profile_label, app.session_list.len())
    } else {
        format!(
            "Sessions{} ({}/{})",
            profile_label,
            filtered_len,
            app.session_list.len()
        )
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title)
        .title_alignment(ratatui::layout::Alignment::Left);
    let inner_area = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    // Profile tabs row (only if multiple profiles)
    let has_profile_tabs = app.profiles.len() > 1;
    let profile_tab_height = if has_profile_tabs { 2 } else { 0 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(profile_tab_height), // Profile tabs
            Constraint::Length(2),                  // Search input
            Constraint::Min(1),                     // Session list
            Constraint::Length(2),                  // Footer hints
        ])
        .split(inner_area);

    // Render profile tabs
    if has_profile_tabs {
        let mut tab_spans = Vec::new();
        for (i, profile) in app.profiles.iter().enumerate() {
            let is_active = Some(profile.id.as_str()) == app.current_profile_id.as_deref();
            if i > 0 {
                tab_spans.push(Span::raw(" "));
            }
            if is_active {
                tab_spans.push(Span::styled(
                    format!("[{}]", profile.name),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ));
            } else {
                tab_spans.push(Span::styled(
                    format!(" {} ", profile.name),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
        frame.render_widget(
            Paragraph::new(vec![Line::from(tab_spans), Line::from("")]),
            chunks[0],
        );
    }

    // Render search input
    let search_line = if app.session_search.is_empty() {
        Line::from(Span::styled(
            "Type to search...",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(vec![
            Span::styled("Search: ", Style::default().fg(Color::Cyan)),
            Span::styled(&app.session_search, Style::default().fg(Color::Yellow)),
        ])
    };
    frame.render_widget(Paragraph::new(vec![search_line, Line::from("")]), chunks[1]);
    let caret_x = chunks[1].x
        + 8
        + u16::try_from(UnicodeWidthStr::width(app.session_search.as_str())).unwrap_or(u16::MAX);
    frame.set_cursor_position((
        caret_x.min(chunks[1].right().saturating_sub(1)),
        chunks[1].y,
    ));

    // Build display lines with section headers
    let list_height = chunks[2].height as usize;
    let visible_height = list_height.saturating_sub(2);

    let scroll = if visible_height == 0 {
        0
    } else if app.session_selected >= app.session_scroll + visible_height {
        app.session_selected.saturating_sub(visible_height - 1)
    } else if app.session_selected < app.session_scroll {
        app.session_selected
    } else {
        app.session_scroll
    };

    let mut session_lines = Vec::new();

    if filtered.is_empty() {
        if app.session_list.is_empty() {
            session_lines.push(Line::from(Span::styled(
                "No sessions.",
                Style::default().fg(Color::DarkGray),
            )));
            session_lines.push(Line::from(""));
            session_lines.push(Line::from(Span::styled(
                "Press 'n' to create a new session.",
                Style::default().fg(Color::Yellow),
            )));
        } else {
            session_lines.push(Line::from(Span::styled(
                "No matching sessions.",
                Style::default().fg(Color::DarkGray),
            )));
        }
    } else {
        if scroll > 0 {
            session_lines.push(Line::from(Span::styled(
                format!("  {scroll} more above"),
                Style::default().fg(Color::DarkGray),
            )));
        }

        // Track section transitions for rendering headers
        let mut prev_section: Option<&str> = None;

        for (display_idx, (_original_idx, summary)) in filtered
            .iter()
            .enumerate()
            .skip(scroll)
            .take(visible_height)
        {
            // Insert section headers when section changes (only for profile view, not searching)
            if has_profile && app.session_search.is_empty() {
                let section = if summary.pinned {
                    "pinned"
                } else if summary.archived {
                    "archived"
                } else {
                    "regular"
                };

                if prev_section != Some(section) {
                    if prev_section.is_some() {
                        // Separator between sections
                        session_lines.push(Line::from(Span::styled(
                            "  ────────────────────────",
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                    // Section header
                    let header = match section {
                        "pinned" => {
                            let count = app.profile_sessions.as_ref().map_or(0, |ps| {
                                ps.pinned
                                    .iter()
                                    .map(|item| match item {
                                        crate::profile::PinnedItem::Session(_) => 1,
                                        crate::profile::PinnedItem::Folder { sessions, .. } => {
                                            sessions.len()
                                        }
                                    })
                                    .sum::<usize>()
                            });
                            format!("  Pinned ({count})")
                        }
                        "archived" => {
                            let count = app
                                .profile_sessions
                                .as_ref()
                                .map_or(0, |ps| ps.archived.len());
                            format!("  Archived ({count})")
                        }
                        _ => "  Recent".to_string(),
                    };
                    session_lines.push(Line::from(Span::styled(
                        header,
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )));
                    prev_section = Some(section);
                }
            }

            let selected = display_idx == app.session_selected;
            let prefix = if selected { "> " } else { "  " };

            // Add pin/archive indicator
            let indicator = if summary.pinned {
                "# "
            } else if summary.archived {
                "~ "
            } else {
                ""
            };

            let display_name = summary.title.as_deref().unwrap_or("Untitled");
            let is_current = current_id == Some(summary.id.as_str());

            let mut name_style = if selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            if is_current {
                name_style = name_style.add_modifier(ratatui::style::Modifier::BOLD);
            }

            let indicator_style = if summary.pinned {
                Style::default().fg(Color::Cyan)
            } else if summary.archived {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
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
                Span::styled(indicator, indicator_style),
                Span::styled(display_name, name_style),
                Span::styled(
                    format!(" ({} messages)", summary.message_count),
                    Style::default().fg(Color::DarkGray),
                ),
                if is_current {
                    Span::styled(" (current)", Style::default().fg(Color::Green))
                } else {
                    Span::raw("")
                },
            ]);
            session_lines.push(line);

            // Show search match preview
            if selected && !app.session_search.is_empty() {
                render_search_preview(&mut session_lines, summary, &app.session_search);
            }
        }

        // Archived section collapsed indicator
        if has_profile
            && app.session_search.is_empty()
            && !app.archived_expanded
            && app
                .profile_sessions
                .as_ref()
                .is_some_and(|ps| !ps.archived.is_empty())
        {
            let count = app
                .profile_sessions
                .as_ref()
                .map_or(0, |ps| ps.archived.len());
            session_lines.push(Line::from(Span::styled(
                "  ────────────────────────",
                Style::default().fg(Color::DarkGray),
            )));
            session_lines.push(Line::from(Span::styled(
                format!("  Archived ({count}) [press 'e' to expand]"),
                Style::default().fg(Color::DarkGray),
            )));
        }

        let shown_end = scroll + visible_height;
        if shown_end < filtered_len {
            session_lines.push(Line::from(Span::styled(
                format!("  {} more below", filtered_len - shown_end),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    frame.render_widget(Paragraph::new(session_lines), chunks[2]);

    // Footer hints
    let footer_line = if app.session_search.is_empty() {
        let mut spans = vec![
            Span::styled("[jk] ", Style::default().fg(Color::Cyan)),
            Span::styled("Nav", Style::default().fg(Color::White)),
            Span::raw(" "),
            Span::styled("[Enter] ", Style::default().fg(Color::Cyan)),
            Span::styled("Open", Style::default().fg(Color::White)),
            Span::raw(" "),
            Span::styled("[n] ", Style::default().fg(Color::Cyan)),
            Span::styled("New", Style::default().fg(Color::White)),
            Span::raw(" "),
            Span::styled("[d] ", Style::default().fg(Color::Cyan)),
            Span::styled("Del", Style::default().fg(Color::White)),
        ];
        if has_profile {
            spans.extend([
                Span::raw(" "),
                Span::styled("[p] ", Style::default().fg(Color::Cyan)),
                Span::styled("Pin", Style::default().fg(Color::White)),
                Span::raw(" "),
                Span::styled("[a] ", Style::default().fg(Color::Cyan)),
                Span::styled("Archive", Style::default().fg(Color::White)),
            ]);
        }
        if has_profile_tabs {
            spans.extend([
                Span::raw(" "),
                Span::styled("[Tab] ", Style::default().fg(Color::Cyan)),
                Span::styled("Profile", Style::default().fg(Color::White)),
            ]);
        }
        Line::from(spans)
    } else {
        Line::from(vec![
            Span::styled("[↑↓] ", Style::default().fg(Color::Cyan)),
            Span::styled("Navigate", Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("[Enter] ", Style::default().fg(Color::Cyan)),
            Span::styled("Select", Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("[Esc] ", Style::default().fg(Color::Cyan)),
            Span::styled("Clear search", Style::default().fg(Color::White)),
        ])
    };
    frame.render_widget(Paragraph::new(vec![Line::from(""), footer_line]), chunks[3]);
}

/// Render search match preview snippet below a selected session.
pub(super) fn render_search_preview(
    lines: &mut Vec<Line<'_>>,
    summary: &crate::SessionSummary,
    search: &str,
) {
    let query = search.to_lowercase();
    let query_char_count = query.chars().count();
    let content_lower = summary.content_preview.to_lowercase();
    let Some(pos) = content_lower.find(&query) else {
        return;
    };

    let start = summary.content_preview[..pos]
        .char_indices()
        .rev()
        .nth(20)
        .map_or(0, |(i, _)| i);
    let end = summary.content_preview[pos..]
        .char_indices()
        .nth(query_char_count + 40)
        .map_or(summary.content_preview.len(), |(i, _)| pos + i);

    let snippet = summary.content_preview[start..end].replace('\n', " ");
    let snippet_lower = snippet.to_lowercase();

    let prefix = if start > 0 { "..." } else { "" };
    let suffix = if end < summary.content_preview.len() {
        "..."
    } else {
        ""
    };

    if let Some(match_pos) = snippet_lower.find(&query) {
        let match_end = match_pos + query.len();
        let before_match = snippet[..match_pos].to_string();
        let matched = snippet[match_pos..match_end.min(snippet.len())].to_string();
        let after_match = snippet[match_end.min(snippet.len())..].to_string();

        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(prefix.to_string(), Style::default().fg(Color::DarkGray)),
            Span::styled(before_match, Style::default().fg(Color::DarkGray)),
            Span::styled(
                matched,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::styled(after_match, Style::default().fg(Color::DarkGray)),
            Span::styled(suffix.to_string(), Style::default().fg(Color::DarkGray)),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            format!("    {prefix}{snippet}{suffix}"),
            Style::default().fg(Color::DarkGray),
        )));
    }
}

/// Render profile selection popup.
pub(super) fn render_profile_select_popup(frame: &mut Frame, area: Rect, app: &App) {
    let popup_area = area;
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title("Switch Profile")
        .title_alignment(ratatui::layout::Alignment::Left);
    let inner_area = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // Profile list
            Constraint::Length(1), // Footer
        ])
        .split(inner_area);

    let mut items: Vec<ListItem> = Vec::new();
    for (i, profile) in app.profiles.iter().enumerate() {
        let selected = i == app.profile_selected;
        let is_current = Some(profile.id.as_str()) == app.current_profile_id.as_deref();

        let env_indicator = match profile.environment.kind() {
            crate::profile::ProfileEnvironment::Local => "",
            crate::profile::ProfileEnvironment::Container { .. } => " [container]",
        };

        let prefix = if selected { "> " } else { "  " };
        let mut spans = vec![
            Span::styled(
                prefix,
                if selected {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                },
            ),
            Span::styled(
                &profile.name,
                if selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(ratatui::style::Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                },
            ),
            Span::styled(env_indicator, Style::default().fg(Color::DarkGray)),
        ];
        if is_current {
            spans.push(Span::styled(" (active)", Style::default().fg(Color::Green)));
        }

        items.push(ListItem::new(Line::from(spans)));
    }

    frame.render_widget(List::new(items), chunks[0]);

    let footer = Line::from(vec![
        Span::styled("[jk] ", Style::default().fg(Color::Cyan)),
        Span::styled("Navigate", Style::default().fg(Color::White)),
        Span::raw("  "),
        Span::styled("[Enter] ", Style::default().fg(Color::Cyan)),
        Span::styled("Select", Style::default().fg(Color::White)),
        Span::raw("  "),
        Span::styled("[Esc] ", Style::default().fg(Color::Cyan)),
        Span::styled("Close", Style::default().fg(Color::White)),
    ]);
    frame.render_widget(Paragraph::new(footer), chunks[1]);
}

/// Render file picker popup.
pub(super) fn render_file_picker_popup(frame: &mut Frame, area: Rect, app: &App) {
    let popup_area = area;

    // Clear background
    frame.render_widget(Clear, popup_area);

    let Some(picker) = &app.file_picker else {
        return;
    };

    // Split popup into search input and results list
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Search input
            Constraint::Min(1),    // Results list
        ])
        .split(popup_area);

    // Search input
    let query = picker.query();
    let input_text = if query.is_empty() {
        "Type to search files...".to_string()
    } else {
        query.to_string()
    };

    let input_style = if query.is_empty() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    };

    let search_input = Paragraph::new(input_text).style(input_style).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(format!("Select File ({} matches)", picker.match_count())),
    );

    frame.render_widget(search_input, chunks[0]);
    let caret_x =
        chunks[0].x + 1 + u16::try_from(UnicodeWidthStr::width(query)).unwrap_or(u16::MAX);
    frame.set_cursor_position((
        caret_x.min(chunks[0].right().saturating_sub(2)),
        chunks[0].y + 1,
    ));

    // Results list
    let max_visible = (chunks[1].height as usize).saturating_sub(2);
    let selected_idx = picker.selected_index();

    let items: Vec<ListItem> = picker
        .visible_matches(max_visible)
        .map(|(idx, path)| {
            let path_str = path.to_string_lossy();
            let style = if idx == selected_idx {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(path_str.to_string()).style(style)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Plain)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    frame.render_widget(list, chunks[1]);
}
