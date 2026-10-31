//! Main layout for the TUI.

mod config;
mod pickers;
mod popups;
mod sidebar;

use self::config::render_config_screen;
use self::pickers::{render_file_picker_popup, render_profile_select_popup, render_session_popup};
use self::popups::{render_mcp_popup, render_skill_popup, render_ssh_popup};

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use ratatui::Frame;

use super::super::app::{App, ImageRegion};
use super::super::completion::CompletionMode;
use super::super::state::{
    AppMode, AskUserState, BtwState, ChatgptAuthStatus, CopilotAuthStatus, JobDetailState,
    JobDetailTarget, RewindState,
};
use super::chat::{self, InputContext, QueueContext, StatusContext, UnifiedRenderContext};

/// Convert character index to byte index for safe string slicing.
pub(super) fn char_to_byte_idx(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(i, _)| i)
}

/// Render the main layout.
pub fn render(frame: &mut Frame, app: &mut App) {
    let render_start = std::time::Instant::now();
    let area = frame.area();

    // Full-screen flows. Everything else is a bottom sheet over the
    // still-visible chat (see `sheet_height`).
    match &app.mode {
        // The settings screen is a multi-pane editor (section tabs, provider
        // list, per-field editors, model tiers) that needs the whole screen;
        // a sheet would leave its panes a few rows tall.
        AppMode::Config => {
            render_config_screen(frame, area, app);
            return;
        }
        // Picks a model for a tier *inside* the settings screen, over it.
        AppMode::TierModelPicker => {
            render_config_screen(frame, area, app);
            render_tier_model_picker(frame, area, app);
            return;
        }
        // The OAuth device/browser flows are steps of the settings screen
        // (entered from a provider there and returning to it), so they keep
        // its full-screen frame rather than popping a sheet over the chat.
        AppMode::CopilotAuth => {
            render_copilot_auth_screen(frame, area, app);
            return;
        }
        AppMode::ChatgptAuth => {
            render_chatgpt_auth_screen(frame, area, app);
            return;
        }
        _ => {}
    }

    // The right sidebar takes a column of wide terminals; the chat and its
    // sheets live in the rest.
    let (column, sidebar_area) = if app.sidebar.visible_at(area.width) {
        let width = crate::tui::sidebar::SidebarState::width_for(area.width);
        (
            Rect {
                width: area.width - width,
                ..area
            },
            Some(Rect {
                x: area.x + area.width - width,
                width,
                ..area
            }),
        )
    } else {
        (area, None)
    };
    // A sheet replaces the input line at the bottom of the chat column and
    // takes only the rows it needs; the chat keeps the rows above it.
    let sheet_area = sheet_height(app, column).map(|height| Rect {
        y: column.y + column.height - height,
        height,
        ..column
    });

    // Scan chat content for images and request fetches for any not yet cached
    let t = std::time::Instant::now();
    app.scan_and_fetch_images();
    app.frame_stats.image_scan = t.elapsed();

    // Build queue message previews
    let queue_messages: Vec<String> = app
        .queue
        .iter()
        .map(|message| {
            let label = match message.delivery_mode {
                crate::session::PendingUserMessageMode::Steer => "[steer] ",
                crate::session::PendingUserMessageMode::Queue => "[queue] ",
            };
            let mut preview = String::from(label);
            preview.push_str(message.content.as_str());
            preview
        })
        .collect();

    // Build usage info for status
    let usage_info = app.usage.as_ref().and_then(|u| {
        u.prompt_tokens
            .map(|pt| (u64::from(pt), app.context_window.map(u64::from)))
    });

    // Decode rate for the status bar: live while a response streams, then
    // frozen at the finished turn's average until the next turn resets it.
    let decode_rate = app
        .config
        .preferences
        .show_decode_speed
        .then(|| app.decode_meter.rate())
        .flatten();

    let images_supported = app.supports_images();
    let chat_area = Rect {
        height: column.height - sheet_area.map_or(0, |sheet| sheet.height),
        ..column
    };
    app.chat_area = chat_area;

    // Running background entries for the status bar: tracked subagents first,
    // then running terminal jobs.
    let mut background_items: Vec<chat::StatusBackgroundItem> = app
        .subagents
        .iter()
        .cloned()
        .map(|run| chat::StatusBackgroundItem {
            elapsed_secs: run.elapsed_secs(),
            label: run.agent,
            subagent: true,
        })
        .collect();
    background_items.extend(
        app.jobs
            .iter()
            .filter(|job| matches!(job.status, aither_sandbox::JobStatus::Running))
            .map(|job| chat::StatusBackgroundItem {
                label: job
                    .script
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(24)
                    .collect(),
                subagent: false,
                elapsed_secs: job.started_at.elapsed().as_secs(),
            }),
    );

    // Working directory shown in the status bar (`~` for the user home).
    let cwd_label = app.cwd_label();

    // Image blocks intersecting the view, collected while rendering.
    let mut image_positions: Vec<chat::ImagePlacement> = Vec::new();

    let scroll = app.scroll;
    let model_display = app.model_display();
    let status_message = app.status_text().map(str::to_string);

    // Render everything in a single unified scrollable area
    app.frame_stats.chat = chat::render_unified(
        frame,
        chat_area,
        &app.chat,
        &mut app.chat_layout,
        scroll,
        UnifiedRenderContext {
            selected_tool: app.selected_tool.as_deref(),
            content_height_out: Some(&app.content_height),
            view_height_out: Some(&app.view_height),
            input_ctx: sheet_area.is_none().then_some(InputContext {
                input: &app.input,
                cursor: app.cursor,
                attachments: &app.attachments,
            }),
            input_caret_active: matches!(app.mode, AppMode::Chat),
            queue_ctx: if queue_messages.is_empty() {
                None
            } else {
                Some(QueueContext {
                    messages: &queue_messages,
                })
            },
            status_ctx: Some(StatusContext {
                model: &model_display,
                usage: usage_info,
                queue_count: app.queue.len(),
                open_questions: app.open_question_count,
                background: &background_items,
                decode_rate,
                status_message: status_message.as_deref(),
                cwd: cwd_label.as_str(),
                activity: app.processing.then_some(chat::ActivityContext {
                    phase: app.activity_phase,
                    tokens: app.decode_meter.tokens(),
                }),
                spinner_frame: app.spinner_frame,
            }),
            images_supported,
            // The sidebar shows the task tree; the inline block would repeat it.
            hide_todos: sidebar_area.is_some(),
            image_positions_out: Some(&mut image_positions),
            spinner_frame: app.spinner_frame,
        },
    );
    // The plain mirror is read back from the cached layout on demand.
    app.frame_stats.plain_lines = std::time::Duration::ZERO;

    let t = std::time::Instant::now();
    if app.selection.is_some() {
        render_text_selection_overlay(frame, chat_area, app);
    }

    // Render images as overlays at their scroll-adjusted positions. Images
    // never paint under a popup or the zoom view: a protocol image drawn
    // and then overwritten in the same frame loses its one-time kitty
    // transmission, and sixel/iTerm2 pixels would bleed through the popup.
    app.image_regions.clear();
    if images_supported
        && app.image_support.is_some()
        && !image_positions.is_empty()
        && !matches!(app.mode, AppMode::ImageView(_))
    {
        let mut occluders: Vec<Rect> = Vec::new();
        if let Some(rect) = completion_overlay_rect(app, chat_area) {
            occluders.push(rect);
        }
        render_inline_images(
            frame,
            chat_area,
            &mut app.image_cache,
            &mut app.image_regions,
            &image_positions,
            &occluders,
        );
    }

    if let Some(sidebar) = sidebar_area {
        sidebar::render_sidebar(frame, sidebar, app);
    }
    if let Some(sheet) = sheet_area {
        render_sheet(frame, sheet, app);
    }

    // Render completion overlay if active (always on top)
    if app.completion.is_active() && !app.completion.is_empty() {
        render_completion_overlay(frame, chat_area, app);
    }

    // The zoomed image overlay sits above everything else.
    if matches!(app.mode, AppMode::ImageView(_)) {
        render_image_view(frame, area, app);
    } else {
        app.image_view_area = None;
    }

    app.frame_stats.overlay = t.elapsed();
    app.frame_stats.total = render_start.elapsed();
}

