//! The right sidebar: session title, task tree with linked issues, running
//! subagents and background terminal jobs.

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::super::chat::{format_elapsed, format_number};
use crate::tui::app::App;
use crate::tui::sidebar::{IssueBrief, TaskTree};

/// Rows kept for each background section when tasks compete for space.
const BACKGROUND_ROWS: usize = 6;

/// `text` cut to `width` display columns, with `…` when it was cut.
fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Wrap `text` to `width` columns; continuation rows are indented by
/// `indent` columns so they line up under the first row's text.
fn wrap_hanging(text: &str, width: usize, indent: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0;
    let mut limit = width.max(1);
    for word in text.split(' ') {
        let word_width = word.width();
        let gap = usize::from(!row.is_empty());
        if used + gap + word_width > limit && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            used = 0;
            limit = width.saturating_sub(indent).max(1);
        }
        if !row.is_empty() {
            row.push(' ');
            used += 1;
        }
        // A word longer than the row is split by characters.
        for ch in word.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > limit && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                used = 0;
                limit = width.saturating_sub(indent).max(1);
            }
            row.push(ch);
            used += w;
        }
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

fn heading(text: String) -> Line<'static> {
    Line::from(Span::styled(
        text,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn dim(text: String) -> Line<'static> {
    Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
}

/// Rows of the task tree, plus the row of the first open task.
fn task_rows(tree: &TaskTree, width: usize) -> (Vec<Line<'static>>, Option<usize>) {
    let mut rows = Vec::new();
    let mut first_open = None;
    for node in &tree.nodes {
        let indent = 2 * node.depth.min(6);
        let (marker, marker_style, text_style) = if node.done {
            (
                "✓ ",
                Style::default().fg(Color::Green),
                Style::default().fg(Color::DarkGray),
            )
        } else {
            ("○ ", Style::default().fg(Color::Yellow), Style::default())
        };
        if !node.done && first_open.is_none() {
            first_open = Some(rows.len());
        }
        let text_width = width.saturating_sub(indent + 2);
        for (index, part) in wrap_hanging(&node.text, text_width, 0)
            .into_iter()
            .enumerate()
        {
            let lead = if index == 0 { marker } else { "  " };
            rows.push(Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(lead, marker_style),
                Span::styled(part, text_style),
            ]));
        }
    }
    (rows, first_open)
}

fn issue_row(issue: &IssueBrief, width: usize) -> Line<'static> {
    let number = format!("#{} ", issue.number);
    let (state, state_style) = match (&issue.title, issue.closed) {
        // Not found (yet): no state to claim.
        (None, _) => ("", Style::default()),
        (Some(_), true) => ("closed", Style::default().fg(Color::DarkGray)),
        (Some(_), false) => ("open", Style::default().fg(Color::Green)),
    };
    let title_width = width.saturating_sub(number.width() + state.width() + 1);
    let title = issue
        .title
        .as_deref()
        .map_or_else(|| "…".to_string(), |title| truncate(title, title_width));
    let pad = width.saturating_sub(number.width() + title.width() + state.width());
    Line::from(vec![
        Span::styled(number, Style::default().fg(Color::Magenta)),
        Span::raw(title),
        Span::raw(" ".repeat(pad.max(1))),
        Span::styled(state, state_style),
    ])
}

