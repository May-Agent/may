//! Main layout for the TUI.

use std::sync::atomic::Ordering;

mod config;
mod pickers;
mod popups;

use self::config::render_config_screen;
use self::pickers::{render_file_picker_popup, render_profile_select_popup, render_session_popup};
use self::popups::{render_mcp_popup, render_skill_popup, render_ssh_popup};

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use unicode_width::UnicodeWidthChar;

use ratatui::Frame;

use super::super::app::App;
use super::super::completion::CompletionMode;
use super::super::state::{
    AppMode, AskUserState, BtwState, ChatgptAuthStatus, CopilotAuthStatus, JobDetailState,
    JobDetailTarget, RewindState,
};
use super::chat::{self, InputContext, QueueContext, StatusContext, UnifiedRenderContext};
use crate::workspace_request::WorkspaceRequest;

/// Convert character index to byte index for safe string slicing.
pub(super) fn char_to_byte_idx(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(i, _)| i)
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
        AppMode::ChatgptAuth => {
            render_chatgpt_auth_screen(frame, area, app);
            return;
        }
        AppMode::PermissionConfirm(state) => {
            render_permission_confirm(frame, area, state);
            return;
        }
        AppMode::WorkspaceConfirm(state) => {
            render_workspace_confirm(frame, area, &state.request);
            return;
        }
        _ => {}
    }

    // Scan chat content for images and request fetches for any not yet cached
    app.scan_and_fetch_images();

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
    let chat_area = area;

    // Running background entries for the status bar: tracked subagents first,
    // then running terminal jobs.
    let mut background_items: Vec<chat::StatusBackgroundItem> = app
        .subagent_tracker
        .running()
        .into_iter()
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
                open_questions: app.open_question_count,
                background: &background_items,
                decode_rate,
                status_message: app.status_text(),
                cwd: cwd_label.as_str(),
                activity: app.processing.then_some(chat::ActivityContext {
                    phase: app.activity_phase,
                    tokens: app.decode_meter.tokens(),
                }),
                spinner_frame: app.spinner_frame,
            }),
            images_supported,
            image_positions_out: Some(&mut image_positions),
        },
    );

    // Keep selection cache aligned with chat content and account for top padding.
    let queue_ctx = if queue_messages.is_empty() {
        None
    } else {
        Some(QueueContext {
            messages: &queue_messages,
        })
    };
    let status_ctx = StatusContext {
        model: &app.model_display(),
        usage: usage_info,
        queue_count: app.queue.len(),
        open_questions: app.open_question_count,
        background: &background_items,
        decode_rate,
        status_message: app.status_text(),
        cwd: cwd_label.as_str(),
        activity: app.processing.then_some(chat::ActivityContext {
            phase: app.activity_phase,
            tokens: app.decode_meter.tokens(),
        }),
        spinner_frame: app.spinner_frame,
    };
    let mut collapse_regions = Vec::new();
    let plain_all_lines = chat::build_plain_wrapped_lines_unified(
        &app.chat,
        chat_area.width,
        queue_ctx.as_ref(),
        Some(InputContext {
            input: &app.input,
            cursor: app.cursor,
            attachments: &app.attachments,
        }),
        Some(&status_ctx),
        Some(&mut collapse_regions),
        images_supported,
    );

    let view_h = chat_area.height as usize;
    app.render_top_padding = view_h.saturating_sub(plain_all_lines.len());
    app.rendered_plain_lines = plain_all_lines;
    app.collapse_regions = collapse_regions;

    if app.selection.is_some() {
        render_text_selection_overlay(frame, chat_area, app);
    }

    // Render images as overlays at their scroll-adjusted positions
    if images_supported && !image_positions.is_empty() {
        render_inline_images(frame, chat_area, app, &image_positions, scroll);
    }

    // Render popups if in special modes
    match app.mode {
        // The questions panel docks at the bottom over the still-rendered
        // chat: opening it never hides the conversation behind it.
        AppMode::AskUser(ref state) => {
            render_ask_user(frame, area, state);
        }
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
        AppMode::ProfileSelect => {
            render_profile_select_popup(frame, area, app);
        }
        AppMode::Rewind(ref state) => {
            render_rewind_popup(frame, area, state);
        }
        AppMode::Jobs => {
            render_jobs_panel(frame, area, app);
        }
        AppMode::JobDetail(ref state) => {
            render_job_detail(frame, area, state);
        }
        AppMode::Btw(ref state) => {
            render_btw_popup(frame, area, state);
        }
        _ => {}
    }

    // Render completion overlay if active (always on top)
    if app.completion.is_active() && !app.completion.is_empty() {
        render_completion_overlay(frame, area, app);
    }
}