/// Rows the chat keeps above a sheet whenever the column has them.
const MIN_CHAT_ROWS: u16 = 6;

/// Height of the bottom sheet the current mode shows in `column`, or
/// `None` for modes without one. Each sheet asks for the rows its content
/// needs; the result is capped so the chat keeps at least
/// [`MIN_CHAT_ROWS`] (and at most 60% of the column goes to the sheet,
/// 80% for the tool and job detail views), and sheets whose content is
/// longer scroll inside.
fn sheet_height(app: &App, column: Rect) -> Option<u16> {
    let full = column.height;
    let pct =
        |percent: u16| u16::try_from(u32::from(full) * u32::from(percent) / 100).unwrap_or(full);
    let rows = |count: usize| u16::try_from(count).unwrap_or(u16::MAX);
    let (wanted, cap) = match &app.mode {
        AppMode::AskUser(state) => (ask_user_rows(state, column.width), pct(60)),
        AppMode::SessionSelect => (
            rows(app.filtered_sessions().len() * 2 + 10).max(14),
            pct(70),
        ),
        AppMode::FilePicker => (
            rows(
                app.file_picker
                    .as_ref()
                    .map_or(0, crate::tui::ui::file_picker::FilePicker::match_count)
                    + 6,
            )
            .max(10),
            pct(60),
        ),
        AppMode::Help => (rows(HELP_TEXT.lines().count() + 2), pct(70)),
        AppMode::ToolDetail => (full, pct(80)),
        AppMode::ModelSelect => (rows(app.model_select_list.len() + 2).min(12), pct(60)),
        AppMode::McpManage | AppMode::SshManage | AppMode::SkillManage => {
            (pct(60).max(12), pct(70))
        }
        AppMode::ProfileSelect => (rows(app.profiles.len() + 2), pct(60)),
        AppMode::Rewind(state) => (
            if state.options_for.is_some() {
                9
            } else {
                rows(state.targets.len() * 2 + 4).min(18)
            },
            pct(60),
        ),
        AppMode::Jobs => (
            rows(crate::tui::reducer::jobs_rows(app).len() * 2 + 3).max(6),
            pct(60),
        ),
        AppMode::JobDetail(_) => (pct(80), pct(80)),
        AppMode::Btw(state) => (if !state.loading && !state.done { 5 } else { 14 }, pct(60)),
        AppMode::PermissionConfirm(state) => (
            rows(confirm_body_rows(state.request.script.as_str(), column.width) + 8),
            pct(70),
        ),
        AppMode::WorkspaceConfirm(state) => (
            rows(confirm_body_rows(&state.reason, column.width) + 9),
            pct(70),
        ),
        AppMode::Chat
        | AppMode::ImageView(_)
        | AppMode::Config
        | AppMode::TierModelPicker
        | AppMode::CopilotAuth
        | AppMode::ChatgptAuth => return None,
    };
    Some(fit_sheet(wanted, cap, full))
}

/// Rows a sheet that wants `wanted` rows gets in a `full`-row column: at
/// most `cap`, leaving the chat [`MIN_CHAT_ROWS`] when the column has
/// them; a tiny column still gives the sheet up to five rows.
fn fit_sheet(wanted: u16, cap: u16, full: u16) -> u16 {
    let limit = cap
        .min(full.saturating_sub(MIN_CHAT_ROWS))
        .max(full.min(5))
        .max(3);
    wanted.clamp(3, limit).min(full)
}

/// Wrapped rows of a confirm dialog's free-text field.
fn confirm_body_rows(text: &str, width: u16) -> usize {
    let width = usize::from(width.saturating_sub(4)).max(1);
    text.lines()
        .map(|line| (UnicodeWidthStr::width(line) + 2).div_ceil(width).max(1))
        .sum::<usize>()
        .max(1)
}