/// Render the sidebar into `area`.
pub(super) fn render_sidebar(frame: &mut Frame, area: Rect, app: &App) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let inner = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(1),
        ..inner
    };
    let width = usize::from(inner.width);
    if width < 8 || inner.height == 0 {
        return;
    }

    let mut head: Vec<Line<'static>> = Vec::new();
    let title = app
        .sidebar
        .title
        .clone()
        .or_else(|| {
            app.session_manager
                .current()
                .and_then(|session| session.title.clone())
        })
        .unwrap_or_else(|| "New session".to_string());
    for row in wrap_hanging(&title, width, 0).into_iter().take(2) {
        head.push(Line::from(Span::styled(
            row,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
    }
    head.push(Line::from(""));

    // Background sections, bounded so tasks always get room too.
    let mut background: Vec<Line<'static>> = Vec::new();
    if !app.subagents.is_empty() {
        background.push(Line::from(""));
        background.push(heading(format!("Subagents ({})", app.subagents.len())));
        for run in app.subagents.iter().take(BACKGROUND_ROWS) {
            let stats = format!(
                " {} · {} tok",
                format_elapsed(run.elapsed_secs()),
                format_number(usize::try_from(run.total_tokens()).unwrap_or(usize::MAX)),
            );
            let name = truncate(&run.agent, width.saturating_sub(stats.width() + 2));
            background.push(Line::from(vec![
                Span::styled("◉ ", Style::default().fg(Color::Magenta)),
                Span::raw(name),
                Span::styled(stats, Style::default().fg(Color::DarkGray)),
            ]));
        }
        if app.subagents.len() > BACKGROUND_ROWS {
            background.push(dim(format!(
                "  … {} more",
                app.subagents.len() - BACKGROUND_ROWS
            )));
        }
    }
    if !app.jobs.is_empty() {
        let mut jobs: Vec<&aither_sandbox::JobInfo> = app.jobs.iter().collect();
        jobs.sort_by_key(|job| !matches!(job.status, aither_sandbox::JobStatus::Running));
        let running = jobs
            .iter()
            .filter(|job| matches!(job.status, aither_sandbox::JobStatus::Running))
            .count();
        background.push(Line::from(""));
        background.push(heading(format!("Jobs ({running} running)")));
        for job in jobs.iter().take(BACKGROUND_ROWS) {
            let (icon, icon_style, status) = match &job.status {
                aither_sandbox::JobStatus::Running => (
                    "● ",
                    Style::default().fg(Color::Yellow),
                    format!(
                        "running {}",
                        format_elapsed(job.started_at.elapsed().as_secs())
                    ),
                ),
                aither_sandbox::JobStatus::Completed { exit_code } => (
                    "✓ ",
                    if *exit_code == 0 {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default().fg(Color::Red)
                    },
                    format!("exit {exit_code}"),
                ),
                aither_sandbox::JobStatus::Failed { .. } => {
                    ("✗ ", Style::default().fg(Color::Red), "failed".to_string())
                }
                aither_sandbox::JobStatus::Killed => (
                    "■ ",
                    Style::default().fg(Color::DarkGray),
                    "killed".to_string(),
                ),
            };
            let status = format!(" {status}");
            let script = job.script.lines().next().unwrap_or("").trim();
            let script = truncate(script, width.saturating_sub(status.width() + 2));
            background.push(Line::from(vec![
                Span::styled(icon, icon_style),
                Span::raw(script),
                Span::styled(status, Style::default().fg(Color::DarkGray)),
            ]));
        }
        if jobs.len() > BACKGROUND_ROWS {
            background.push(dim(format!("  … {} more", jobs.len() - BACKGROUND_ROWS)));
        }
    }

    // Tasks and their linked issues fill what is left.
    let mut tasks: Vec<Line<'static>> = Vec::new();
    let mut first_open = None;
    match &app.sidebar.tasks {
        Some(tree) if !tree.nodes.is_empty() => {
            let (done, total) = tree.progress();
            tasks.push(heading(format!("Tasks {done}/{total}")));
            let (rows, open) = task_rows(tree, width);
            first_open = open.map(|row| row + 1);
            tasks.extend(rows);
            if !app.sidebar.issues.is_empty() {
                tasks.push(Line::from(""));
                tasks.push(heading("Issues".to_string()));
                tasks.extend(
                    app.sidebar
                        .issues
                        .iter()
                        .map(|issue| issue_row(issue, width)),
                );
            }
        }
        _ => {
            tasks.push(heading("Tasks".to_string()));
            tasks.push(dim("No tasks yet".to_string()));
        }
    }

    let height = usize::from(inner.height);
    let budget = height
        .saturating_sub(head.len())
        .saturating_sub(background.len());
    if tasks.len() > budget && budget > 1 {
        // Keep the heading, and scroll the list so the first open task
        // (the one in progress) stays in view.
        let body = budget - 1;
        let start = first_open.map_or(1, |row| row.saturating_sub(2).max(1));
        let start = start.min(tasks.len().saturating_sub(body));
        let hidden_after = tasks.len().saturating_sub(start + body);
        let mut window: Vec<Line<'static>> = Vec::with_capacity(budget);
        window.push(tasks[0].clone());
        window.extend(tasks.iter().skip(start).take(body).cloned());
        if hidden_after > 0
            && let Some(last) = window.last_mut()
        {
            *last = dim(format!("  … {} more", hidden_after + 1));
        }
        tasks = window;
    }

    let mut lines = head;
    lines.extend(tasks);
    lines.extend(background);
    lines.truncate(height);
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::{truncate, wrap_hanging};

    #[test]
    fn wrap_hanging_wraps_by_display_width() {
        assert_eq!(
            wrap_hanging("fix the image diff", 8, 0),
            vec!["fix the", "image", "diff"]
        );
        assert_eq!(
            wrap_hanging("修复图片渲染问题", 6, 0),
            vec!["修复图", "片渲染", "问题"]
        );
        assert_eq!(wrap_hanging("", 6, 0), vec![""]);
    }

    #[test]
    fn truncate_marks_the_cut() {
        assert_eq!(truncate("cargo test --lib", 9), "cargo te…");
        assert_eq!(truncate("short", 9), "short");
        assert_eq!(truncate("终端渲染逻辑", 7), "终端渲…");
    }
}
