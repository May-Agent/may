//! Provider / model / preferences configuration screen and its sections.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};

use super::super::super::app::App;
use super::super::super::state::{ConfigEditMode, ConfigSection};
use super::{centered_rect, char_to_byte_idx};

pub(super) fn render_config_screen(frame: &mut Frame, area: Rect, app: &App) {
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
pub(super) fn render_providers_section(frame: &mut Frame, area: Rect, app: &App) {
    let state = &app.config_state;

    // Build provider list
    let mut items: Vec<ListItem> = state
        .provider_ids
        .iter()
        .enumerate()
        .map(|(idx, id)| {
            let prefix = if idx == state.selected { "> " } else { "  " };
            let instance = app.config.provider.get(id);
            let status = instance.map_or_else(
                || "(unknown)".to_string(),
                |inst| {
                    // For custom providers, show base_url
                    inst.base_url.as_ref().map_or_else(
                        || {
                            inst.api_key.as_ref().map_or_else(
                                || {
                                    if id == "copilot" {
                                        "(OAuth)".to_string()
                                    } else {
                                        "(no API key)".to_string()
                                    }
                                },
                                |key| {
                                    let masked: String =
                                        key.chars().take(4).collect::<String>() + "****";
                                    format!("{masked} (configured)")
                                },
                            )
                        },
                        |base_url| {
                            // Truncate URL safely (unicode-safe)
                            const URL_DISPLAY_MAX_CHARS: usize = 27;

                            match base_url.char_indices().nth(URL_DISPLAY_MAX_CHARS) {
                                Some((pos, _)) => format!("{}...", &base_url[..pos]),
                                None => base_url.clone(),
                            }
                        },
                    )
                },
            );
            let text = format!("{prefix}{id:<20} {status}");
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
    items.push(ListItem::new(format!("{add_prefix}+ Add provider...")).style(add_style));

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
                ListItem::new(format!("{prefix}{name}")).style(style)
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
        items[state.selected] = ListItem::new(format!("  API Key: {display}"))
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
pub(super) fn render_models_section(frame: &mut Frame, area: Rect, app: &App) {
    let state = &app.config_state;
    let config = &app.config;

    let tier_names = ["Advanced", "Balanced", "Fast", "Image Generation"];
    let tier_values: Vec<String> = vec![
        config
            .models
            .advanced
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        config
            .models
            .balanced
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        config
            .models
            .fast
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        config
            .models
            .image_generation
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    ];

    let mut items: Vec<ListItem> = tier_names
        .iter()
        .zip(tier_values.iter())
        .enumerate()
        .map(|(idx, (name, value))| {
            let prefix = if idx == state.selected { "> " } else { "  " };
            let display_value = if value.is_empty() { "(not set)" } else { value };
            let text = format!("{prefix}{name:<20} {display_value}");
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
pub(super) fn render_preferences_section(frame: &mut Frame, area: Rect, app: &App) {
    let state = &app.config_state;
    let config = &app.config;

    let pref_names = [
        "Max iterations",
        "Warn on model change",
        "Show decode speed",
    ];
    let pref_values: Vec<String> = vec![
        if config.max_iterations == 0 {
            "unlimited".to_string()
        } else {
            config.max_iterations.to_string()
        },
        config.preferences.warn_on_model_change.to_string(),
        config.preferences.show_decode_speed.to_string(),
    ];

    let mut items: Vec<ListItem> = pref_names
        .iter()
        .zip(pref_values.iter())
        .enumerate()
        .map(|(idx, (name, value))| {
            let prefix = if idx == state.selected { "> " } else { "  " };
            let text = format!("{prefix}{name:<25} {value}");
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
    if let Some((input, cursor)) = state.edit_text()
        && state.selected == 0
    {
        let cursor_pos = char_to_byte_idx(input, cursor);
        let display = format!("{}|{}", &input[..cursor_pos], &input[cursor_pos..]);
        items[0] = ListItem::new(format!("  Max iterations:        {display}"))
            .style(Style::default().fg(Color::Yellow));
    }

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title("General preferences"),
    );
    frame.render_widget(list, area);
}