/// Render the current mode's bottom sheet into `sheet`.
fn render_sheet(frame: &mut Frame, sheet: Rect, app: &App) {
    frame.render_widget(Clear, sheet);
    match &app.mode {
        AppMode::AskUser(state) => render_ask_user(frame, sheet, state),
        AppMode::SessionSelect => render_session_popup(frame, sheet, app),
        AppMode::FilePicker => render_file_picker_popup(frame, sheet, app),
        AppMode::Help => render_help_popup(frame, sheet, app.help_scroll),
        AppMode::ToolDetail => render_tool_detail_popup(frame, sheet, app),
        AppMode::ModelSelect => render_model_select_popup(frame, sheet, app),
        AppMode::McpManage => render_mcp_popup(frame, sheet, app),
        AppMode::SshManage => render_ssh_popup(frame, sheet, app),
        AppMode::SkillManage => render_skill_popup(frame, sheet, app),
        AppMode::ProfileSelect => render_profile_select_popup(frame, sheet, app),
        AppMode::Rewind(state) => render_rewind_popup(frame, sheet, state),
        AppMode::Jobs => render_jobs_panel(frame, sheet, app),
        AppMode::JobDetail(state) => render_job_detail(frame, sheet, state),
        AppMode::Btw(state) => render_btw_popup(frame, sheet, state),
        AppMode::PermissionConfirm(state) => render_permission_confirm(frame, sheet, state),
        AppMode::WorkspaceConfirm(state) => {
            render_workspace_confirm(frame, sheet, &state.path, &state.reason);
        }
        _ => {}
    }
}

/// Render the zoomed image overlay: the image fitted into a large centered
/// popup with its path/URL caption underneath. Esc or a click outside the
/// image closes it. It stays an overlay over the whole screen rather than a
/// sheet: it exists to show the image as large as the terminal allows.
fn render_image_view(frame: &mut Frame, area: Rect, app: &mut App) {
    use ratatui_image::Resize;

    let AppMode::ImageView(state) = &app.mode else {
        return;
    };
    let (key, caption) = (state.key.clone(), state.caption.clone());
    if !app.image_cache.contains_key(&key) {
        app.mode = AppMode::Chat;
        return;
    }

    let popup = centered_rect(80, 75, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Image (Esc or click outside to close) ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let image_area = rows[0];
    let state = app.image_cache.get_mut(&key).expect("checked above");
    let fitted = state.protocol.size_for(Resize::Fit(None), image_area);
    let draw_area = Rect::new(
        image_area.x + image_area.width.saturating_sub(fitted.width) / 2,
        image_area.y + image_area.height.saturating_sub(fitted.height) / 2,
        fitted.width.min(image_area.width),
        fitted.height.min(image_area.height),
    );
    app.image_view_area = Some(draw_area);
    super::image::render_image(frame, draw_area, state);

    let caption = Paragraph::new(caption)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    frame.render_widget(caption, rows[1]);
}

fn render_text_selection_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let Some(sel) = app.selection else {
        return;
    };
    let rows = app.chat_layout.plain_len();
    if sel.is_empty() || rows == 0 {
        return;
    }

    let (start, end) = sel.normalized();
    let start_line = start.line.min(rows.saturating_sub(1));
    let end_line = end.line.min(rows.saturating_sub(1));

    let base_scroll = app.effective_scroll();
    let padding = app.chat_layout.top_padding();
    // Only the selected rows inside the view are painted.
    let first_visible = base_scroll.saturating_sub(padding);
    let last_visible = (base_scroll + area.height as usize).saturating_sub(padding);
    for line_idx in start_line.max(first_visible)..=end_line.min(last_visible) {
        let visual_line = padding.saturating_add(line_idx);
        if visual_line < base_scroll {
            continue;
        }

        let y_in_view = visual_line - base_scroll;
        if y_in_view >= area.height as usize {
            continue;
        }

        let line = app.chat_layout.plain_line(line_idx).unwrap_or_default();
        let char_count = line.chars().count();
        let from = if line_idx == start_line {
            start.column.min(char_count)
        } else {
            0
        };
        let to = if line_idx == end_line {
            end.column.min(char_count)
        } else {
            char_count
        };
        if to <= from {
            continue;
        }

        let x0 = display_width_prefix(line, from).min(area.width as usize);
        let x1 = display_width_prefix(line, to).min(area.width as usize);
        if x1 <= x0 {
            continue;
        }

        let rect = Rect::new(
            area.x + u16::try_from(x0).unwrap_or(u16::MAX),
            area.y + u16::try_from(y_in_view).unwrap_or(u16::MAX),
            u16::try_from(x1 - x0).unwrap_or(u16::MAX),
            1,
        );
        frame
            .buffer_mut()
            .set_style(rect, Style::default().bg(Color::Rgb(70, 90, 130)));
    }
}

pub(super) fn display_width_prefix(text: &str, char_count: usize) -> usize {
    text.chars()
        .take(char_count)
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(1))
        .sum()
}

/// Help text content.
const HELP_TEXT: &str = include_str!("../help.txt");

/// Render the help sheet; `scroll` rows of it are scrolled off the top.
fn render_help_popup(frame: &mut Frame, area: Rect, scroll: u16) {
    let max_scroll = u16::try_from(HELP_TEXT.lines().count())
        .unwrap_or(u16::MAX)
        .saturating_sub(area.height.saturating_sub(2));
    let help = Paragraph::new(HELP_TEXT)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title("Help (↑↓ scroll · ? or Esc to close)"),
        )
        .style(Style::default().fg(Color::White))
        .wrap(ratatui::widgets::Wrap { trim: false })
        .scroll((scroll.min(max_scroll), 0));

    frame.render_widget(help, area);
}

/// Render tool detail popup (full screen).
fn render_tool_detail_popup(frame: &mut Frame, area: Rect, app: &App) {
    let popup_area = area;

    // Get the selected tool
    if let Some(ref tool_id) = app.selected_tool
        && let Some(display) = app.chat.get_tool(tool_id)
    {
        chat::render_tool_fullscreen(frame, popup_area, display, app.tool_detail_scroll);
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
    let popup_width = (area.width * 60 / 100).clamp(40, 80);
    let popup_height = (area.height * 50 / 100).clamp(10, 20);
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
    let caret_x = chunks[0].x
        + 1
        + u16::try_from(UnicodeWidthStr::width(app.tier_picker_query.as_str())).unwrap_or(u16::MAX);
    frame.set_cursor_position((
        caret_x.min(chunks[0].right().saturating_sub(2)),
        chunks[0].y + 1,
    ));

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
                let model_id = fetched.map_or("", |m| m.id.as_str());
                // Check if model matches tier (for visual indicator)
                let info = aither_models::lookup(model_id);
                let tier_match = app
                    .tier_picker_tier
                    .and_then(|tier| info.map(|i| crate::config::get_model_tier(i.id()) == tier))
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
                ListItem::new(format!("{prefix}{model_id}")).style(style)
            })
            .collect();

        let title = if total > 0 {
            format!(" {tier_name} ({total}) ")
        } else {
            format!(" {tier_name} (no models) ")
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
    let title = format!(" Select {tier_name} Model ");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    frame.render_widget(block, popup_area);
}

