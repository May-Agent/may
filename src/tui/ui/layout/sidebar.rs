//! The right sidebar: session title, task tree with linked issues, file
//! changes per tracked workspace, running subagents and background
//! terminal jobs.
//!
//! The title stays put; everything below it is one column that scrolls
//! (mouse wheel, `Alt+Up`/`Alt+Down`). Each section folds under its heading,
//! a truncated list shows all its entries from its `… N more` row, and a task
//! row with a detail line expands to show it.

use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::super::band::panel;
use super::super::chat::{format_elapsed, format_number};
use crate::task_rows::{TaskRow, TaskSection};
use crate::tui::app::App;
use crate::tui::sidebar::{IssueBrief, SidebarHit, SidebarSection, SidebarState, task_key};

/// Entries a background section lists before its `… N more` row.
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

/// The scrolling part of the sidebar: its lines, and the click target of
/// each line that has one.
#[derive(Default)]
struct Body {
    lines: Vec<Line<'static>>,
    hits: Vec<(usize, SidebarHit)>,
}

impl Body {
    fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    fn push_hit(&mut self, line: Line<'static>, hit: SidebarHit) {
        self.hits.push((self.lines.len(), hit));
        self.lines.push(line);
    }

    /// A blank line, except at the top of the body.
    fn gap(&mut self) {
        if !self.lines.is_empty() {
            self.lines.push(Line::from(""));
        }
    }