fn render_text_selection_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let Some(sel) = app.selection else {
        return;
    };
    if sel.is_empty() || app.rendered_plain_lines.is_empty() {
        return;
    }

    let (start, end) = sel.normalized();
    let start_line = start
        .line
        .min(app.rendered_plain_lines.len().saturating_sub(1));
    let end_line = end
        .line
        .min(app.rendered_plain_lines.len().saturating_sub(1));

    let base_scroll = if app.scroll == usize::MAX {
        app.content_height
            .load(Ordering::Relaxed)
            .saturating_sub(app.view_height.load(Ordering::Relaxed))
    } else {
        app.scroll
    };

    for line_idx in start_line..=end_line {
        let visual_line = app.render_top_padding.saturating_add(line_idx);
        if visual_line < base_scroll {
            continue;
        }

        let y_in_view = visual_line - base_scroll;
        if y_in_view >= area.height as usize {
            continue;
        }

        let line = &app.rendered_plain_lines[line_idx];
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
    // Position popup near bottom of screen, left-aligned
    let popup_height = u16::try_from((app.model_select_list.len() + 2).min(10)).unwrap_or(u16::MAX);
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

    let popup_height = u16::try_from((state.targets.len() * 2 + 4).min(18)).unwrap_or(u16::MAX);
    let popup_width = area.width.saturating_sub(8).clamp(40, 88);
    let popup_area = centered_rect_exact(popup_width, popup_height, area);

    frame.render_widget(Clear, popup_area);

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
    let popup_height = 9u16;
    let popup_width = area.width.saturating_sub(8).clamp(44, 72);
    let popup_area = centered_rect_exact(popup_width, popup_height, area);

    frame.render_widget(Clear, popup_area);

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
    let needed = u16::try_from(rows.len() * 2 + 3).unwrap_or(u16::MAX);
    let panel_height = needed.clamp(6, area.height.saturating_sub(4).clamp(6, 16));
    let panel_area = bottom_rect(area.width, panel_height, area);

    frame.render_widget(Clear, panel_area);

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

    let needed = u16::try_from(body.len() + 3).unwrap_or(u16::MAX);
    let panel_height = needed.clamp(8, area.height.saturating_sub(2));
    let panel_area = bottom_rect(area.width, panel_height, area);
    frame.render_widget(Clear, panel_area);

    let inner_height = usize::from(panel_height.saturating_sub(2));
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

/// A rect of `width`/`height` anchored to the bottom edge of `area`.
fn bottom_rect(width: u16, height: u16, area: Rect) -> Rect {
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height - height,
        width: width.min(area.width),
        height,
    }
}

fn render_btw_popup(frame: &mut Frame, area: Rect, state: &BtwState) {
    let awaiting_question = !state.loading && !state.done;
    let answer_height = if awaiting_question { 5u16 } else { 14u16 };
    let popup_width = area.width.saturating_sub(10).clamp(50, 90);
    let popup_area = centered_rect_exact(popup_width, answer_height, area);

    frame.render_widget(Clear, popup_area);

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
            format!("> {}▌", state.input),
            Style::default().fg(Color::White),
        )));
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

/// Create a centered rectangle with exact pixel dimensions.
pub(super) fn centered_rect_exact(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
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
            format!("  {}", request.args.request.path.as_str()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(ratatui::style::Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled("Reason:", Style::default().fg(Color::White))),
        Line::from(Span::styled(
            format!("  {}", request.args.request.reason.as_str()),
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

fn render_permission_confirm(
    frame: &mut Frame,
    area: Rect,
    state: &crate::tui::state::PermissionConfirmState,
) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Black)),
        area,
    );

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
            Constraint::Percentage(12),
            Constraint::Min(60),
            Constraint::Percentage(12),
        ])
        .split(chunks[1]);

    let content = vec![
        Line::from(""),
        Line::from(Span::styled(
            "The agent is requesting elevated terminal access:",
            Style::default().fg(Color::White),
        )),
        Line::from(""),
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
                .title(" Permission Request ")
                .title_alignment(ratatui::layout::Alignment::Center),
        )
        .alignment(ratatui::layout::Alignment::Left);
    frame.render_widget(widget, center_chunks[1]);
}

/// Render MCP server management popup.
/// Render the `AskUser` dialog as a bottom overlay.
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

    // Calculate overlay height: question(2) + items + custom_input?(1) + help(1) + border(2)
    let custom_line = u16::from(qs.custom_mode);
    let max_height = (area.height * 60) / 100;
    let needed = 2 + u16::try_from(total_items).unwrap_or(u16::MAX) + custom_line + 1 + 2;
    let height = needed.min(max_height);

    // Bottom-anchored overlay
    let overlay = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(height),
        area.width,
        height,
    );
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
        Constraint::Length(2), // question text
        Constraint::Min(1),    // items list
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
            Span::styled("_", Style::default().fg(Color::DarkGray)),
        ]));
        frame.render_widget(input_line, chunks[2]);
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
fn render_completion_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let max_items = 10;
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
    let content_height = app.content_height.load(Ordering::Relaxed);

    // Calculate offset the same way as render_impl does:
    // When content doesn't fill the screen, it renders at the BOTTOM of the
    // viewport area, so the y-offset equals the unused space above.
    let (padding, actual_scroll) = if content_height < view_height {
        (view_height - content_height, 0)
    } else {
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

        let image_start_y = chat_area.y + u16::try_from(image_content_line).unwrap_or(u16::MAX);
        let available_height = (chat_area.y + chat_area.height).saturating_sub(image_start_y);

        // Skip if no space available
        if available_height == 0 {
            continue;
        }

        // Calculate image area with proper bounds
        // IMAGE_HEIGHT - 1 because we skip the header line
        let image_height = u16::try_from(IMAGE_HEIGHT - 1)
            .unwrap_or(u16::MAX)
            .min(available_height);
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
