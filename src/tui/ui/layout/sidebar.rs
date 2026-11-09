//! The right sidebar: session title, task tree with linked issues, file
//! changes per tracked workspace, running subagents and background
//! terminal jobs.

use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::super::band::panel;
use super::super::chat::{format_elapsed, format_number};
use crate::task_rows::{TaskRow, TaskSection};
use crate::tui::app::App;
use crate::tui::sidebar::IssueBrief;

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

/// `text` cut to `width` display columns keeping its tail — for paths
/// whose basename matters more than their leading directories.
fn truncate_tail(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut tail = String::new();
    let mut used = 1; // the leading …
    for ch in text.chars().rev() {
        let w = ch.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        tail.insert(0, ch);
        used += w;
    }
    format!("…{tail}")
}

/// `path` with the user's home directory shown as `~`.
fn home_label(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

/// Rows of the task tree, plus the row of the first open task.
/// The open work grouped under its section's heading, plus the line of the
/// first row of `Doing` (or of the first section when nothing is doing).
/// Roots are not listed here: the issues they name appear under "Issues".
fn task_rows(rows: &[TaskRow], width: usize) -> (Vec<Line<'static>>, Option<usize>) {
    let mut lines = Vec::new();
    let mut first_open = None;
    for (section, title) in [
        (TaskSection::Doing, "Doing"),
        (TaskSection::Queued, "Queued"),
    ] {
        let mut section_rows = rows.iter().filter(|row| row.section == section).peekable();
        if section_rows.peek().is_none() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(heading(title.to_string()));
        for row in section_rows {
            if first_open.is_none() {
                first_open = Some(lines.len());
            }
            let (marker, marker_style) = if row.blocking.is_some() {
                ("⏸ ", Style::default().fg(Color::Red))
            } else {
                ("○ ", Style::default().fg(Color::Yellow))
            };
            let mut text = row.text.split_whitespace().collect::<Vec<_>>().join(" ");
            if let Some(what) = &row.blocking {
                text = format!("{text} (blocked: {what})");
            }
            for (index, part) in wrap_hanging(&text, width.saturating_sub(2), 0)
                .into_iter()
                .enumerate()
            {
                let lead = if index == 0 { marker } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(lead, marker_style),
                    Span::raw(part),
                ]));
            }
        }
    }
    (lines, first_open)
}

fn issue_row(issue: &IssueBrief, width: usize) -> Line<'static> {
    let number = format!("#{} ", issue.number);
    // GitHub-style state icon: a green circle for open, a dark check for
    // closed, nothing while the lookup is still pending.
    let (icon, icon_style) = match (&issue.title, issue.closed) {
        // Not found (yet): no state to claim.
        (None, _) => ("", Style::default()),
        (Some(_), true) => ("✓", Style::default().fg(Color::DarkGray)),
        (Some(_), false) => ("●", Style::default().fg(Color::Green)),
    };
    let title_width = width.saturating_sub(number.width() + icon.width() + 1);
    let title = issue
        .title
        .as_deref()
        .map_or_else(|| "…".to_string(), |title| truncate(title, title_width));
    let pad = width.saturating_sub(number.width() + title.width() + icon.width());
    Line::from(vec![
        Span::styled(number, Style::default().fg(Color::Magenta)),
        Span::raw(title),
        Span::raw(" ".repeat(pad.max(1))),
        Span::styled(icon, icon_style),
    ])
}

/// Check-rollup icon for a live PR row: GitHub's own traffic light —
/// red while any check failed, yellow while some still run, green when
/// all passed. `merging` overrides with green; no rollup read yet shows
/// the neutral open dot.
fn pr_icon(pr: &crate::client::WirePr) -> (&'static str, Style) {
    if pr.state == "merging" {
        return ("●", Style::default().fg(Color::Green));
    }
    match pr.checks {
        Some(c) if c.failed > 0 => ("●", Style::default().fg(Color::Red)),
        Some(c) if c.pending > 0 => ("●", Style::default().fg(Color::Yellow)),
        Some(_) => ("✓", Style::default().fg(Color::Green)),
        None => ("●", Style::default().fg(Color::Green)),
    }
}

