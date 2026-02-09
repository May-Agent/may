//! Main layout for the TUI.

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};

use super::super::app::{
    App, AppMode, AskUserState, ConfigEditMode, ConfigSection, CopilotAuthStatus, McpAddMode,
    SkillAddMode, SshAddMode,
};
use super::super::completion::CompletionMode;
use super::chat::{self, InputContext, QueueContext, StatusContext, UnifiedRenderContext};
use crate::workspace_request::WorkspaceRequest;

/// Convert character index to byte index for safe string slicing.
fn char_to_byte_idx(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Render the main layout.
pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Full-screen modes (don't show chat behind them)
    match &app.mode {
        AppMode::Config => {
            render_config_screen(frame, area, app);
            return;
        }
        AppMode::TierModelPicker => {
            // Render config screen as background, then tier picker as overlay
            render_config_screen(frame, area, app);
            render_tier_model_picker(frame, area, app);
            return;
        }
        AppMode::CopilotAuth => {
            render_copilot_auth_screen(frame, area, app);
            return;
        }
        AppMode::WorkspaceConfirm(request) => {
            render_workspace_confirm(frame, area, request);
            return;
        }
        AppMode::AskUser(state) => {
            render_ask_user(frame, area, state);
            return;
        }
        _ => {}
    }

    // Scan chat content for images and request fetches for any not yet cached
    app.scan_and_fetch_images();

    // Build queue message previews
    let queue_messages: Vec<String> = app.queue.iter().map(|m| m.content.clone()).collect();

    // Build usage info for status
    let usage_info = app.usage.as_ref().and_then(|u| {
        u.prompt_tokens
            .map(|pt| (pt as u64, app.context_window.map(|w| w as u64)))
    });

    let images_supported = app.supports_images();
    let chat_area = area;

    // Collect image positions during rendering
    let mut image_positions: Vec<(String, usize)> = Vec::new();

    // Capture scroll value for image position calculations
    let scroll = app.scroll;

    // Render everything in a single unified scrollable area
    chat::render_unified(
        frame,
        chat_area,
        &app.chat,
        scroll,
        UnifiedRenderContext {
            selected_tool: app.selected_tool.as_deref(),
            content_height_out: Some(&app.content_height),
            view_height_out: Some(&app.view_height),
            input_ctx: Some(InputContext {
                input: &app.input,
                cursor: app.cursor,
                attachments: &app.attachments,
            }),
            queue_ctx: if queue_messages.is_empty() {
                None
            } else {
                Some(QueueContext {
                    messages: &queue_messages,
                })
            },
            status_ctx: Some(StatusContext {
                model: &app.model_display(),
                usage: usage_info,
                queue_count: app.queue.len(),
                background_tasks: app.background_tasks,
                processing: app.processing,
                status_message: app.status.as_deref(),
            }),
            images_supported,
            image_positions_out: Some(&mut image_positions),
        },
    );

    // Render images as overlays at their scroll-adjusted positions
    if images_supported && !image_positions.is_empty() {
        render_inline_images(frame, chat_area, app, &image_positions, scroll);
    }

    // Render popups if in special modes
    match app.mode {
        AppMode::SessionSelect => {
            render_session_popup(frame, area, app);
        }
        AppMode::FilePicker => {
            render_file_picker_popup(frame, area, app);
        }
        AppMode::Help => {
            render_help_popup(frame, area);
        }
        AppMode::ToolDetail => {
            render_tool_detail_popup(frame, area, app);
        }
        AppMode::ModelSelect => {
            render_model_select_popup(frame, area, app);
        }
        AppMode::McpManage => {
            render_mcp_popup(frame, area, app);
        }
        AppMode::SshManage => {
            render_ssh_popup(frame, area, app);
        }
        AppMode::SkillManage => {
            render_skill_popup(frame, area, app);
        }
        _ => {}
    }

    // Render completion overlay if active (always on top)
    if app.completion.is_active() && !app.completion.is_empty() {
        render_completion_overlay(frame, area, app);
    }
}

/// Help text content.
const HELP_TEXT: &str = include_str!("help.txt");

/// Render session selection popup.
fn render_session_popup(frame: &mut Frame, area: Rect, app: &App) {
    let popup_area = centered_rect(70, 60, area);

    // Clear background
    frame.render_widget(Clear, popup_area);

    // Get filtered sessions
    let filtered = app.filtered_sessions();
    let filtered_len = filtered.len();

    // Get current session ID for highlighting
    let current_id = app.session_manager.current().map(|s| s.id.as_str());

    // Render the border block
    let title = if app.session_search.is_empty() {
        format!("Sessions ({})", app.session_list.len())
    } else {
        format!("Sessions ({}/{})", filtered_len, app.session_list.len())
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title)
        .title_alignment(ratatui::layout::Alignment::Left);
    let inner_area = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    // Split inner area: search (2 lines) | sessions list | footer (2 lines)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Search input
            Constraint::Min(1),    // Session list
            Constraint::Length(2), // Footer hints
        ])
        .split(inner_area);

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
            Span::styled("_", Style::default().fg(Color::Yellow)),
        ])
    };
    frame.render_widget(Paragraph::new(vec![search_line, Line::from("")]), chunks[0]);

    // Render session list with scrolling
    let list_height = chunks[1].height as usize;
    let visible_height = list_height.saturating_sub(2); // Account for potential scroll indicators

    // Calculate scroll to keep selected item visible
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
        // Show scroll indicator at top if scrolled
        if scroll > 0 {
            session_lines.push(Line::from(Span::styled(
                format!("↑ {} more above", scroll),
                Style::default().fg(Color::DarkGray),
            )));
        }

        for (display_idx, (_original_idx, summary)) in filtered
            .iter()
            .enumerate()
            .skip(scroll)
            .take(visible_height)
        {
            let selected = display_idx == app.session_selected;
            let prefix = if selected { "> " } else { "  " };

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

            let line = Line::from(vec![
                Span::styled(
                    prefix,
                    if selected {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default()
                    },
                ),
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

            // Show content preview on second line if selected and searching
            if selected && !app.session_search.is_empty() {
                // Find and show matching content snippet with highlighting
                let query = app.session_search.to_lowercase();
                let query_char_count = query.chars().count();
                let content_lower = summary.content_preview.to_lowercase();
                if let Some(pos) = content_lower.find(&query) {
                    // Find safe UTF-8 boundaries around the match
                    let start = summary.content_preview[..pos]
                        .char_indices()
                        .rev()
                        .nth(20)
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    let end = summary.content_preview[pos..]
                        .char_indices()
                        .nth(query_char_count + 40)
                        .map(|(i, _)| pos + i)
                        .unwrap_or(summary.content_preview.len());

                    let snippet = summary.content_preview[start..end].replace('\n', " ");
                    let snippet_lower = snippet.to_lowercase();

                    let prefix = if start > 0 { "..." } else { "" };
                    let suffix = if end < summary.content_preview.len() {
                        "..."
                    } else {
                        ""
                    };

                    // Find match position in the processed snippet for accurate highlighting
                    if let Some(match_pos) = snippet_lower.find(&query) {
                        let match_end = match_pos + query.len();
                        let before_match = snippet[..match_pos].to_string();
                        let matched = snippet[match_pos..match_end.min(snippet.len())].to_string();
                        let after_match = snippet[match_end.min(snippet.len())..].to_string();

                        session_lines.push(Line::from(vec![
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
                        // Fallback: no highlight if match not found in snippet
                        session_lines.push(Line::from(Span::styled(
                            format!("    {}{}{}", prefix, snippet, suffix),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
            }
        }

        // Show scroll indicator at bottom if more items
        let shown_end = scroll + visible_height;
        if shown_end < filtered_len {
            session_lines.push(Line::from(Span::styled(
                format!("↓ {} more below", filtered_len - shown_end),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    frame.render_widget(Paragraph::new(session_lines), chunks[1]);

    // Render footer hints
    let footer_line = if app.session_search.is_empty() {
        Line::from(vec![
            Span::styled("[↑↓/jk] ", Style::default().fg(Color::Cyan)),
            Span::styled("Navigate", Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("[Enter] ", Style::default().fg(Color::Cyan)),
            Span::styled("Select", Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("[n] ", Style::default().fg(Color::Cyan)),
            Span::styled("New", Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("[d] ", Style::default().fg(Color::Cyan)),
            Span::styled("Delete", Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("[Esc] ", Style::default().fg(Color::Cyan)),
            Span::styled("Close", Style::default().fg(Color::White)),
        ])
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
    frame.render_widget(Paragraph::new(vec![Line::from(""), footer_line]), chunks[2]);
}

/// Render file picker popup.
fn render_file_picker_popup(frame: &mut Frame, area: Rect, app: &App) {
    let popup_area = centered_rect(70, 60, area);

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

/// Render help popup.
fn render_help_popup(frame: &mut Frame, area: Rect) {
    let popup_area = centered_rect(70, 80, area);

    // Clear background
    frame.render_widget(Clear, popup_area);

    let help = Paragraph::new(HELP_TEXT)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title("Help (press ? or Esc to close)"),
        )
        .style(Style::default().fg(Color::White))
        .wrap(ratatui::widgets::Wrap { trim: false });

    frame.render_widget(help, popup_area);
}

/// Render tool detail popup (full screen).
fn render_tool_detail_popup(frame: &mut Frame, area: Rect, app: &App) {
    // Full screen popup
    let popup_area = centered_rect(90, 90, area);

    // Clear background
    frame.render_widget(Clear, popup_area);

    // Get the selected tool
    if let Some(ref tool_id) = app.selected_tool {
        if let Some(display) = app.chat.get_tool(tool_id) {
            chat::render_tool_fullscreen(frame, popup_area, display, app.tool_detail_scroll);
        }
    }
}

/// Render tier model picker as a popup menu.
fn render_tier_model_picker(frame: &mut Frame, area: Rect, app: &App) {
    use crate::config::ModelTier;

    let tier_name = match app.tier_picker_tier {
        Some(ModelTier::Advanced) => "Advanced",
        Some(ModelTier::Balanced) => "Balanced",
        Some(ModelTier::Fast) => "Fast",
        Some(ModelTier::ImageGeneration) => "Image Generation",
        None => "Model",
    };

    // Create a popup area (60% width, 50% height, centered)
    let popup_width = (area.width * 60 / 100).min(80).max(40);
    let popup_height = (area.height * 50 / 100).min(20).max(10);
    let popup_x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let popup_y = area.y + (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(popup_x, popup_y, popup_width, popup_height);

    // Clear the popup area
    frame.render_widget(Clear, popup_area);

    // Layout: Search + List
    let inner_area = Rect::new(
        popup_area.x + 1,
        popup_area.y + 1,
        popup_area.width.saturating_sub(2),
        popup_area.height.saturating_sub(2),
    );
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Search box
            Constraint::Min(3),    // Model list
        ])
        .split(inner_area);

    // Search box
    let search_display = if app.tier_picker_query.is_empty() {
        "Type to search...".to_string()
    } else {
        app.tier_picker_query.clone()
    };
    let search = Paragraph::new(search_display)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(" Search "),
        )
        .style(if app.tier_picker_query.is_empty() {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        });
    frame.render_widget(search, chunks[0]);

    // Model list or loading indicator
    if app.tier_picker_loading {
        let loading = Paragraph::new("Loading models...")
            .style(Style::default().fg(Color::Yellow))
            .alignment(ratatui::layout::Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(" Models "),
            );
        frame.render_widget(loading, chunks[1]);
    } else {
        // Show visible window of models (scrolling)
        let visible_height = chunks[1].height.saturating_sub(2) as usize; // -2 for borders
        let total = app.tier_picker_filtered.len();
        let selected = app.tier_picker_selected;

        // Calculate scroll offset to keep selected item visible
        let scroll_offset = if selected >= visible_height {
            selected - visible_height + 1
        } else {
            0
        };

        let items: Vec<ListItem> = app
            .tier_picker_filtered
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(visible_height)
            .map(|(display_idx, &model_idx)| {
                let fetched = app.tier_picker_models.get(model_idx);
                let model_id = fetched.map(|m| m.id.as_str()).unwrap_or("");
                // Check if model matches tier (for visual indicator)
                let info = aither_models::lookup(model_id);
                let tier_match = app
                    .tier_picker_tier
                    .map(|t| match t {
                        ModelTier::Advanced => aither_models::ModelTier::Flagship,
                        ModelTier::Balanced => aither_models::ModelTier::Balanced,
                        ModelTier::Fast => aither_models::ModelTier::Fast,
                        ModelTier::ImageGeneration => aither_models::ModelTier::Flagship,
                    })
                    .and_then(|tier| info.map(|i| i.has_tier(tier)))
                    .unwrap_or(false);

                let style = if display_idx == selected {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else if tier_match {
                    Style::default().fg(Color::Green)
                } else if info.is_some() {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let prefix = if display_idx == selected { "> " } else { "  " };
                ListItem::new(format!("{}{}", prefix, model_id)).style(style)
            })
            .collect();

        let title = if total > 0 {
            format!(" {} ({}) ", tier_name, total)
        } else {
            format!(" {} (no models) ", tier_name)
        };

        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(title),
        );
        frame.render_widget(list, chunks[1]);
    }

    // Draw popup border
    let title = format!(" Select {} Model ", tier_name);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    frame.render_widget(block, popup_area);
}

/// Render unified configuration screen (full screen) with tabs.
fn render_config_screen(frame: &mut Frame, area: Rect, app: &App) {
    // Guard: need minimum size to render safely
    if area.width < 40 || area.height < 15 {
        let msg = Paragraph::new("Terminal too small")
            .style(Style::default().fg(Color::Red))
            .alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(msg, area);
        return;
    }

    let state = &app.config_state;

    // Layout: Header (tabs) | Content | Help
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // Tab bar
            Constraint::Min(5),    // Content
            Constraint::Length(1), // Help
        ])
        .split(area);

    // Tab bar
    let tabs: Vec<Span> = ConfigSection::ALL
        .iter()
        .map(|s| {
            if *s == state.section {
                Span::styled(
                    format!(" [{}] ", s.name()),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                )
            } else {
                Span::styled(format!(" {} ", s.name()), Style::default().fg(Color::White))
            }
        })
        .collect();
    let tab_line = Line::from(
        vec![Span::styled("Settings: ", Style::default().fg(Color::Cyan))]
            .into_iter()
            .chain(tabs)
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(tab_line), chunks[0]);

    // Content based on section
    match state.section {
        ConfigSection::Providers => render_providers_section(frame, chunks[1], app),
        ConfigSection::Models => render_models_section(frame, chunks[1], app),
        ConfigSection::Preferences => render_preferences_section(frame, chunks[1], app),
    }

    // Help text
    let help_text = if state.is_editing() {
        "Enter: save | Esc: cancel"
    } else {
        "Tab: switch section | j/k: navigate | Enter: edit | d: delete | Esc: close"
    };
    let help = Paragraph::new(help_text)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(help, chunks[2]);
}

/// Render providers section content.
fn render_providers_section(frame: &mut Frame, area: Rect, app: &App) {
    let state = &app.config_state;

    // Build provider list
    let mut items: Vec<ListItem> = state
        .provider_ids
        .iter()
        .enumerate()
        .map(|(idx, id)| {
            let prefix = if idx == state.selected { "> " } else { "  " };
            let instance = app.config.provider.get(id);
            let status = if let Some(inst) = instance {
                // For custom providers, show base_url
                if let Some(ref base_url) = inst.base_url {
                    // Truncate URL safely (unicode-safe)
                    let url_short = match base_url.char_indices().nth(27) {
                        Some((pos, _)) => format!("{}...", &base_url[..pos]),
                        None => base_url.clone(),
                    };
                    url_short
                } else if inst.api_key.is_some() {
                    let key = inst.api_key.as_ref().unwrap();
                    let masked: String = key.chars().take(4).collect::<String>() + "****";
                    format!("{} (configured)", masked)
                } else if id == "copilot" {
                    "(OAuth)".to_string()
                } else {
                    "(no API key)".to_string()
                }
            } else {
                "(unknown)".to_string()
            };
            let text = format!("{}{:<20} {}", prefix, id, status);
            let style = if idx == state.selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(text).style(style)
        })
        .collect();

    // Add "Add provider..." item
    let add_prefix = if state.selected >= state.provider_ids.len() {
        "> "
    } else {
        "  "
    };
    let add_style = if state.selected >= state.provider_ids.len() {
        Style::default()
            .fg(Color::Green)
            .add_modifier(ratatui::style::Modifier::BOLD)
    } else {
        Style::default().fg(Color::Green)
    };
    items.push(ListItem::new(format!("{}+ Add provider...", add_prefix)).style(add_style));

    // If in list selection mode, show provider type selector overlay
    if let ConfigEditMode::List {
        items: list_items,
        selected,
    } = &state.edit_mode
    {
        let popup_area = centered_rect(50, 60, area);
        frame.render_widget(Clear, popup_area);

        let list_items: Vec<ListItem> = list_items
            .iter()
            .enumerate()
            .map(|(idx, name)| {
                let prefix = if idx == *selected { "> " } else { "  " };
                let style = if idx == *selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(ratatui::style::Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                ListItem::new(format!("{}{}", prefix, name)).style(style)
            })
            .collect();

        let list = List::new(list_items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title("Select Provider Type"),
        );
        frame.render_widget(list, popup_area);
        return;
    }

    // If in custom provider edit mode, show form overlay
    if let ConfigEditMode::CustomProvider {
        focused_field,
        name,
        base_url,
        api_key,
        cursor,
        ..
    } = &state.edit_mode
    {
        let popup_area = centered_rect(60, 40, area);
        frame.render_widget(Clear, popup_area);

        // Split into 3 input fields + instructions
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3), // Name
                Constraint::Length(3), // Base URL
                Constraint::Length(3), // API Key
                Constraint::Min(1),    // Instructions
            ])
            .split(popup_area);

        // Helper to render a field
        let render_field = |frame: &mut Frame,
                            area: Rect,
                            label: &str,
                            value: &str,
                            is_focused: bool,
                            cur: usize,
                            mask: bool| {
            let display_value = if mask {
                value.chars().map(|_| '*').collect::<String>()
            } else {
                value.to_string()
            };
            // Convert char index to byte index for safe slicing
            let cursor_pos = char_to_byte_idx(&display_value, cur);
            let display = if is_focused {
                format!(
                    "{}|{}",
                    &display_value[..cursor_pos],
                    &display_value[cursor_pos..]
                )
            } else {
                display_value
            };
            let style = if is_focused {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::White)
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(if is_focused {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Gray)
                })
                .title(label);
            let paragraph = Paragraph::new(display).style(style).block(block);
            frame.render_widget(paragraph, area);
        };

        render_field(
            frame,
            chunks[0],
            "Name (ID)",
            name,
            *focused_field == 0,
            *cursor,
            false,
        );
        render_field(
            frame,
            chunks[1],
            "Base URL",
            base_url,
            *focused_field == 1,
            *cursor,
            false,
        );
        render_field(
            frame,
            chunks[2],
            "API Key (optional)",
            api_key,
            *focused_field == 2,
            *cursor,
            true,
        );

        let instructions =
            Paragraph::new("Tab/↓↑ to switch fields · Enter to save · Esc to cancel")
                .style(Style::default().fg(Color::DarkGray))
                .alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(instructions, chunks[3]);

        // Draw border around the popup
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title("Add Custom Provider");
        frame.render_widget(block, popup_area);
        return;
    }

    // If in text edit mode, show edit overlay
    if let Some((input, cursor)) = state.edit_text() {
        // Show input at selected row (masked is ASCII so byte=char, but use helper for consistency)
        let masked: String = input.chars().map(|_| '*').collect();
        let cursor_pos = char_to_byte_idx(&masked, cursor);
        let display = format!("{}|{}", &masked[..cursor_pos], &masked[cursor_pos..]);
        items[state.selected] = ListItem::new(format!("  API Key: {}", display))
            .style(Style::default().fg(Color::Yellow));
    }

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title("Manage LLM providers"),
    );
    frame.render_widget(list, area);
}

/// Render models section content.
fn render_models_section(frame: &mut Frame, area: Rect, app: &App) {
    let state = &app.config_state;
    let config = &app.config;

    let tier_names = ["Advanced", "Balanced", "Fast", "Image Generation"];
    let tier_values: Vec<String> = vec![
        config.models.advanced.join(", "),
        config.models.balanced.join(", "),
        config.models.fast.join(", "),
        config.models.image_generation.join(", "),
    ];

    let mut items: Vec<ListItem> = tier_names
        .iter()
        .zip(tier_values.iter())
        .enumerate()
        .map(|(idx, (name, value))| {
            let prefix = if idx == state.selected { "> " } else { "  " };
            let display_value = if value.is_empty() { "(not set)" } else { value };
            let text = format!("{}{:<20} {}", prefix, name, display_value);
            let style = if idx == state.selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(text).style(style)
        })
        .collect();

    // If in text edit mode, show edit overlay
    if let Some((input, cursor)) = state.edit_text() {
        let cursor_pos = char_to_byte_idx(input, cursor);
        let display = format!("{}|{}", &input[..cursor_pos], &input[cursor_pos..]);
        items[state.selected] =
            ListItem::new(format!("  {}: {}", tier_names[state.selected], display))
                .style(Style::default().fg(Color::Yellow));
    }

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title("Configure model preferences by tier"),
    );
    frame.render_widget(list, area);
}

/// Render preferences section content.
fn render_preferences_section(frame: &mut Frame, area: Rect, app: &App) {
    use crate::config::LoadBalanceStrategy;

    let state = &app.config_state;
    let config = &app.config;

    let pref_names = ["Max iterations", "Load balance", "Warn on model change"];
    let pref_values: Vec<String> = vec![
        config.max_iterations.to_string(),
        match config.preferences.load_balance_strategy {
            LoadBalanceStrategy::RoundRobin => "Round Robin".to_string(),
            LoadBalanceStrategy::FailoverOnly => "Failover Only".to_string(),
            LoadBalanceStrategy::Random => "Random".to_string(),
        },
        config.preferences.warn_on_model_change.to_string(),
    ];

    let mut items: Vec<ListItem> = pref_names
        .iter()
        .zip(pref_values.iter())
        .enumerate()
        .map(|(idx, (name, value))| {
            let prefix = if idx == state.selected { "> " } else { "  " };
            let text = format!("{}{:<25} {}", prefix, name, value);
            let style = if idx == state.selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(text).style(style)
        })
        .collect();

    // If in text edit mode for max_iterations
    if let Some((input, cursor)) = state.edit_text() {
        if state.selected == 0 {
            let cursor_pos = char_to_byte_idx(input, cursor);
            let display = format!("{}|{}", &input[..cursor_pos], &input[cursor_pos..]);
            items[0] = ListItem::new(format!("  Max iterations:        {}", display))
                .style(Style::default().fg(Color::Yellow));
        }
    }

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title("General preferences"),
    );
    frame.render_widget(list, area);
}

/// Render model select popup (anchored near input area).
fn render_model_select_popup(frame: &mut Frame, area: Rect, app: &App) {
    // Position popup near bottom of screen, left-aligned
    let popup_height = (app.model_select_list.len() + 2).min(10) as u16;
    let popup_width = 40.min(area.width);
    let popup_x = area.x;
    // Position near bottom, leaving space for status and input (2 lines from bottom)
    let popup_y = area.y + area.height.saturating_sub(popup_height + 2);

    let popup_area = Rect::new(popup_x, popup_y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let items: Vec<ListItem> = app
        .model_select_list
        .iter()
        .enumerate()
        .map(|(idx, model)| {
            let prefix = if idx == app.model_select_index {
                "> "
            } else {
                "  "
            };
            // Check if this is current model
            let current = app
                .current_model
                .as_deref()
                .map(|m| m == model)
                .unwrap_or(false);
            let suffix = if current { " (current)" } else { "" };
            let text = format!("{}{}{}", prefix, model, suffix);
            let style = if idx == app.model_select_index {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(text).style(style)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title("Select Model"),
    );
    frame.render_widget(list, popup_area);
}

/// Helper to create a centered rect.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Render Copilot OAuth authentication screen.
fn render_copilot_auth_screen(frame: &mut Frame, area: Rect, app: &App) {
    // Center the content
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Min(12),
            Constraint::Percentage(25),
        ])
        .split(area);

    let center_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(15),
            Constraint::Min(50),
            Constraint::Percentage(15),
        ])
        .split(chunks[1]);

    let content = match &app.copilot_auth_status {
        Some(CopilotAuthStatus::RequestingCode) => {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Requesting authorization code...",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(""),
            ]
        }
        Some(CopilotAuthStatus::WaitingForUser {
            user_code,
            verification_uri,
        }) => {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "1. Open your browser and visit:",
                    Style::default().fg(Color::White),
                )),
                Line::from(Span::styled(
                    format!("   {}", verification_uri),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "2. Enter the code:",
                    Style::default().fg(Color::White),
                )),
                Line::from(Span::styled(
                    format!("   {}", user_code),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Waiting for authorization...",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "[Enter: open browser & copy code | Esc: cancel]",
                    Style::default().fg(Color::DarkGray),
                )),
            ]
        }
        Some(CopilotAuthStatus::Success) => {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Authentication successful!",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "[Press Enter to continue]",
                    Style::default().fg(Color::DarkGray),
                )),
            ]
        }
        Some(CopilotAuthStatus::Failed(error)) => {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Authentication failed:",
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!("  {}", error),
                    Style::default().fg(Color::Red),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "[Press Enter to go back]",
                    Style::default().fg(Color::DarkGray),
                )),
            ]
        }
        None => {
            vec![Line::from("")]
        }
    };

    let widget = Paragraph::new(content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Magenta))
                .title(" GitHub Copilot Authentication ")
                .title_alignment(ratatui::layout::Alignment::Center),
        )
        .alignment(ratatui::layout::Alignment::Left);

    frame.render_widget(widget, center_chunks[1]);
}

