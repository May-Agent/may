//! The `/effort` panel: a centred slider over the selected model's effort
//! ladder. Moving the selection applies nothing; the key handler applies it
//! on Enter.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use super::super::super::app::App;

const HEADING: &str = "Effort";
const FASTER: &str = "Faster";
const SMARTER: &str = "Smarter";
const FOOTER: &str = "←/→ to adjust · Enter to confirm · Esc to cancel";
const DEFAULT_NOTE: &str = "The default effort for this model";
const TRACK: &str = "─";
const MARKER: &str = "▲";

/// Rows the panel needs: heading, spacer, end labels, track, level names,
/// spacer, description, spacer, footer.
pub(super) const ROWS: usize = 9;

/// Display columns kept between the end labels at the narrowest.
const END_LABEL_GAP: usize = 2;

/// A short, neutral description of a level.
fn describe(level: &str) -> &'static str {
    match level {
        "none" => "No reasoning before answering",
        "minimal" => "Very little reasoning before answering",
        "low" => "Brief reasoning for quick responses",
        "medium" => "Balanced reasoning for most tasks",
        "high" => "Thorough reasoning for harder tasks",
        "xhigh" => "Extended reasoning for difficult work",
        "max" => "The deepest reasoning, slowest and most costly",
        other => unreachable!("effort ladders hold only known levels, got {other:?}"),
    }
}

fn spaces(count: usize) -> String {
    " ".repeat(count)
}

/// A line of `text` centred in `width` display columns.
fn centred(text: &str, width: usize) -> Line<'static> {
    let pad = width.saturating_sub(UnicodeWidthStr::width(text)) / 2;
    Line::from(vec![Span::raw(spaces(pad)), Span::raw(text.to_owned())])
}

/// The panel's lines for `width` columns: `ladder` with the level at `index`
/// selected, `default` the model's default level.
pub(super) fn panel_lines(
    ladder: &[&'static str],
    index: usize,
    default: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let count = ladder.len().max(1);
    let widest = ladder
        .iter()
        .map(|level| UnicodeWidthStr::width(*level))
        .max()
        .unwrap_or(0);
    let min_block =
        UnicodeWidthStr::width(FASTER) + UnicodeWidthStr::width(SMARTER) + END_LABEL_GAP;
    let wanted = ((widest + 4) * count).max(min_block);
    let cell = (wanted.min(width.saturating_sub(2)) / count).max(1);
    let block = cell * count;
    let pad = spaces(width.saturating_sub(block) / 2);

    let ends_gap = block
        .saturating_sub(UnicodeWidthStr::width(FASTER) + UnicodeWidthStr::width(SMARTER))
        .max(1);
    let ends = Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled(FASTER, dim),
        Span::raw(spaces(ends_gap)),
        Span::styled(SMARTER, dim),
    ]);

    let marker_at = cell * index + cell / 2;
    let track = Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled(TRACK.repeat(marker_at), dim),
        Span::styled(
            MARKER,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(TRACK.repeat(block.saturating_sub(marker_at + 1)), dim),
    ]);

    let mut names = vec![Span::raw(pad)];
    let mut cursor = 0;
    for (position, level) in ladder.iter().enumerate() {
        let label_width = UnicodeWidthStr::width(*level);
        let start = (cell * position + cell / 2)
            .saturating_sub(label_width / 2)
            .max(cursor);
        names.push(Span::raw(spaces(start - cursor)));
        let style = if position == index {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        names.push(Span::styled(*level, style));
        cursor = start + label_width;
    }

    let selected = ladder.get(index).copied().unwrap_or(default);
    let description = if selected == default {
        DEFAULT_NOTE
    } else {
        describe(selected)
    };

    vec![
        Line::from(Span::styled(
            format!(" {HEADING}"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        ends,
        track,
        Line::from(names),
        Line::from(""),
        centred(description, width),
        Line::from(""),
        Line::from(Span::styled(format!(" {FOOTER}"), dim)),
    ]
}

/// Render the effort panel into `area`.
pub(super) fn render_effort_select_popup(frame: &mut Frame, area: Rect, app: &App) {
    let lines = panel_lines(
        &app.effort_ladder,
        app.effort_index,
        crate::models::DEFAULT_EFFORT,
        usize::from(area.width),
    );
    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    const LADDER: [&str; 6] = ["none", "low", "medium", "high", "xhigh", "max"];

    fn column_of(line: &str, needle: &str) -> usize {
        let byte = line.find(needle).expect("needle on line");
        UnicodeWidthStr::width(&line[..byte])
    }

    /// The marker sits over the middle of the selected level's name and the
    /// block is centred, at every width and every selection.
    #[test]
    fn marker_is_over_the_selected_level_and_the_block_is_centred() {
        for width in [40_usize, 64, 80, 120, 200] {
            for (index, level) in LADDER.iter().enumerate() {
                let lines = panel_lines(&LADDER, index, "medium", width);
                assert_eq!(lines.len(), ROWS);
                let track = text(&lines[3]);
                let names = text(&lines[4]);
                let marker = column_of(&track, MARKER);
                let name_mid = column_of(&names, level) + UnicodeWidthStr::width(*level) / 2;
                assert!(
                    marker.abs_diff(name_mid) <= 1,
                    "width {width} index {index}: marker {marker}, name middle {name_mid}"
                );
                let left = track.len() - track.trim_start().len();
                let right = width - UnicodeWidthStr::width(track.as_str());
                assert!(left.abs_diff(right) <= 1, "width {width}: not centred");
            }
        }
    }

    #[test]
    fn default_level_reads_as_the_model_default() {
        let lines = panel_lines(&LADDER, 2, "medium", 80);
        assert!(text(&lines[6]).contains(DEFAULT_NOTE));
        let lines = panel_lines(&LADDER, 3, "medium", 80);
        assert!(text(&lines[6]).contains(describe("high")));
    }

    /// The panel at 120 columns, as plain text.
    #[test]
    fn panel_at_120_columns() {
        let lines = panel_lines(&LADDER, 2, "medium", 120);
        let rendered = lines.iter().map(text).collect::<Vec<_>>().join("\n");
        println!("{rendered}");
        assert!(rendered.contains(FASTER));
        assert!(rendered.contains(SMARTER));
        assert!(rendered.contains(FOOTER));
    }
}