/// Render model select popup (anchored near input area).
fn render_model_select_popup(frame: &mut Frame, area: Rect, app: &App) {
    let popup_area = area;

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
            let current = app.current_model.as_deref().is_some_and(|m| m == model);
            let suffix = if current { " (current)" } else { "" };
            let text = format!("{prefix}{model}{suffix}");
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

/// Render the rewind picker. Stage one lists the session's user prompts
/// with the file changes the turn each started produced, plus a trailing
/// `(current)` entry; choosing a prompt opens stage two, which picks the
/// restoration scope (conversation only, or conversation and code).
fn render_rewind_popup(frame: &mut Frame, area: Rect, state: &RewindState) {
    if let Some(prompt_index) = state.options_for {
        render_rewind_scope_popup(frame, area, state, prompt_index);
        return;
    }

    let popup_area = area;

    let selected_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let normal_style = Style::default().fg(Color::White);
    let mut items: Vec<ListItem> = Vec::with_capacity(state.targets.len() + 1);
    for (idx, target) in state.targets.iter().enumerate() {
        let prefix = if idx == state.selected { "> " } else { "  " };
        let style = if idx == state.selected {
            selected_style
        } else {
            normal_style
        };
        items.push(ListItem::new(vec![
            Line::from(Span::styled(
                format!("{prefix}{}", prompt_preview(&target.content)),
                style,
            )),
            Line::from(Span::styled(
                format!(
                    "    ± {} files (+{} −{})",
                    target.files, target.additions, target.deletions
                ),
                Style::default().fg(Color::DarkGray),
            )),
        ]));
    }
    items.push(ListItem::new(Line::from(Span::styled(
        if state.selected == state.targets.len() {
            "> (current)"
        } else {
            "  (current)"
        },
        if state.selected == state.targets.len() {
            selected_style
        } else {
            Style::default().fg(Color::DarkGray)
        },
    ))));

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title("Rewind to before… (Enter select, Esc cancel)"),
    );
    frame.render_widget(list, popup_area);
}

/// First line of a prompt, truncated to one display row.
fn prompt_preview(content: &str) -> String {
    const MAX_CHARS: usize = 72;
    let first = content.lines().next().unwrap_or_default().trim();
    if first.chars().count() > MAX_CHARS {
        let mut preview: String = first.chars().take(MAX_CHARS - 1).collect();
        preview.push('…');
        preview
    } else {
        first.to_string()
    }
}

/// The two restoration scopes the rewind picker offers, with their hint line.
const REWIND_SCOPE_OPTIONS: &[(&str, &str)] = &[
    (
        "Rewind conversation",
        "messages only; keep the files as they are",
    ),
    (
        "Rewind conversation and code",
        "also restore tracked files to this prompt's checkpoint",
    ),
];

/// Stage two of the rewind picker: how much of the turn to restore.
fn render_rewind_scope_popup(
    frame: &mut Frame,
    area: Rect,
    state: &RewindState,
    prompt_index: usize,
) {
    let popup_area = area;

    let items: Vec<ListItem> = REWIND_SCOPE_OPTIONS
        .iter()
        .enumerate()
        .map(|(idx, (label, hint))| {
            let selected = idx == state.option_selected;
            ListItem::new(vec![
                Line::from(Span::styled(
                    format!("{}{}", if selected { "> " } else { "  " }, label),
                    if selected {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    },
                )),
                Line::from(Span::styled(
                    format!("    {hint}"),
                    Style::default().fg(Color::DarkGray),
                )),
            ])
        })
        .collect();

    let prompt = state
        .targets
        .get(prompt_index)
        .map_or_else(String::new, |t| prompt_preview(&t.content));
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(format!("{prompt} (Enter restore, Esc back)")),
    );
    frame.render_widget(list, popup_area);
}

/// Jobs panel: bottom-anchored drawer listing running subagents and
/// background terminal jobs.
fn render_jobs_panel(frame: &mut Frame, area: Rect, app: &App) {
    use crate::tui::reducer::{JobRow, jobs_rows};

    let rows = jobs_rows(app);
    let panel_area = area;

    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(idx, row)| {
            let prefix = if idx == app.jobs_selected { "> " } else { "  " };
            let selected = idx == app.jobs_selected;
            let title_style = if selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let (title, detail) = match row {
                JobRow::Subagent(run) => {
                    let preview: String = run
                        .prompt
                        .trim()
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(48)
                        .collect();
                    (
                        format!("{prefix}◉ subagent {}", run.agent),
                        format!(
                            "    {} · {} tok (in {}, out {}) · {}",
                            chat::format_elapsed(run.elapsed_secs()),
                            chat::format_number(
                                usize::try_from(run.total_tokens()).unwrap_or(usize::MAX)
                            ),
                            chat::format_number(
                                usize::try_from(run.prompt_tokens).unwrap_or(usize::MAX)
                            ),
                            chat::format_number(
                                usize::try_from(run.completion_tokens).unwrap_or(usize::MAX)
                            ),
                            preview,
                        ),
                    )
                }
                JobRow::Job(job) => {
                    let (icon, status) = match &job.status {
                        aither_sandbox::JobStatus::Running => ("●", "running".to_string()),
                        aither_sandbox::JobStatus::Completed { exit_code } => {
                            ("✓", format!("exit {exit_code}"))
                        }
                        aither_sandbox::JobStatus::Failed { error } => {
                            ("✗", format!("failed: {error}"))
                        }
                        aither_sandbox::JobStatus::Killed => ("■", "killed".to_string()),
                    };
                    let first_line = job.script.lines().next().unwrap_or("");
                    let cmd: String = first_line.chars().take(60).collect();
                    (
                        format!("{prefix}{icon} pid {} · {cmd}", job.pid),
                        format!(
                            "    {status} · {} · task {}",
                            chat::format_elapsed(job.started_at.elapsed().as_secs()),
                            job.task_id
                        ),
                    )
                }
            };
            ListItem::new(vec![
                Line::from(Span::styled(title, title_style)),
                Line::from(Span::styled(detail, Style::default().fg(Color::DarkGray))),
            ])
        })
        .collect();

    let title = if rows.is_empty() {
        "Jobs & subagents — none running (Esc close)"
    } else {
        "Jobs & subagents (↑↓ select · Enter open · x stop · r refresh · Esc close)"
    };
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title),
    );
    frame.render_widget(list, panel_area);
}