/// Render the workspace access confirmation dialog.
fn render_workspace_confirm(frame: &mut Frame, area: Rect, request: &WorkspaceRequest) {
    // Clear background
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Black)),
        area,
    );

    // Center the dialog
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Min(10),
            Constraint::Percentage(30),
        ])
        .split(area);

    let center_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(15),
            Constraint::Min(50),
            Constraint::Percentage(15),
        ])
        .split(chunks[1]);

    let content = vec![
        Line::from(""),
        Line::from(Span::styled(
            "The agent is requesting access to:",
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", request.args.path.as_str()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(ratatui::style::Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled("Reason:", Style::default().fg(Color::White))),
        Line::from(Span::styled(
            format!("  {}", request.args.reason.as_str()),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from(""),
        Line::from(vec![
            Span::styled("[Y/Enter] ", Style::default().fg(Color::Green)),
            Span::styled("Approve", Style::default().fg(Color::White)),
            Span::raw("    "),
            Span::styled("[N/Esc] ", Style::default().fg(Color::Red)),
            Span::styled("Deny", Style::default().fg(Color::White)),
        ]),
    ];

    let widget = Paragraph::new(content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" Workspace Access Request ")
                .title_alignment(ratatui::layout::Alignment::Center),
        )
        .alignment(ratatui::layout::Alignment::Left);

    frame.render_widget(widget, center_chunks[1]);
}

/// Render MCP server management popup.
fn render_mcp_popup(frame: &mut Frame, area: Rect, app: &App) {
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

                    let status_color = if server.is_connected {
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
                        Span::styled(&server.status, Style::default().fg(status_color)),
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
}

/// Render SSH server management popup.
fn render_ssh_popup(frame: &mut Frame, area: Rect, app: &App) {
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
                                Style::default().fg(Color::Yellow).add_modifier(ratatui::style::Modifier::BOLD)
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
}

/// Render the skill management popup.
fn render_skill_popup(frame: &mut Frame, area: Rect, app: &App) {
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
                        format!("↑ {} more above", scroll),
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
                            format!(" ({}/{})", owner, repo)
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
        }
    }
}

/// Render the AskUser dialog as a bottom overlay.
fn render_ask_user(frame: &mut Frame, area: Rect, state: &AskUserState) {
    let Some(question) = state.current() else {
        return;
    };
    let Some(qs) = state.current_state() else {
        return;
    };

    // Item count: options + Skip + Custom answer
    let opt_count = question.option.len();
    let total_items = opt_count + 2;

    // Calculate overlay height: question(2) + items + custom_input?(1) + help(1) + border(2)
    let custom_line = if qs.custom_mode { 1u16 } else { 0 };
    let max_height = (area.height * 60) / 100;
    let needed = 2 + total_items as u16 + custom_line + 1 + 2;
    let height = needed.min(max_height);

    // Bottom-anchored overlay
    let overlay = Rect::new(area.x, area.y + area.height.saturating_sub(height), area.width, height);
    frame.render_widget(Clear, overlay);

    // Build section tabs line for multi-question
    let mut title_spans: Vec<Span> = Vec::new();
    if state.is_multi() {
        for (i, q) in state.questions.iter().enumerate() {
            let label = if q.section.is_empty() {
                format!(" {} ", i + 1)
            } else {
                format!(" {} ", q.section)
            };
            if i == state.current_question {
                title_spans.push(Span::styled(
                    label,
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ));
            } else {
                title_spans.push(Span::styled(label, Style::default().fg(Color::DarkGray)));
            }
            if i + 1 < state.questions.len() {
                title_spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
            }
        }
    }

    let border_title = if state.is_multi() {
        Line::from(title_spans)
    } else if question.multi_select {
        Line::from(" Multiple Choice ")
    } else {
        Line::from(" Single Choice ")
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(border_title)
        .title_alignment(Alignment::Center);
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);

    // Layout inner: question + items + optional custom input + help
    let mut constraints = vec![
        Constraint::Length(2),        // question text
        Constraint::Min(1),           // items list
    ];
    if qs.custom_mode {
        constraints.push(Constraint::Length(1)); // custom input
    }
    constraints.push(Constraint::Length(1)); // help text

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    // Render question text
    let q_text = Paragraph::new(Line::from(Span::styled(
        &question.question,
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
    )))
    .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(q_text, chunks[0]);

    // Render items: options + Skip + Custom answer
    let visible_height = chunks[1].height as usize;
    let scroll_offset = if qs.cursor >= visible_height {
        qs.cursor - visible_height + 1
    } else {
        0
    };

    let mut items: Vec<ListItem> = Vec::with_capacity(total_items);
    for global_idx in 0..total_items {
        if global_idx < scroll_offset || global_idx >= scroll_offset + visible_height {
            continue;
        }
        let is_cursor = global_idx == qs.cursor;
        let prefix = if is_cursor { "> " } else { "  " };
        let cursor_style = if is_cursor {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        if global_idx < opt_count {
            // Regular option
            let is_selected = qs.selections.contains(&global_idx);
            let checkbox = if question.multi_select {
                if is_selected { "[x] " } else { "[ ] " }
            } else if is_selected {
                "(*) "
            } else {
                "( ) "
            };
            let sel_style = if is_selected {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            items.push(ListItem::new(Line::from(vec![
                Span::styled(prefix, cursor_style),
                Span::styled(checkbox, sel_style),
                Span::styled(&question.option[global_idx], cursor_style),
            ])));
        } else if global_idx == opt_count {
            // Skip
            let skip_style = if qs.skipped {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let mark = if qs.skipped { ">> " } else { "   " };
            items.push(ListItem::new(Line::from(vec![
                Span::styled(prefix, cursor_style),
                Span::styled(mark, skip_style),
                Span::styled("Skip", if is_cursor { cursor_style } else { skip_style }),
            ])));
        } else {
            // Custom answer
            let has_custom = !qs.custom_text.is_empty();
            let custom_style = if has_custom {
                Style::default().fg(Color::Magenta)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let mark = if has_custom { ">> " } else { "   " };
            let label = if has_custom {
                format!("Custom: {}", qs.custom_text)
            } else {
                "Custom answer...".to_string()
            };
            items.push(ListItem::new(Line::from(vec![
                Span::styled(prefix, cursor_style),
                Span::styled(mark, custom_style),
                Span::styled(label, if is_cursor { cursor_style } else { custom_style }),
            ])));
        }
    }
    frame.render_widget(List::new(items), chunks[1]);

    // Custom text input line
    let chunk_idx_help;
    if qs.custom_mode {
        let input_line = Paragraph::new(Line::from(vec![
            Span::styled("  > ", Style::default().fg(Color::Cyan)),
            Span::styled(&qs.custom_text, Style::default().fg(Color::White)),
            Span::styled("_", Style::default().fg(Color::DarkGray)),
        ]));
        frame.render_widget(input_line, chunks[2]);
        chunk_idx_help = 3;
    } else {
        chunk_idx_help = 2;
    }

    // Help text
    let nav = if state.is_multi() { " | Tab/l: next | Shift-Tab/H: prev" } else { "" };
    let help_text = if question.multi_select {
        format!("Space: toggle | Enter: confirm | j/k: navigate{nav} | Esc: skip all")
    } else {
        format!("Enter: select | j/k: navigate{nav} | Esc: skip all")
    };
    let help = Paragraph::new(help_text)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    if chunk_idx_help < chunks.len() {
        frame.render_widget(help, chunks[chunk_idx_help]);
    }
}

/// Render completion overlay near bottom of screen.
fn render_completion_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let max_items = 10;
    let item_count = app.completion.len().min(max_items);
    let height = (item_count + 2) as u16; // +2 for borders

    // Position overlay near bottom, leaving 2 lines for input/status
    let overlay_y = area.y + area.height.saturating_sub(height + 2);
    let overlay_area = Rect {
        x: area.x,
        y: overlay_y,
        width: area.width,
        height,
    };

    // Clear background
    frame.render_widget(Clear, overlay_area);

    // Build list items
    let selected = app.completion.selected();
    let items: Vec<ListItem> = app
        .completion
        .visible_items(max_items)
        .map(|(idx, item)| {
            let is_selected = idx == selected;
            let style = if is_selected {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default().fg(Color::White)
            };

            // Format based on completion mode
            let text = match app.completion.mode {
                CompletionMode::Slash => {
                    format!("{:<15} {}", item.label, item.description)
                }
                CompletionMode::File => item.label.clone(),
                CompletionMode::None => item.label.clone(),
            };

            ListItem::new(text).style(style)
        })
        .collect();

    let title = match app.completion.mode {
        CompletionMode::Slash => "Commands",
        CompletionMode::File => "Files",
        CompletionMode::None => "",
    };

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title),
    );

    frame.render_widget(list, overlay_area);
}

/// Render images inline at their scroll-adjusted positions.
/// Images appear within the content flow and scroll with the text.
fn render_inline_images(
    frame: &mut Frame,
    chat_area: Rect,
    app: &mut App,
    image_positions: &[(String, usize)],
    _scroll: usize,
) {
    use super::streaming_markdown::IMAGE_HEIGHT;
    use ratatui_image::StatefulImage;

    // Early exit if image support was disabled during session
    if app.image_support.is_none() {
        return;
    }

    let view_height = chat_area.height as usize;
    let content_height = app.content_height.get();

    // Calculate padding the same way as render_impl does:
    // When content doesn't fill the screen, padding is added at the TOP
    // to push content to the bottom of the screen.
    let (padding, actual_scroll) = if content_height < view_height {
        // Content fits - padding at top, no scroll
        (view_height - content_height, 0)
    } else {
        // Content exceeds screen - no padding, use scroll
        let max_scroll = content_height - view_height;
        let scroll_val = if app.scroll == usize::MAX {
            max_scroll
        } else {
            app.scroll.min(max_scroll)
        };
        (0, scroll_val)
    };

    tracing::debug!(
        "render_inline_images: {} positions, {} cached, padding={}, actual_scroll={}, content_height={}, view_height={}",
        image_positions.len(),
        app.image_cache.len(),
        padding,
        actual_scroll,
        content_height,
        view_height
    );

    for (url, line_index) in image_positions {
        let is_cached = app.image_cache.contains_key(url);
        tracing::debug!(
            "Image at line {}: url={}, cached={}",
            line_index,
            url,
            is_cached
        );

        // Calculate screen position accounting for both padding and scroll
        // line_index is in the unpadded content coordinate system
        // Screen position = padding + line_index - actual_scroll

        // Skip if the image is scrolled above the visible area
        if *line_index < actual_scroll {
            // Image might be partially visible at top
            let lines_hidden = actual_scroll - line_index;
            if lines_hidden >= IMAGE_HEIGHT {
                // Completely scrolled past
                continue;
            }
            // Partial visibility at top - handled below
        }

        // Screen line position (accounting for padding and scroll)
        let screen_line = padding + line_index.saturating_sub(actual_scroll);

        // Skip if completely below visible area
        if screen_line >= view_height {
            continue;
        }

        // Get the cached image state
        let Some(state) = app.image_cache.get_mut(url) else {
            // Image not yet fetched
            continue;
        };

        // Calculate visible portion of the image
        // The image starts at line_index + 1 (skip the header line "🖼 Alt [loading...]")
        // and spans IMAGE_HEIGHT - 1 lines
        let image_content_line = screen_line + 1; // Skip header line

        // Skip if header line is at or past the bottom
        if image_content_line >= view_height {
            continue;
        }

        let image_start_y = chat_area.y + image_content_line as u16;
        let available_height = (chat_area.y + chat_area.height).saturating_sub(image_start_y);

        // Skip if no space available
        if available_height == 0 {
            continue;
        }

        // Calculate image area with proper bounds
        // IMAGE_HEIGHT - 1 because we skip the header line
        let image_height = ((IMAGE_HEIGHT - 1) as u16).min(available_height);
        let indent = 2u16; // Indent from left edge

        let image_area = Rect::new(
            chat_area.x + indent,
            image_start_y,
            chat_area.width.saturating_sub(indent * 2),
            image_height,
        );

        tracing::debug!(
            "Rendering image at area: x={}, y={}, w={}, h={} (screen_line={}, padding={}, line_index={})",
            image_area.x,
            image_area.y,
            image_area.width,
            image_area.height,
            screen_line,
            padding,
            line_index
        );

        // Skip if area is too small
        if image_area.width < 4 || image_area.height < 2 {
            continue;
        }

        // Render the image
        let image_widget = StatefulImage::default();
        frame.render_stateful_widget(image_widget, image_area, &mut state.protocol);
    }
}