/// Render the sidebar into `area`.
pub(super) fn render_sidebar(frame: &mut Frame, area: Rect, app: &App) {
    frame.render_widget(Clear, area);
    let block = panel(None::<Span>, Color::Reset, app.band_bg);
    let inner = block.inner(area);
    frame.render_widget(block, area);
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
    if app.sidebar_file_changes && !app.file_status.is_empty() {
        let total: usize = app.file_status.iter().map(|d| d.changes.len()).sum();
        background.push(Line::from(""));
        background.push(heading(format!("Changes ({total})")));
        let mut rows_left = BACKGROUND_ROWS;
        let mut hidden = 0usize;
        for root in &app.file_status {
            if rows_left == 0 {
                hidden += root.changes.len();
                continue;
            }
            let root_name = root.label.clone().unwrap_or_else(|| home_label(&root.root));
            background.push(dim(format!(
                " {}",
                truncate(&root_name, width.saturating_sub(1))
            )));
            for stat in &root.changes {
                if rows_left == 0 {
                    hidden += 1;
                    continue;
                }
                rows_left -= 1;
                let adds = format!("+{}", stat.additions);
                let dels = format!("−{}", stat.deletions);
                let name = truncate_tail(
                    &stat.path.to_string_lossy(),
                    width.saturating_sub(adds.width() + dels.width() + 4),
                );
                background.push(Line::from(vec![
                    Span::raw("  "),
                    Span::raw(name),
                    Span::styled(format!(" {adds}"), Style::default().fg(Color::Green)),
                    Span::styled(format!(" {dels}"), Style::default().fg(Color::Red)),
                ]));
            }
        }
        if hidden > 0 {
            background.push(dim(format!("  … {hidden} more")));
        }
    }
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
                aither_sandbox::JobStatus::Completed { exit_code, .. } => (
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
                aither_sandbox::JobStatus::Killed { .. } => (
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
        Some(rows) if rows.iter().any(|row| row.section.is_open_work()) => {
            let (lines, open) = task_rows(rows, width);
            first_open = open;
            tasks.extend(lines);
        }
        _ => {
            tasks.push(heading("Tasks".to_string()));
            tasks.push(dim("No tasks yet".to_string()));
        }
    }
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
    let live_prs: Vec<_> = app
        .sidebar
        .prs
        .iter()
        .filter(|pr| matches!(pr.state.as_str(), "open" | "checks_pending" | "merging"))
        .collect();
    if !live_prs.is_empty() {
        tasks.push(Line::from(""));
        tasks.push(heading("Pull requests".to_string()));
        for pr in live_prs {
            let (icon, icon_style) = pr_icon(pr);
            let number = format!("#{} ", pr.number);
            let title_width = width.saturating_sub(number.width() + icon.width() + 1);
            let title = truncate(&pr.title, title_width);
            let pad = width.saturating_sub(number.width() + title.width() + icon.width());
            tasks.push(Line::from(vec![
                Span::styled(number, Style::default().fg(Color::Magenta)),
                Span::raw(title),
                Span::raw(" ".repeat(pad.max(1))),
                Span::styled(icon, icon_style),
            ]));
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
    use super::{pr_icon, truncate, wrap_hanging};
    use crate::client::{WirePr, WirePrChecks};

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

    fn wire_pr(state: &str, checks: Option<WirePrChecks>) -> WirePr {
        WirePr {
            number: 1,
            title: "title".to_string(),
            state: state.to_string(),
            backend: "github".to_string(),
            external_number: Some(1),
            issue_number: None,
            auto_merge: false,
            checks,
            updated_at: String::new(),
        }
    }

    #[test]
    fn the_pr_row_icon_follows_the_check_rollup() {
        use ratatui::style::Color;

        // No rollup read yet: the neutral open dot.
        assert_eq!(pr_icon(&wire_pr("open", None)).0, "●");
        // Merging overrides whatever the checks say.
        assert_eq!(
            pr_icon(&wire_pr(
                "merging",
                Some(WirePrChecks {
                    pending: 2,
                    passed: 0,
                    failed: 0,
                }),
            ))
            .0,
            "●"
        );
        // Failure beats pending; pending beats all-passed.
        let (icon, style) = pr_icon(&wire_pr(
            "open",
            Some(WirePrChecks {
                pending: 1,
                passed: 0,
                failed: 1,
            }),
        ));
        assert_eq!((icon, style.fg), ("●", Some(Color::Red)));
        let (icon, style) = pr_icon(&wire_pr(
            "open",
            Some(WirePrChecks {
                pending: 1,
                passed: 3,
                failed: 0,
            }),
        ));
        assert_eq!((icon, style.fg), ("●", Some(Color::Yellow)));
        let (icon, style) = pr_icon(&wire_pr(
            "open",
            Some(WirePrChecks {
                pending: 0,
                passed: 4,
                failed: 0,
            }),
        ));
        assert_eq!((icon, style.fg), ("✓", Some(Color::Green)));
    }
}
