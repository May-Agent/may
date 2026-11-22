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
use crate::app::App;
use crate::sidebar::{IssueBrief, SidebarHit, SidebarSection, SidebarState, task_key};
use may_protocol::task_rows::{TaskRow, TaskSection};

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
fn pr_icon(pr: &may_client::client::WirePr) -> (&'static str, Style) {
    match pr.state {
        may_db::PrState::Merged => return ("✓", Style::default().fg(Color::Green)),
        may_db::PrState::Closed => return ("×", Style::default().fg(Color::DarkGray)),
        may_db::PrState::Failed | may_db::PrState::Rejected => {
            return ("✗", Style::default().fg(Color::Red));
        }
        may_db::PrState::Merging => return ("●", Style::default().fg(Color::Green)),
        may_db::PrState::Open | may_db::PrState::ChecksPending => {}
    }
    match &pr.checks {
        Some(c) if c.failed > 0 => ("●", Style::default().fg(Color::Red)),
        Some(c) if c.pending > 0 => ("●", Style::default().fg(Color::Yellow)),
        Some(_) => ("✓", Style::default().fg(Color::Green)),
        None => ("●", Style::default().fg(Color::Green)),
    }
}

/// A PR row and, once expanded, its checks, rejection detail, and provenance.
/// Orphaned / review-required non-terminal PRs always expose Merge and Close
/// hit targets.
fn pr_rows(
    body: &mut Body,
    state: &SidebarState,
    pr: &may_client::client::WirePr,
    width: usize,
    now: jiff::Timestamp,
) {
    let (icon, icon_style) = pr_icon(pr);
    let rejection_detail = (!pr.state.is_terminal() || pr.state == may_db::PrState::Closed)
        .then(|| {
            pr.events
                .iter()
                .rev()
                .find(|e| {
                    matches!(
                        e.kind.as_str(),
                        "rejected" | "check_failed" | "removed_from_queue"
                    ) && e.detail.as_ref().is_some_and(|d| !d.trim().is_empty())
                })
                .and_then(|e| e.detail.as_deref())
        })
        .flatten();
    let foldable = pr.checks.as_ref().is_some_and(|c| !c.items.is_empty())
        || pr.checks_error.is_some()
        || !pr.events.is_empty()
        || pr.orphaned
        || pr.require_review
        || pr.merge_commit_sha.is_some()
        || rejection_detail.is_some();
    let open = foldable && state.shows_checks(pr.number);
    let lead = match (foldable, open) {
        (false, _) => "  ",
        (true, true) => "▾ ",
        (true, false) => "▸ ",
    };
    let sigil = may_db::pr_sigil(&pr.backend);
    let number = format!("{sigil}{} ", pr.number);
    let badge = if pr.orphaned {
        "[orphaned] ".to_string()
    } else if pr.require_review && !pr.state.is_terminal() {
        "[review] ".to_string()
    } else {
        String::new()
    };
    let progress = pr
        .checks
        .as_ref()
        .filter(|c| c.total() > 0)
        .map_or_default(|c| format!("{}/{} ", c.finished(), c.total()));
    let title_width = width.saturating_sub(
        lead.width() + number.width() + badge.width() + progress.width() + icon.width() + 1,
    );
    let title = truncate(&pr.title, title_width);
    let pad = width.saturating_sub(
        lead.width()
            + number.width()
            + badge.width()
            + title.width()
            + progress.width()
            + icon.width(),
    );
    let mut spans = vec![
        Span::styled(lead, Style::default().fg(Color::DarkGray)),
        Span::styled(number, Style::default().fg(Color::Magenta)),
    ];
    if !badge.is_empty() {
        spans.push(Span::styled(badge, Style::default().fg(Color::Yellow)));
    }
    spans.extend([
        Span::raw(title),
        Span::raw(" ".repeat(pad.max(1))),
        Span::styled(progress, Style::default().fg(Color::DarkGray)),
        Span::styled(icon, icon_style),
    ]);
    let line = Line::from(spans);
    if foldable {
        body.push_hit(line, SidebarHit::Pr(pr.number));
    } else {
        body.push(line);
    }
    if open {
        let state_label = if pr.orphaned {
            format!("    state {} · orphaned", pr.state)
        } else {
            format!("    state {}", pr.state)
        };
        body.push_hit(
            dim(truncate(&state_label, width)),
            SidebarHit::Pr(pr.number),
        );
        if !pr.repo_key.is_empty() {
            body.push_hit(
                dim(format!(
                    "    repo {}",
                    truncate_tail(&pr.repo_key, width.saturating_sub(9))
                )),
                SidebarHit::Pr(pr.number),
            );
        }
        if let Some(opened) = pr
            .events
            .iter()
            .find(|e| e.kind == "opened")
            .or_else(|| pr.events.first())
        {
            let msg_part = opened.message_id.map_or_default(|id| format!(" msg-#{id}"));
            let prov = format!(
                "    by {} ({}{})",
                if opened.actor.is_empty() {
                    "main"
                } else {
                    opened.actor.as_str()
                },
                if opened.session_id.is_empty() {
                    pr.session_id.as_str()
                } else {
                    opened.session_id.as_str()
                },
                msg_part
            );
            body.push_hit(dim(truncate(&prov, width)), SidebarHit::Pr(pr.number));
        }
        if let Some(sha) = &pr.merge_commit_sha {
            body.push_hit(
                dim(truncate(&format!("    merged → {sha}"), width)),
                SidebarHit::Pr(pr.number),
            );
        }
        if let Some(detail) = rejection_detail {
            for part in wrap_hanging(detail, width.saturating_sub(4), 0) {
                body.push_hit(
                    Line::from(Span::styled(
                        format!("    {part}"),
                        Style::default().fg(Color::Red),
                    )),
                    SidebarHit::Pr(pr.number),
                );
            }
        }
        if let Some(error) = &pr.checks_error {
            for part in wrap_hanging(
                &format!("checks unreadable: {error}"),
                width.saturating_sub(4),
                0,
            ) {
                body.push_hit(
                    Line::from(Span::styled(
                        format!("    {part}"),
                        Style::default().fg(Color::Red),
                    )),
                    SidebarHit::Pr(pr.number),
                );
            }
        }
        for check in pr.checks.iter().flat_map(|c| &c.items) {
            let (mark, style) = match check.state.as_str() {
                "passed" => ("✓", Style::default().fg(Color::Green)),
                "failed" => ("✗", Style::default().fg(Color::Red)),
                "running" => ("●", Style::default().fg(Color::Yellow)),
                _ => ("○", Style::default().fg(Color::DarkGray)),
            };
            let time = check.duration_secs(now).map_or_default(format_elapsed);
            let name_width = width.saturating_sub(4 + 2 + time.width() + 1);
            let name = truncate(&check.name, name_width);
            let pad = width.saturating_sub(4 + 2 + name.width() + time.width());
            body.push_hit(
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(format!("{mark} "), style),
                    Span::raw(name),
                    Span::raw(" ".repeat(pad.max(1))),
                    Span::styled(time, Style::default().fg(Color::DarkGray)),
                ]),
                SidebarHit::Pr(pr.number),
            );
            if check.state == "failed"
                && let Some(url) = &check.url
            {
                body.push_hit(
                    dim(format!(
                        "      {}",
                        truncate_tail(url, width.saturating_sub(6))
                    )),
                    SidebarHit::Pr(pr.number),
                );
            }
        }
    }
    if (pr.orphaned || pr.require_review) && !pr.state.is_terminal() && pr.id > 0 {
        body.push_hit(
            Line::from(vec![
                Span::raw("    "),
                Span::styled("[Merge PR]", Style::default().fg(Color::Green)),
            ]),
            SidebarHit::PrMerge(pr.id),
        );
        body.push_hit(
            Line::from(vec![
                Span::raw("    "),
                Span::styled("[Close PR]", Style::default().fg(Color::Red)),
            ]),
            SidebarHit::PrClose(pr.id),
        );
    }
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
    if !state.prs.is_empty() {
        body.gap();
        if body.heading(state, SidebarSection::PullRequests, "Pull requests") {
            let now = jiff::Timestamp::now();
            let expanded =
                state.expand_all || state.listed_all.contains(&SidebarSection::PullRequests);
            let limit = if expanded { state.prs.len() } else { 8 };
            for pr in state.prs.iter().take(limit) {
                pr_rows(&mut body, state, pr, width, now);
            }
            if state.prs.len() > limit {
                body.push_hit(
                    dim(format!("  … {} more", state.prs.len() - limit)),
                    SidebarHit::More(SidebarSection::PullRequests),
                );
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
    use may_client::client::{WirePr, WirePrCheck, WirePrChecks};
    use may_db::PrState;
    use unicode_width::UnicodeWidthStr;

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
        use crate::sidebar::SidebarState;

        let rows = may_protocol::task_rows::task_rows(
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

    fn wire_pr(state: PrState, checks: Option<WirePrChecks>) -> WirePr {
        WirePr {
            id: 0,
            repo_key: String::new(),
            session_id: String::new(),
            orphaned: false,
            require_review: false,
            number: 1,
            title: "title".to_string(),
            state,
            backend: "github".to_string(),
            issue_number: None,
            auto_merge: false,
            base_sha: String::new(),
            head_sha: None,
            merge_commit_sha: None,
            checks,
            checks_error: None,
            events: Vec::new(),
            updated_at: String::new(),
        }
    }

    fn rollup(pending: u32, passed: u32, failed: u32) -> WirePrChecks {
        WirePrChecks {
            pending,
            passed,
            failed,
            items: Vec::new(),
        }
    }

    fn check(
        name: &str,
        state: &str,
        started: Option<&str>,
        finished: Option<&str>,
    ) -> WirePrCheck {
        WirePrCheck {
            name: name.to_string(),
            state: state.to_string(),
            started_at: started.map(|t| t.parse().unwrap()),
            finished_at: finished.map(|t| t.parse().unwrap()),
            url: (state == "failed")
                .then(|| "https://github.com/o/r/actions/runs/9/job/8".to_string()),
        }
    }

    #[test]
    fn a_pr_row_folds_into_its_checks_with_state_duration_and_progress() {
        use crate::sidebar::SidebarState;

        let mut pr = wire_pr(
            PrState::Open,
            Some(WirePrChecks {
                items: vec![
                    check(
                        "fmt",
                        "passed",
                        Some("2026-11-09T10:00:00Z"),
                        Some("2026-11-09T10:00:42Z"),
                    ),
                    check(
                        "测试 テスト 테스트",
                        "failed",
                        Some("2026-11-09T10:00:00Z"),
                        Some("2026-11-09T10:02:05Z"),
                    ),
                    check("clippy", "running", Some("2026-11-09T10:00:00Z"), None),
                    check("docs", "queued", None, None),
                ],
                ..rollup(2, 1, 1)
            }),
        );
        pr.title = "wire it".to_string();
        let now: jiff::Timestamp = "2026-11-09T10:03:00Z".parse().unwrap();
        let render = |state: &SidebarState, pr: &WirePr| -> Vec<String> {
            let mut body = super::Body::default();
            super::pr_rows(&mut body, state, pr, 40, now);
            let lines: Vec<String> = body.lines.iter().map(ToString::to_string).collect();
            // Every row fits the sidebar; runs of padding collapse so the
            // assertions read the content.
            assert!(lines.iter().all(|line| line.width() <= 40), "{lines:?}");
            lines
                .iter()
                .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect()
        };

        let mut state = SidebarState::default();
        assert_eq!(render(&state, &pr), vec!["▸ #1 wire it 2/4 ●"]);
        state.checked.insert(1);
        assert_eq!(
            render(&state, &pr),
            vec![
                "▾ #1 wire it 2/4 ●",
                "state open",
                "✓ fmt 42s",
                "✗ 测试 テスト 테스트 2m05s",
                "…thub.com/o/r/actions/runs/9/job/8",
                "● clippy 3m00s",
                "○ docs",
            ]
        );

        // A failed read is stated on the expanded row.
        pr.checks = None;
        pr.checks_error = Some("GitHub 502".to_string());
        state.checked.insert(1);
        assert!(
            render(&state, &pr)
                .iter()
                .any(|line| line.contains("checks unreadable: GitHub 502"))
        );
    }

    #[test]
    fn the_pr_row_icon_follows_the_check_rollup() {
        use ratatui::style::Color;

        // No rollup read yet: the neutral open dot.
        assert_eq!(pr_icon(&wire_pr(PrState::Open, None)).0, "●");
        // Merging overrides whatever the checks say.
        assert_eq!(
            pr_icon(&wire_pr(PrState::Merging, Some(rollup(2, 0, 0)))).0,
            "●"
        );
        // Failure beats pending; pending beats all-passed.
        let (icon, style) = pr_icon(&wire_pr(PrState::Open, Some(rollup(1, 0, 1))));
        assert_eq!((icon, style.fg), ("●", Some(Color::Red)));
        let (icon, style) = pr_icon(&wire_pr(PrState::Open, Some(rollup(1, 3, 0))));
        assert_eq!((icon, style.fg), ("●", Some(Color::Yellow)));
        let (icon, style) = pr_icon(&wire_pr(PrState::Open, Some(rollup(0, 4, 0))));
        assert_eq!((icon, style.fg), ("✓", Some(Color::Green)));
    }

    #[test]
    fn pr_rows_render_rejected_detail_and_orphaned_user_action_hits() {
        use crate::sidebar::{SidebarHit, SidebarState};
        use may_client::client::WirePrEvent;

        let mut pr = wire_pr(PrState::Rejected, None);
        pr.id = 99;
        pr.orphaned = true;
        pr.events.push(WirePrEvent {
            session_id: "gone".to_string(),
            actor: "main".to_string(),
            message_id: Some(5),
            kind: "rejected".to_string(),
            detail: Some("conflict on src/lib.rs".to_string()),
            created_at: "2026-11-15T10:00:00Z".to_string(),
        });

        let state = SidebarState::default();
        let now: jiff::Timestamp = "2026-11-15T10:05:00Z".parse().unwrap();
        let mut body = super::Body::default();
        super::pr_rows(&mut body, &state, &pr, 50, now);
        let text: Vec<String> = body
            .lines
            .iter()
            .map(|l| {
                l.to_string()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        assert!(text.iter().any(|l| l.contains("[orphaned]")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("[Merge PR]")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("[Close PR]")), "{text:?}");
        assert!(
            body.hits.iter().any(|(_, h)| *h == SidebarHit::PrMerge(99)),
            "{:?}",
            body.hits
        );
        assert!(
            body.hits.iter().any(|(_, h)| *h == SidebarHit::PrClose(99)),
            "{:?}",
            body.hits
        );
    }
}
