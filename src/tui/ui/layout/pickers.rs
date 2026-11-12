//! Session / profile / file picker overlays.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Clear, List, ListItem, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use super::super::band::panel;
use jiff::Timestamp;

use super::super::super::app::App;
use crate::tui::connect::{ConnectStage, ConnectState};
use crate::tui::state::{ResumePickerState, SessionView};
use crate::tui::ui::sweep::sweep_spans;

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
    let heading = match app.session_view {
        SessionView::Recent => "Sessions",
        SessionView::Archive => "Archived sessions",
    };
    let title = if app.session_search.is_empty() {
        format!("{heading}{} ({})", profile_label, app.session_list.len())
    } else {
        format!(
            "{heading}{} ({}/{})",
            profile_label,
            filtered_len,
            app.session_list.len()
        )
    };

    let block = panel(Some(Span::raw(title)), Color::Cyan, app.band_bg)
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
        if app.session_list.is_empty() && app.session_view == SessionView::Archive {
            session_lines.push(Line::from(Span::styled(
                "No archived sessions.",
                Style::default().fg(Color::DarkGray),
            )));
        } else if app.session_list.is_empty() {
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
        let mut prev_repo: Option<&str> = None;

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
                        // Blank row between sections
                        session_lines.push(Line::from(""));
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
                    prev_repo = None;
                }
                // Regular sessions run in project groups: a header names the
                // project, GitHub ones by `owner/repo` and local ones by path.
                if section == "regular" && summary.repo.as_deref() != prev_repo {
                    if let Some(repo) = summary.repo.as_deref() {
                        let github = repo.starts_with("github:");
                        session_lines.push(Line::from(vec![
                            Span::styled(
                                if github { "  GitHub " } else { "  " },
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(
                                crate::session::project_label(repo),
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(ratatui::style::Modifier::BOLD),
                            ),
                        ]));
                    }
                    prev_repo = summary.repo.as_deref();
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

            let mut spans = vec![
                Span::styled(
                    prefix,
                    if selected {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(indicator, indicator_style),
            ];
            if summary.working {
                // The highlight sweeps along the title while the session works.
                spans.extend(sweep_spans(
                    display_name,
                    app.spinner_frame,
                    name_style.fg(Color::DarkGray),
                    name_style.fg(Color::White),
                ));
            } else {
                spans.push(Span::styled(display_name, name_style));
            }
            spans.push(Span::styled(
                format!(" ({} messages)", summary.message_count),
                Style::default().fg(Color::DarkGray),
            ));
            if summary.working {
                spans.push(Span::styled(
                    " Working...",
                    Style::default().fg(Color::Cyan),
                ));
            }
            if is_current {
                spans.push(Span::styled(
                    " (current)",
                    Style::default().fg(Color::Green),
                ));
            }
            session_lines.push(Line::from(spans));

            // Show search match preview
            if selected && !app.session_search.is_empty() {
                render_search_preview(&mut session_lines, summary, &app.session_search);
            }
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
                Span::styled(
                    match app.session_view {
                        SessionView::Recent => "Archive",
                        SessionView::Archive => "Unarchive",
                    },
                    Style::default().fg(Color::White),
                ),
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

    let block = panel(Some(Span::raw("Switch Profile")), Color::Cyan, app.band_bg)
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

    let search_input = Paragraph::new(input_text).style(input_style).block(panel(
        Some(Span::raw(format!(
            "Select File ({} matches)",
            picker.match_count()
        ))),
        Color::Cyan,
        app.band_bg,
    ));

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

    let list = List::new(items).block(panel(None::<Span>, Color::Cyan, app.band_bg));

    frame.render_widget(list, chunks[1]);
}

/// Render the `/resume-claude` session picker.
pub(super) fn render_resume_picker(
    frame: &mut Frame,
    area: Rect,
    state: &ResumePickerState,
    bg: Option<Color>,
) {
    frame.render_widget(Clear, area);

    let filtered = state.filtered();
    let title = if state.loading {
        "Claude sessions".to_string()
    } else if state.search.is_empty() {
        format!("Claude sessions ({})", state.sessions.len())
    } else {
        format!(
            "Claude sessions ({}/{})",
            filtered.len(),
            state.sessions.len()
        )
    };

    let block = panel(Some(Span::raw(title)), Color::Cyan, bg)
        .title_alignment(ratatui::layout::Alignment::Left);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(inner);

    let search_line = if state.search.is_empty() {
        Line::from(Span::styled(
            "Type to search...",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(vec![
            Span::styled("Search: ", Style::default().fg(Color::Cyan)),
            Span::styled(&state.search, Style::default().fg(Color::Yellow)),
        ])
    };
    frame.render_widget(Paragraph::new(vec![search_line, Line::from("")]), chunks[0]);
    let caret_col = if state.search.is_empty() {
        0
    } else {
        8 + UnicodeWidthStr::width(state.search.as_str())
    };
    let caret_x = chunks[0].x + u16::try_from(caret_col).unwrap_or(u16::MAX);
    frame.set_cursor_position((
        caret_x.min(chunks[0].right().saturating_sub(1)),
        chunks[0].y,
    ));

    let mut lines = Vec::new();
    if state.loading {
        lines.push(Line::from(Span::styled(
            "Loading Claude sessions...",
            Style::default().fg(Color::DarkGray),
        )));
    } else if filtered.is_empty() {
        let message = if state.sessions.is_empty() {
            "No Claude Code sessions found."
        } else {
            "No matching sessions."
        };
        lines.push(Line::from(Span::styled(
            message,
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let list_height = chunks[1].height as usize;
        let visible = (list_height / 2).max(1);
        let scroll = state.selected.saturating_sub(visible.saturating_sub(1));
        if scroll > 0 {
            lines.push(Line::from(Span::styled(
                format!("  {scroll} more above"),
                Style::default().fg(Color::DarkGray),
            )));
        }
        for (display_idx, &index) in filtered.iter().enumerate().skip(scroll).take(visible) {
            let Some(session) = state.sessions.get(index) else {
                continue;
            };
            let selected = display_idx == state.selected;
            let prefix = if selected { "> " } else { "  " };
            let name_style = if selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let meta_style = Style::default().fg(Color::DarkGray);
            let cwd = session.cwd.as_deref().map(display_cwd).unwrap_or_default();
            let id_short: String = session.id.chars().take(8).collect();
            let age = session.modified.map(relative_age).unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(
                    prefix,
                    if selected {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(&session.title, name_style),
            ]));
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(cwd, meta_style),
                Span::styled("  ", meta_style),
                Span::styled(id_short, meta_style),
                Span::styled("  ", meta_style),
                Span::styled(age, meta_style),
            ]));
        }
        let shown_end = scroll + visible;
        if shown_end < filtered.len() {
            lines.push(Line::from(Span::styled(
                format!("  {} more below", filtered.len() - shown_end),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), chunks[1]);

    let footer = Line::from(vec![
        Span::styled("[↑↓] ", Style::default().fg(Color::Cyan)),
        Span::styled("Navigate", Style::default().fg(Color::White)),
        Span::raw("  "),
        Span::styled("[Enter] ", Style::default().fg(Color::Cyan)),
        Span::styled("Import", Style::default().fg(Color::White)),
        Span::raw("  "),
        Span::styled("[Esc] ", Style::default().fg(Color::Cyan)),
        Span::styled("Close", Style::default().fg(Color::White)),
    ]);
    frame.render_widget(Paragraph::new(vec![Line::from(""), footer]), chunks[2]);
}

fn display_cwd(cwd: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy();
        if let Some(rest) = cwd.strip_prefix(home.as_ref()) {
            return format!("~{rest}");
        }
    }
    cwd.to_string()
}

fn relative_age(ts: Timestamp) -> String {
    let Ok(span) = Timestamp::now().since(ts) else {
        return ts.strftime("%Y-%m-%d").to_string();
    };
    let Ok(secs) = span.total(jiff::Unit::Second) else {
        return ts.strftime("%Y-%m-%d").to_string();
    };
    if !(0.0..2_592_000.0).contains(&secs) {
        return ts.strftime("%Y-%m-%d").to_string();
    }
    if secs < 60.0 {
        format!("{}s", secs.round())
    } else if secs < 3600.0 {
        format!("{}m", (secs / 60.0).round())
    } else if secs < 86_400.0 {
        format!("{}h", (secs / 3600.0).round())
    } else {
        format!("{}d", (secs / 86_400.0).round())
    }
}

/// Braille frames of the connect picker's loading indicator.
const CONNECT_SPINNER: [char; 4] = ['⠋', '⠙', '⠹', '⠸'];

/// Rows the `/connect` picker wants.
pub(super) const fn connect_picker_rows(state: &ConnectState) -> usize {
    match &state.stage {
        ConnectStage::Detecting | ConnectStage::Connecting(_) => 6,
        ConnectStage::Agents {
            detected,
            connected,
        } => detected.len() + connected.len() + 6,
        ConnectStage::Choose { valid, .. } => valid.len() + 6,
    }
}

/// Render the `/connect` picker.
pub(super) fn render_connect_picker(
    frame: &mut Frame,
    area: Rect,
    state: &ConnectState,
    spinner_frame: usize,
    bg: Option<Color>,
) {
    frame.render_widget(Clear, area);
    let dim = Style::default().fg(Color::DarkGray);
    let spinner = CONNECT_SPINNER[spinner_frame % CONNECT_SPINNER.len()];
    let row = |selected: bool, text: String, detail: String| {
        let (prefix, style) = if selected {
            (
                "> ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            )
        } else {
            ("  ", Style::default().fg(Color::White))
        };
        Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(text, style),
            Span::styled(detail, dim),
        ])
    };
    let mut lines = Vec::new();
    let (title, footer) = match &state.stage {
        ConnectStage::Detecting => {
            lines.push(Line::from(Span::styled(
                format!("{spinner} Looking for installed ACP agents..."),
                dim,
            )));
            ("Connect an ACP agent", "[Esc] Close")
        }
        ConnectStage::Connecting(request) => {
            lines.push(Line::from(Span::styled(
                format!(
                    "{spinner} Opening a trial session with {}...",
                    request.command
                ),
                dim,
            )));
            ("Connect an ACP agent", "[Esc] Close")
        }
        ConnectStage::Agents {
            detected,
            connected,
        } => {
            if detected.is_empty() {
                lines.push(Line::from(Span::styled(
                    "No installed ACP agent found. Use /connect <command> [args].",
                    dim,
                )));
            }
            for (index, agent) in detected.iter().enumerate() {
                let detail = agent.connected_as.as_ref().map_or_else(
                    || format!("  {} {}", agent.command, agent.args.join(" ")),
                    |name| format!("  connected as {name}"),
                );
                lines.push(row(
                    index == state.selected,
                    format!("{} {}", agent.name, agent.version),
                    detail,
                ));
            }
            let others: Vec<_> = connected
                .iter()
                .filter(|agent| {
                    !detected
                        .iter()
                        .any(|found| found.connected_as.as_ref() == Some(&agent.name))
                })
                .collect();
            if !others.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("Also connected:", dim)));
                for agent in others {
                    lines.push(Line::from(Span::styled(
                        format!("  {}  ({})", agent.name, agent.command),
                        dim,
                    )));
                }
            }
            (
                "Connect an ACP agent",
                "[↑↓] Navigate  [Enter] Connect  [Esc] Close",
            )
        }
        ConnectStage::Choose { choice, valid, .. } => {
            for (index, id) in valid.iter().enumerate() {
                lines.push(row(index == state.selected, id.clone(), String::new()));
            }
            (
                match choice {
                    crate::client::acp::AcpChoice::ModelRequired => "Choose the agent's model",
                    crate::client::acp::AcpChoice::ModeRequired => "Choose the agent's mode",
                },
                "[↑↓] Navigate  [Enter] Choose  [Esc] Cancel",
            )
        }
    };
    let block = panel(Some(Span::raw(title)), Color::Cyan, bg);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(Paragraph::new(lines), chunks[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer,
            Style::default().fg(Color::Cyan),
        ))),
        chunks[1],
    );
}