/// Job/subagent detail: bottom drawer with live stats or buffered output.
fn render_job_detail(frame: &mut Frame, area: Rect, state: &JobDetailState) {
    let (title, body): (String, Vec<Line<'_>>) = match state.target {
        JobDetailTarget::Job(pid) => {
            let title = state.job.as_ref().map_or_else(
                || format!("job {pid} — gone"),
                |job| {
                    let status = match &job.status {
                        aither_sandbox::JobStatus::Running => format!(
                            "running · {}",
                            chat::format_elapsed(job.started_at.elapsed().as_secs())
                        ),
                        aither_sandbox::JobStatus::Completed { exit_code } => {
                            format!("exit {exit_code}")
                        }
                        aither_sandbox::JobStatus::Failed { error } => format!("failed: {error}"),
                        aither_sandbox::JobStatus::Killed => "killed".to_string(),
                    };
                    format!(
                        "job {} · {} — {status}",
                        job.pid,
                        job.script
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(48)
                            .collect::<String>(),
                    )
                },
            );
            let mut lines: Vec<Line<'_>> = Vec::new();
            if let Some((stdout, stderr)) = &state.output {
                lines.extend(
                    String::from_utf8_lossy(stdout)
                        .lines()
                        .map(|line| Line::from(line.to_string())),
                );
                if !stderr.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "── stderr ──".to_string(),
                        Style::default().fg(Color::DarkGray),
                    )));
                    lines.extend(
                        String::from_utf8_lossy(stderr)
                            .lines()
                            .map(|line| Line::from(line.to_string())),
                    );
                }
            }
            if lines.is_empty() {
                lines.push(Line::from(Span::styled(
                    "(no buffered output)",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            (title, lines)
        }
        JobDetailTarget::Subagent(_) => state.subagent.as_ref().map_or_else(
            || {
                (
                    "subagent — gone".to_string(),
                    vec![Line::from(Span::styled(
                        "(finished)",
                        Style::default().fg(Color::DarkGray),
                    ))],
                )
            },
            |run| {
                let status = if state.finished {
                    "finished"
                } else {
                    "running"
                };
                let title = format!(
                    "subagent {} — {status} · {} · {} tok (in {}, out {}) · {} req",
                    run.agent,
                    chat::format_elapsed(run.elapsed_secs()),
                    chat::format_number(usize::try_from(run.total_tokens()).unwrap_or(usize::MAX)),
                    chat::format_number(usize::try_from(run.prompt_tokens).unwrap_or(usize::MAX)),
                    chat::format_number(
                        usize::try_from(run.completion_tokens).unwrap_or(usize::MAX)
                    ),
                    run.requests,
                );
                let mut lines = vec![Line::from(Span::styled(
                    "prompt:",
                    Style::default().fg(Color::DarkGray),
                ))];
                lines.extend(run.prompt.lines().map(|line| Line::from(line.to_string())));
                (title, lines)
            },
        ),
    };

    let panel_area = area;
    let inner_height = usize::from(panel_area.height.saturating_sub(2));
    let scroll = state.scroll.min(body.len().saturating_sub(inner_height));
    let scroll = u16::try_from(scroll).unwrap_or(u16::MAX);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(
            "{title} (Esc back · ↑↓ scroll · x stop · r refresh)"
        ));
    frame.render_widget(
        Paragraph::new(body).block(block).scroll((scroll, 0)),
        panel_area,
    );
}

fn render_btw_popup(frame: &mut Frame, area: Rect, state: &BtwState) {
    let awaiting_question = !state.loading && !state.done;
    let popup_area = area;

    let title = if awaiting_question {
        "btw — side question (not in history)"
    } else {
        "btw — side question"
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let mut lines: Vec<Line> = Vec::new();
    if awaiting_question {
        lines.push(Line::from(Span::styled(
            format!("> {}", state.input),
            Style::default().fg(Color::White),
        )));
        // IME anchor at the caret; approximate the wrap for long input.
        let offset = 2 + UnicodeWidthStr::width(state.input.as_str());
        let row_w = usize::from(inner.width).max(1);
        let caret_row = usize::from(inner.y)
            .saturating_add(offset / row_w)
            .min(usize::from(inner.bottom()).saturating_sub(1));
        let caret_col = usize::from(inner.x) + offset % row_w;
        frame.set_cursor_position((
            u16::try_from(caret_col).unwrap_or(u16::MAX),
            u16::try_from(caret_row).unwrap_or(u16::MAX),
        ));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Ask a quick question about this session — Enter ask · Esc close",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("> {}", state.input),
            Style::default().fg(Color::Yellow),
        )));
        lines.push(Line::from(""));
        if state.answer.is_empty() && state.loading {
            lines.push(Line::from(Span::styled(
                "thinking…",
                Style::default().fg(Color::DarkGray),
            )));
        }
        for line in state.answer.lines() {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::White),
            )));
        }
        if state.loading {
            lines.push(Line::from(Span::styled(
                "▌",
                Style::default().fg(Color::White),
            )));
        }
        if let Some(error) = &state.error {
            lines.push(Line::from(Span::styled(
                format!("error: {error}"),
                Style::default().fg(Color::Red),
            )));
        }
        if state.done {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Esc/q close · ↑↓ scroll",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    let paragraph = Paragraph::new(lines)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .scroll((state.scroll, 0));
    frame.render_widget(paragraph, inner);
}

/// Helper to create a centered rect.
pub(super) fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
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
                    format!("   {verification_uri}"),
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
                    format!("   {user_code}"),
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
                    format!("  {error}"),
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