    /// A section heading with its fold marker; `true` when the section is
    /// open and its entries follow.
    fn heading(&mut self, state: &SidebarState, section: SidebarSection, text: &str) -> bool {
        let open = !state.is_collapsed(section);
        let marker = if open { "▾" } else { "▸" };
        self.push_hit(
            Line::from(Span::styled(
                format!("{marker} {text}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            SidebarHit::Heading(section),
        );
        open
    }

    /// The entries of a capped list: all of them when `section` lists all,
    /// else the first [`BACKGROUND_ROWS`] and a `… N more` row.
    fn capped<T>(
        &mut self,
        state: &SidebarState,
        section: SidebarSection,
        entries: impl ExactSizeIterator<Item = T>,
        mut row: impl FnMut(&mut Self, T),
    ) {
        let total = entries.len();
        let all = state.lists_all(section);
        for entry in entries.take(if all { total } else { BACKGROUND_ROWS }) {
            row(self, entry);
        }
        if total > BACKGROUND_ROWS {
            let text = if all {
                "  show fewer".to_string()
            } else {
                format!("  … {} more", total - BACKGROUND_ROWS)
            };
            self.push_hit(dim(text), SidebarHit::More(section));
        }
    }
}

/// The open work grouped under its section's heading. Roots are not listed
/// here: the issues they name appear under "Issues".
///
/// A row is its title; its detail lines are shown once the row is expanded.
fn task_rows(body: &mut Body, state: &SidebarState, rows: &[TaskRow], width: usize) {
    for (section, title, id) in [
        (TaskSection::Doing, "Doing", SidebarSection::Doing),
        (TaskSection::Queued, "Queued", SidebarSection::Queued),
    ] {
        let section_rows: Vec<&TaskRow> =
            rows.iter().filter(|row| row.section == section).collect();
        if section_rows.is_empty() {
            continue;
        }
        body.gap();
        if !body.heading(state, id, title) {
            continue;
        }
        for row in section_rows {
            let (marker, marker_style) = if row.blocking.is_some() {
                ("⏸ ", Style::default().fg(Color::Red))
            } else {
                ("○ ", Style::default().fg(Color::Yellow))
            };
            let key = task_key(row);
            let mut text = key.clone();
            if let Some(what) = &row.blocking {
                text = format!("{text} (blocked: {what})");
            }
            let detail: Vec<String> = row
                .detail
                .lines()
                .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|line| !line.is_empty())
                .collect();
            let shown = !detail.is_empty() && state.shows_detail(&key);
            let fold = match (detail.is_empty(), shown) {
                (true, _) => "",
                (false, true) => " ▾",
                (false, false) => " ▸",
            };
            let hit = || SidebarHit::Task(key.clone());
            let parts = wrap_hanging(&format!("{text}{fold}"), width.saturating_sub(2), 0);
            for (index, part) in parts.into_iter().enumerate() {
                let lead = if index == 0 { marker } else { "  " };
                let line = Line::from(vec![Span::styled(lead, marker_style), Span::raw(part)]);
                if detail.is_empty() {
                    body.push(line);
                } else {
                    body.push_hit(line, hit());
                }
            }
            if shown {
                for line in &detail {
                    for part in wrap_hanging(line, width.saturating_sub(4), 0) {
                        body.push_hit(dim(format!("    {part}")), hit());
                    }
                }
            }
        }
    }
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

fn pr_row(pr: &crate::client::WirePr, width: usize) -> Line<'static> {
    let (icon, icon_style) = pr_icon(pr);
    let number = format!("#{} ", pr.number);
    let title_width = width.saturating_sub(number.width() + icon.width() + 1);
    let title = truncate(&pr.title, title_width);
    let pad = width.saturating_sub(number.width() + title.width() + icon.width());
    Line::from(vec![
        Span::styled(number, Style::default().fg(Color::Magenta)),
        Span::raw(title),
        Span::raw(" ".repeat(pad.max(1))),
        Span::styled(icon, icon_style),
    ])
}

fn changes(body: &mut Body, app: &App, width: usize) {
    let state = &app.sidebar;
    let total: usize = app.file_status.iter().map(|d| d.changes.len()).sum();
    body.gap();
    if !body.heading(
        state,
        SidebarSection::Changes,
        &format!("Changes ({total})"),
    ) {
        return;
    }
    // Rows are capped across roots; a root's name line is not counted.
    let all = state.lists_all(SidebarSection::Changes);
    let mut rows_left = if all { usize::MAX } else { BACKGROUND_ROWS };
    let mut hidden = 0usize;
    for root in &app.file_status {
        if rows_left == 0 {
            hidden += root.changes.len();
            continue;
        }
        let root_name = root.label.clone().unwrap_or_else(|| home_label(&root.root));
        body.push(dim(format!(
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
            body.push(Line::from(vec![
                Span::raw("  "),
                Span::raw(name),
                Span::styled(format!(" {adds}"), Style::default().fg(Color::Green)),
                Span::styled(format!(" {dels}"), Style::default().fg(Color::Red)),
            ]));
        }
    }
    if hidden > 0 {
        body.push_hit(
            dim(format!("  … {hidden} more")),
            SidebarHit::More(SidebarSection::Changes),
        );
    } else if all && total > BACKGROUND_ROWS {
        body.push_hit(
            dim("  show fewer".to_string()),
            SidebarHit::More(SidebarSection::Changes),
        );
    }
}

fn subagents(body: &mut Body, app: &App, width: usize) {
    body.gap();
    if !body.heading(
        &app.sidebar,
        SidebarSection::Subagents,
        &format!("Subagents ({})", app.subagents.len()),
    ) {
        return;
    }
    body.capped(
        &app.sidebar,
        SidebarSection::Subagents,
        app.subagents.iter(),
        |body, run| {
            let stats = format!(
                " {} · {} tok",
                format_elapsed(run.elapsed_secs()),
                format_number(usize::try_from(run.total_tokens()).unwrap_or(usize::MAX)),
            );
            let name = truncate(&run.agent, width.saturating_sub(stats.width() + 2));
            body.push(Line::from(vec![
                Span::styled("◉ ", Style::default().fg(Color::Magenta)),
                Span::raw(name),
                Span::styled(stats, Style::default().fg(Color::DarkGray)),
            ]));
        },
    );
}

fn jobs(body: &mut Body, app: &App, width: usize) {
    let mut jobs: Vec<&aither_sandbox::JobInfo> = app.jobs.iter().collect();
    jobs.sort_by_key(|job| !matches!(job.status, aither_sandbox::JobStatus::Running));
    let running = jobs
        .iter()
        .filter(|job| matches!(job.status, aither_sandbox::JobStatus::Running))
        .count();
    body.gap();
    if !body.heading(
        &app.sidebar,
        SidebarSection::Jobs,
        &format!("Jobs ({running} running)"),
    ) {
        return;
    }
    body.capped(
        &app.sidebar,
        SidebarSection::Jobs,
        jobs.into_iter(),
        |body, job| {
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
            body.push(Line::from(vec![
                Span::styled(icon, icon_style),
                Span::raw(script),
                Span::styled(status, Style::default().fg(Color::DarkGray)),
            ]));
        },
    );
}

/// Everything below the title, whole: the draw path windows it.
fn build_body(app: &App, width: usize) -> Body {
    let state = &app.sidebar;
    let mut body = Body::default();
    match &state.tasks {
        Some(rows) if rows.iter().any(|row| row.section.is_open_work()) => {
            task_rows(&mut body, state, rows, width);
        }
        _ => {
            body.push(Line::from(Span::styled(
                "Tasks".to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            body.push(dim("No tasks yet".to_string()));
        }
    }
    if !state.issues.is_empty() {
        body.gap();
        if body.heading(state, SidebarSection::Issues, "Issues") {
            for issue in &state.issues {
                body.push(issue_row(issue, width));
            }
        }
    }
    let live_prs: Vec<_> = state
        .prs
        .iter()
        .filter(|pr| matches!(pr.state.as_str(), "open" | "checks_pending" | "merging"))
        .collect();
    if !live_prs.is_empty() {
        body.gap();
        if body.heading(state, SidebarSection::PullRequests, "Pull requests") {
            for pr in live_prs {
                body.push(pr_row(pr, width));
            }
        }
    }
    if app.sidebar_file_changes && !app.file_status.is_empty() {
        changes(&mut body, app, width);
    }
    if !app.subagents.is_empty() {
        subagents(&mut body, app, width);
    }
    if !app.jobs.is_empty() {
        jobs(&mut body, app, width);
    }
    body
}

/// The dim row under an overflowing body saying how much lies off screen.
fn overflow_row(above: usize, below: usize) -> Line<'static> {
    let mut parts = Vec::new();
    if above > 0 {
        parts.push(format!("↑ {above}"));
    }
    if below > 0 {
        parts.push(format!("↓ {below}"));
    }
    dim(parts.join("  "))
}

/// Render the sidebar into `area`.
pub(super) fn render_sidebar(frame: &mut Frame, area: Rect, app: &mut App) {
    frame.render_widget(Clear, area);
    let block = panel(None::<Span>, Color::Reset, app.band_bg);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.sidebar.area = area;
    app.sidebar.view.hits.clear();
    app.sidebar.view.max_scroll = 0;
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

    let body = build_body(app, width);
    let height = usize::from(inner.height);
    let room = height.saturating_sub(head.len());
    // A body that does not fit gives its last row to the overflow marker.
    let overflows = body.lines.len() > room;
    let rows = if overflows {
        room.saturating_sub(1)
    } else {
        room
    };
    let max_scroll = body.lines.len().saturating_sub(rows);
    let scroll = app.sidebar.scroll.min(max_scroll);
    app.sidebar.scroll = scroll;
    app.sidebar.view.max_scroll = max_scroll;

    let top = inner.y + u16::try_from(head.len()).unwrap_or(u16::MAX);
    for (index, hit) in body.hits {
        if (scroll..scroll + rows).contains(&index) {
            let y = top + u16::try_from(index - scroll).unwrap_or(u16::MAX);
            app.sidebar.view.hits.push((y, hit));
        }
    }

    let hidden_below = body.lines.len().saturating_sub(scroll + rows);
    let mut lines = head;
    lines.extend(body.lines.into_iter().skip(scroll).take(rows));
    if overflows {
        lines.push(overflow_row(scroll, hidden_below));
    }
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
    fn the_sidebar_shows_each_rows_title_and_its_detail_once_expanded() {
        use crate::tui::sidebar::SidebarState;

        let rows = crate::task_rows::task_rows(
            "## Doing\n- wire the command\n  the detail shows on expansion\n- ask scope\n  BLOCKING q9\n\n## Queued\n- 写文档 ドキュメント 문서\n  詳細を表示する\n- plain\n",
        );
        let render = |state: &SidebarState| -> Vec<String> {
            let mut body = super::Body::default();
            super::task_rows(&mut body, state, &rows, 60);
            body.lines.iter().map(ToString::to_string).collect()
        };

        let mut state = SidebarState::default();
        assert_eq!(
            render(&state),
            vec![
                "▾ Doing",
                "○ wire the command ▸",
                // Its BLOCKING line is the title's mark, so nothing is left to expand.
                "⏸ ask scope (blocked: q9)",
                "",
                "▾ Queued",
                "○ 写文档 ドキュメント 문서 ▸",
                "○ plain",
            ]
        );

        // The title is the expansion key.
        state
            .detailed
            .insert("写文档 ドキュメント 문서".to_string());
        assert_eq!(
            render(&state),
            vec![
                "▾ Doing",
                "○ wire the command ▸",
                "⏸ ask scope (blocked: q9)",
                "",
                "▾ Queued",
                "○ 写文档 ドキュメント 문서 ▾",
                "    詳細を表示する",
                "○ plain",
            ]
        );
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