fn render_chatgpt_auth_screen(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Min(14),
            Constraint::Percentage(20),
        ])
        .split(area);

    let center_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(10),
            Constraint::Min(60),
            Constraint::Percentage(10),
        ])
        .split(chunks[1]);

    let content = match &app.chatgpt_auth_status {
        Some(ChatgptAuthStatus::Working) => {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Contacting OpenAI...",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(""),
            ]
        }
        Some(ChatgptAuthStatus::WaitingForCallback { url }) => {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "1. Open this URL in your browser and authorize:",
                    Style::default().fg(Color::White),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    format!("   {url}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "2. The browser will redirect to localhost:1455 automatically.",
                    Style::default().fg(Color::White),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Waiting for authorization callback...",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "[Enter: open browser | Esc: cancel]",
                    Style::default().fg(Color::DarkGray),
                )),
            ]
        }
        Some(ChatgptAuthStatus::WaitingForUser {
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
                    format!("   {verification_uri}"),
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
                    format!("   {user_code}"),
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
        Some(ChatgptAuthStatus::Success) => {
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
        Some(ChatgptAuthStatus::Failed(error)) => {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Authentication failed:",
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!("  {error}"),
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
                .title(" ChatGPT Authentication ")
                .title_alignment(ratatui::layout::Alignment::Center),
        )
        .alignment(ratatui::layout::Alignment::Left);

    frame.render_widget(widget, center_chunks[1]);
}

/// Render the workspace access confirmation as a bottom sheet.
fn render_workspace_confirm(frame: &mut Frame, area: Rect, path: &str, reason: &str) {
    let content = vec![
        Line::from(Span::styled(
            "The agent is requesting access to:",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            format!("  {path}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(ratatui::style::Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled("Reason:", Style::default().fg(Color::White))),
        Line::from(Span::styled(
            format!("  {reason}"),
            Style::default().fg(Color::Yellow),
        )),
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
                .title(" Workspace Access Request "),
        )
        .wrap(ratatui::widgets::Wrap { trim: false });

    frame.render_widget(widget, area);
}

fn render_permission_confirm(
    frame: &mut Frame,
    area: Rect,
    state: &crate::tui::state::PermissionConfirmState,
) {
    let content = vec![
        Line::from(Span::styled(
            "The agent is requesting elevated terminal access:",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            format!("  mode: {}", state.request.mode.as_str()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(ratatui::style::Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled("Script:", Style::default().fg(Color::White))),
        Line::from(Span::styled(
            format!("  {}", state.request.script.as_str()),
            Style::default().fg(Color::Yellow),
        )),
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
                .border_style(Style::default().fg(Color::LightRed))
                .title(" Permission Request "),
        )
        .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(widget, area);
}

/// Rows the question text of the `AskUser` sheet takes at `width`.
fn ask_question_rows(question: &str, width: u16) -> u16 {
    let inner = usize::from(width.saturating_sub(2)).max(1);
    let rows = UnicodeWidthStr::width(question).div_ceil(inner).clamp(1, 4);
    u16::try_from(rows + 1).unwrap_or(5)
}

/// Rows the `AskUser` sheet needs: question, options, the custom answer
/// entry (+ its input line while typing), help line and borders.
fn ask_user_rows(state: &AskUserState, width: u16) -> u16 {
    let (Some(question), Some(qs)) = (state.current(), state.current_state()) else {
        return 3;
    };
    let items = u16::try_from(question.option.len() + 1).unwrap_or(u16::MAX);
    ask_question_rows(&question.question, width)
        .saturating_add(items)
        .saturating_add(u16::from(qs.custom_mode))
        .saturating_add(1 + 2)
}

/// Render the `AskUser` dialog as a bottom sheet.
fn render_ask_user(frame: &mut Frame, area: Rect, state: &AskUserState) {
    let Some(question) = state.current() else {
        return;
    };
    let Some(qs) = state.current_state() else {
        return;
    };

    // Item count: options + Custom answer
    let opt_count = question.option.len();
    let total_items = opt_count + 1;
    let overlay = area;

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
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
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
        Constraint::Length(ask_question_rows(&question.question, area.width)), // question text
        Constraint::Min(1),                                                    // items list
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
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )))
    .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(q_text, chunks[0]);

    // Render items: options + Custom answer
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
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        match global_idx.cmp(&opt_count) {
            std::cmp::Ordering::Less => {
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
            }
            std::cmp::Ordering::Equal | std::cmp::Ordering::Greater => {
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
                    Span::styled(
                        label,
                        if is_cursor {
                            cursor_style
                        } else {
                            custom_style
                        },
                    ),
                ])));
            }
        }
    }
    frame.render_widget(List::new(items), chunks[1]);

    // Custom text input line

    let chunk_idx_help = if qs.custom_mode {
        let input_line = Paragraph::new(Line::from(vec![
            Span::styled("  > ", Style::default().fg(Color::Cyan)),
            Span::styled(&qs.custom_text, Style::default().fg(Color::White)),
        ]));
        frame.render_widget(input_line, chunks[2]);
        let caret_x = chunks[2].x
            + 4
            + u16::try_from(UnicodeWidthStr::width(qs.custom_text.as_str())).unwrap_or(u16::MAX);
        frame.set_cursor_position((
            caret_x.min(chunks[2].right().saturating_sub(1)),
            chunks[2].y,
        ));
        3
    } else {
        2
    };

    // Help text
    let nav = if state.is_multi() {
        " | Tab/l: next | Shift-Tab/H: prev"
    } else {
        ""
    };
    // An open question stays open when its modal closes; a prompt a run
    // blocks on can only be answered.
    let close = if state.request.is_none() {
        " | Esc: close (stays open)"
    } else {
        ""
    };
    let help_text = if question.multi_select {
        format!("Space: toggle | Enter: confirm | j/k: navigate{nav}{close}")
    } else {
        format!("Enter: select | j/k: navigate{nav}{close}")
    };
    let help = Paragraph::new(help_text)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    if chunk_idx_help < chunks.len() {
        frame.render_widget(help, chunks[chunk_idx_help]);
    }
}

/// Render completion overlay near bottom of screen.
/// Rows the completion overlay lists at most.
const COMPLETION_MAX_ITEMS: usize = 10;

fn render_completion_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let max_items = COMPLETION_MAX_ITEMS;
    let item_count = app.completion.len().min(max_items);
    let height = u16::try_from(item_count + 2).unwrap_or(u16::MAX); // +2 for borders

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
                CompletionMode::File | CompletionMode::None => item.label.clone(),
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

/// Rect the completion overlay occupies this frame, if it is shown.
fn completion_overlay_rect(app: &App, area: Rect) -> Option<Rect> {
    if !app.completion.is_active() || app.completion.is_empty() {
        return None;
    }
    let item_count = app.completion.len().min(COMPLETION_MAX_ITEMS);
    let height = u16::try_from(item_count + 2).unwrap_or(u16::MAX); // +2 for borders
    // Near the bottom, leaving 2 lines for input/status.
    let y = area.y + area.height.saturating_sub(height + 2);
    Some(Rect {
        x: area.x,
        y,
        width: area.width,
        height: height.min(area.height),
    })
}

/// Paint the inline image blocks of the view.
///
/// Each block is drawn at its full reserved size or not at all: a block cut
/// by the top or bottom edge used to be re-fitted into the visible rows,
/// which re-encoded (and for kitty re-transmitted) the whole image on every
/// scroll step and showed it squashed. A cut block shows a one-line caption
/// in its visible rows instead. Blocks overlapping an `occluders` rect are
/// skipped so a popup is never painted under.
fn render_inline_images(
    frame: &mut Frame,
    chat_area: Rect,
    image_cache: &mut ahash::AHashMap<String, super::image::ImageState>,
    image_regions: &mut Vec<ImageRegion>,
    placements: &[chat::ImagePlacement],
    occluders: &[Rect],
) {
    use super::streaming_markdown::IMAGE_HEIGHT;
    use ratatui_image::Resize;

    let view_height = i32::from(chat_area.height);
    let block_height = i32::try_from(IMAGE_HEIGHT).unwrap_or(i32::MAX);
    let indent = 2u16; // Indent from left edge
    for placement in placements {
        let top = placement.top;
        let bottom = top.saturating_add(block_height);
        if bottom <= 0 || top >= view_height {
            continue;
        }
        let width = chat_area.width.saturating_sub(indent * 2);
        if width < 4 {
            continue;
        }
        if top < 0 || bottom > view_height {
            // Caption in the first visible reserved row.
            let y = chat_area.y + u16::try_from(top.max(0)).unwrap_or(0);
            frame.buffer_mut().set_stringn(
                chat_area.x + indent,
                y,
                format!("▏image · {}", placement.caption),
                usize::from(width),
                Style::default().fg(Color::DarkGray),
            );
            continue;
        }
        let image_area = Rect::new(
            chat_area.x + indent,
            chat_area.y + u16::try_from(top).unwrap_or(0),
            width,
            u16::try_from(IMAGE_HEIGHT).unwrap_or(u16::MAX),
        );
        if occluders.iter().any(|rect| rect.intersects(image_area)) {
            continue;
        }

        // Get the cached image state
        let Some(state) = image_cache.get_mut(&placement.key) else {
            // Image not yet fetched
            continue;
        };

        // The fitted rect is what the image actually paints into — it is the
        // click region for the zoom view.
        let fitted = state.protocol.size_for(Resize::Fit(None), image_area);
        let draw_area = Rect::new(
            image_area.x,
            image_area.y,
            fitted.width.min(image_area.width),
            fitted.height.min(image_area.height),
        );

        super::image::render_image(frame, image_area, state);
        image_regions.push(ImageRegion {
            rect: draw_area,
            key: placement.key.clone(),
            caption: placement.caption.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::chat::{ChatStream, ImagePlacement, UnifiedRenderContext, render_unified};
    use super::super::chat_layout::ChatLayout;
    use super::super::image::{ImageState, render_image};
    use super::super::test_screen::{IMAGE_MARK, ScreenBackend, buffer_screen};
    use super::render_inline_images;
    use crate::tui::app::ImageRegion;
    use ratatui::Terminal;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui_image::picker::{Picker, ProtocolType};

    #[allow(deprecated)] // a fixed font size keeps the test off the real terminal
    fn image_state(protocol: ProtocolType) -> ImageState {
        let mut picker = Picker::from_fontsize((10, 20));
        picker.set_protocol_type(protocol);
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            120,
            100,
            image::Rgb([200, 40, 40]),
        ));
        ImageState {
            protocol: picker.new_resize_protocol(image),
        }
    }

    /// Text drawn around an image must keep updating while the image stays
    /// on screen. The protocols pack an escape sequence (base64 payload for
    /// sixel/iTerm2) into one cell whose computed width made the buffer diff
    /// skip up to thousands of following cells: the rows below the image
    /// kept showing old text.
    #[test]
    fn text_around_an_image_keeps_updating() {
        for protocol in [
            ProtocolType::Kitty,
            ProtocolType::Sixel,
            ProtocolType::Iterm2,
        ] {
            let mut state = image_state(protocol);
            let mut terminal = Terminal::new(ScreenBackend::new(40, 16)).unwrap();
            let image_area = Rect::new(2, 2, 20, 6);
            for fill in ['a', 'b', 'c'] {
                let mut truth = Buffer::empty(Rect::new(0, 0, 40, 16));
                terminal
                    .draw(|frame| {
                        let area = frame.area();
                        let row: String = std::iter::repeat_n(fill, 40).collect();
                        for y in 0..area.height {
                            frame.buffer_mut().set_string(
                                0,
                                y,
                                &row,
                                ratatui::style::Style::default(),
                            );
                        }
                        render_image(frame, image_area, &mut state);
                        truth.clone_from(frame.buffer_mut());
                    })
                    .unwrap();
                let shown = terminal.backend().screen();
                let expected = buffer_screen(&truth);
                for y in 0..16u16 {
                    // Rows the image covers hold skip cells the terminal
                    // paints itself; compare everything else.
                    if (image_area.top()..image_area.bottom()).contains(&y) {
                        let (shown_row, expected_row) =
                            (&shown[usize::from(y)], &expected[usize::from(y)]);
                        let right_shown: String = shown_row
                            .chars()
                            .skip(usize::from(image_area.right()))
                            .collect();
                        let right_expected: String = expected_row
                            .chars()
                            .skip(usize::from(image_area.right()))
                            .collect();
                        assert_eq!(
                            right_shown, right_expected,
                            "{protocol:?} fill={fill} row={y} right of image"
                        );
                        continue;
                    }
                    assert_eq!(
                        shown[usize::from(y)],
                        expected[usize::from(y)],
                        "{protocol:?} fill={fill} row={y}"
                    );
                }
                assert!(
                    shown[usize::from(image_area.y)].contains(IMAGE_MARK),
                    "{protocol:?}: image not drawn"
                );
            }
        }
    }

    fn transcript() -> ChatStream {
        use std::fmt::Write as _;
        let mut chat = ChatStream::new();
        chat.push_user("show me", Vec::new(), 0);
        let mut reply = String::from("Here:\n\n![chart](chart.png)\n\n");
        for index in 0..40 {
            let _ = writeln!(reply, "line {index} after the image\n");
        }
        chat.push_text(&reply);
        chat.end_text();
        chat
    }

    /// Draw the transcript at `scroll` with the inline image overlay, as
    /// `layout::render` does, asserting the terminal matches the frame.
    fn draw(
        terminal: &mut Terminal<ScreenBackend>,
        chat: &ChatStream,
        layout: &mut ChatLayout,
        cache: &mut ahash::AHashMap<String, ImageState>,
        scroll: usize,
    ) -> (Vec<String>, Vec<ImageRegion>) {
        let mut regions = Vec::new();
        let mut truth = None;
        terminal
            .draw(|frame| {
                let area = frame.area();
                let mut placements: Vec<ImagePlacement> = Vec::new();
                render_unified(
                    frame,
                    area,
                    chat,
                    layout,
                    scroll,
                    UnifiedRenderContext {
                        selected_tool: None,
                        content_height_out: None,
                        view_height_out: None,
                        input_ctx: None,
                        input_caret_active: false,
                        queue_ctx: None,
                        status_ctx: None,
                        images_supported: true,
                        hide_todos: false,
                        image_positions_out: Some(&mut placements),
                        spinner_frame: 0,
                    },
                );
                render_inline_images(frame, area, cache, &mut regions, &placements, &[]);
                truth = Some(frame.buffer_mut().clone());
            })
            .unwrap();
        let shown = terminal.backend().screen();
        let truth = buffer_screen(&truth.expect("drawn"));
        // Skip cells inside an image are painted by the terminal itself.
        for (y, (shown_row, truth_row)) in shown.iter().zip(&truth).enumerate() {
            let y = u16::try_from(y).unwrap();
            if regions
                .iter()
                .any(|r| (r.rect.top()..r.rect.bottom()).contains(&y))
            {
                continue;
            }
            assert_eq!(shown_row, truth_row, "scroll={scroll} row={y}");
        }
        (shown, regions)
    }

    /// Scrolling an image out of view leaves no image cell behind, and an
    /// image cut by the view edge is not drawn squashed into what remains:
    /// its visible rows show a caption.
    #[test]
    fn scrolled_out_image_leaves_no_cells() {
        let chat = transcript();
        let mut cache = ahash::AHashMap::new();
        cache.insert("chart.png".to_string(), image_state(ProtocolType::Kitty));
        let mut terminal = Terminal::new(ScreenBackend::new(50, 20)).unwrap();
        let mut layout = ChatLayout::default();

        let (shown, regions) = draw(&mut terminal, &chat, &mut layout, &mut cache, 0);
        assert_eq!(regions.len(), 1, "image visible at the top");
        assert!(shown.iter().any(|row| row.contains(IMAGE_MARK)));

        // Cut by the top edge: caption only, no protocol cells.
        let (shown, regions) = draw(&mut terminal, &chat, &mut layout, &mut cache, 5);
        assert!(regions.is_empty());
        assert!(
            !shown.iter().any(|row| row.contains(IMAGE_MARK)),
            "{shown:#?}"
        );
        assert!(shown[0].contains("image · chart.png"), "{shown:#?}");

        // Fully out of view.
        let (shown, regions) = draw(&mut terminal, &chat, &mut layout, &mut cache, 30);
        assert!(regions.is_empty());
        assert!(
            !shown.iter().any(|row| row.contains(IMAGE_MARK)),
            "{shown:#?}"
        );

        // Back in view.
        let (shown, regions) = draw(&mut terminal, &chat, &mut layout, &mut cache, 0);
        assert_eq!(regions.len(), 1);
        assert!(shown.iter().any(|row| row.contains(IMAGE_MARK)));
    }

    #[test]
    fn sheets_take_what_they_need_and_leave_the_chat_rows() {
        use super::{MIN_CHAT_ROWS, fit_sheet};
        // Short content: exactly its rows.
        assert_eq!(fit_sheet(9, 24, 40), 9);
        // Long content: capped, chat keeps its rows.
        assert_eq!(fit_sheet(200, 24, 40), 24);
        assert_eq!(fit_sheet(200, 30, 30), 30 - MIN_CHAT_ROWS);
        // A tiny terminal still shows a usable sheet.
        assert_eq!(fit_sheet(200, 4, 8), 5);
        assert_eq!(fit_sheet(1, 4, 8), 3);
        assert_eq!(fit_sheet(200, 2, 2), 2);
    }

    /// An occluding rect (a sheet or overlay) keeps the image off-screen.
    #[test]
    fn occluded_image_is_not_drawn() {
        let chat = transcript();
        let mut cache = ahash::AHashMap::new();
        cache.insert("chart.png".to_string(), image_state(ProtocolType::Kitty));
        let mut layout = ChatLayout::default();
        let mut terminal = Terminal::new(ScreenBackend::new(50, 20)).unwrap();
        let mut regions = Vec::new();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let mut placements = Vec::new();
                render_unified(
                    frame,
                    area,
                    &chat,
                    &mut layout,
                    0,
                    UnifiedRenderContext {
                        selected_tool: None,
                        content_height_out: None,
                        view_height_out: None,
                        input_ctx: None,
                        input_caret_active: false,
                        queue_ctx: None,
                        status_ctx: None,
                        images_supported: true,
                        hide_todos: false,
                        image_positions_out: Some(&mut placements),
                        spinner_frame: 0,
                    },
                );
                let occluder = Rect::new(0, 4, 50, 3);
                render_inline_images(
                    frame,
                    area,
                    &mut cache,
                    &mut regions,
                    &placements,
                    &[occluder],
                );
            })
            .unwrap();
        assert!(regions.is_empty());
        assert!(
            !terminal
                .backend()
                .screen()
                .iter()
                .any(|row| row.contains(IMAGE_MARK))
        );
    }
}
