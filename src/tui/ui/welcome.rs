//! The welcome header at the top of a chat: May's logo, name, version and
//! working directory. It is the first element of the stream, so it scrolls
//! away with the conversation.

use askama::Template as _;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use super::chat::suffix_max_width;

/// The cat, drawn in block characters; `●` marks an eye.
const LOGO: &str = include_str!("welcome_logo.txt");
/// The character of [`LOGO`] drawn in the eye colour.
const EYE: char = '●';
/// Columns between the logo and the text beside it.
const GAP: usize = 3;
/// Columns the text keeps at its left edge when there is no logo.
const INDENT: usize = 2;
/// Narrowest text column worth drawing the logo next to.
const MIN_TEXT_WIDTH: usize = 24;
/// Logo row the text block starts on, level with the cat's eyes.
const TEXT_ROW: usize = 3;

/// The three text rows: name, version and working directory.
#[derive(askama::Template)]
#[template(path = "tui_welcome.txt", escape = "none")]
struct WelcomeText<'a> {
    version: &'a str,
    cwd: &'a str,
    /// `cwd` lost its head to fit the width.
    shortened: bool,
}

/// Rows of the header at `width` columns. `cwd` is the working directory as
/// shown (home already written `~`). The logo is dropped when the text would
/// get less than [`MIN_TEXT_WIDTH`] columns beside it.
pub(super) fn welcome_lines(cwd: &str, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width);
    let logo: Vec<&str> = LOGO.lines().collect();
    let logo_width = logo
        .iter()
        .map(|row| UnicodeWidthStr::width(*row))
        .max()
        .unwrap_or(0);
    let with_logo = width >= INDENT + logo_width + GAP + MIN_TEXT_WIDTH;
    let left = if with_logo {
        INDENT + logo_width + GAP
    } else {
        INDENT
    };

    // One column of the budget is the ellipsis a shortened path carries.
    let budget = width.saturating_sub(left);
    let (cwd, shortened) = if UnicodeWidthStr::width(cwd) <= budget {
        (cwd, false)
    } else {
        (suffix_max_width(cwd, budget.saturating_sub(1)), true)
    };
    let text = WelcomeText {
        version: env!("CARGO_PKG_VERSION"),
        cwd,
        shortened,
    }
    .render()
    .expect("welcome template renders");
    let mut text_rows = text.lines().map(str::to_owned);
    let name = text_rows.next().unwrap_or_default();
    let version = text_rows.next().unwrap_or_default();
    let dir = text_rows.next().unwrap_or_default();
    let styled_text = [
        Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(version, Style::default().fg(Color::DarkGray)),
        Span::styled(dir, Style::default().fg(Color::DarkGray)),
    ];

    let mut lines = vec![Line::from("")];
    if with_logo {
        let mut texts = styled_text.into_iter();
        for (index, row) in logo.iter().enumerate() {
            let mut spans = vec![Span::raw(" ".repeat(INDENT))];
            spans.extend(logo_spans(row));
            if index >= TEXT_ROW
                && let Some(span) = texts.next()
            {
                let pad = logo_width - UnicodeWidthStr::width(*row) + GAP;
                spans.push(Span::raw(" ".repeat(pad)));
                spans.push(span);
            }
            lines.push(Line::from(spans));
        }
    } else {
        for span in styled_text {
            lines.push(Line::from(vec![Span::raw(" ".repeat(INDENT)), span]));
        }
    }
    lines.push(Line::from(""));
    lines
}

/// One logo row as spans: the cat in the terminal's foreground, the eyes
/// yellow.
fn logo_spans(row: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut parts = row.split(EYE);
    if let Some(first) = parts.next() {
        spans.push(Span::raw(first.to_owned()));
    }
    for part in parts {
        spans.push(Span::styled(
            EYE.to_string(),
            Style::default().fg(Color::Yellow),
        ));
        spans.push(Span::raw(part.to_owned()));
    }
    spans
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    use super::super::chat::{ChatStream, render};

    fn screen(cwd: &str, width: u16) -> Vec<String> {
        let mut chat = ChatStream::new();
        chat.push_welcome(cwd.to_owned());
        let area = Rect::new(0, 0, width, 14);
        let mut terminal = Terminal::new(TestBackend::new(width, 14)).unwrap();
        terminal
            .draw(|frame| render(frame, area, &chat, usize::MAX))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..14)
            .map(|y| {
                // A wide glyph's trailing columns hold blanks: skip them.
                let mut row = String::new();
                let mut x = 0;
                while x < width {
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    x += u16::try_from(unicode_width::UnicodeWidthStr::width(symbol).max(1))
                        .unwrap();
                }
                row.trim_end().to_owned()
            })
            .collect()
    }

    #[test]
    fn wide_terminal_shows_logo_and_text() {
        let rows = screen("~/Coding/may", 80);
        println!("{}", rows.join("\n"));
        let all = rows.join("\n");
        assert!(all.contains('●'), "{all}");
        assert!(all.contains(" May"), "{all}");
        assert!(
            all.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
            "{all}"
        );
        assert!(all.contains("~/Coding/may"), "{all}");
    }

    #[test]
    fn narrow_terminal_drops_logo_keeps_text() {
        let rows = screen("~/Coding/may", 30);
        let all = rows.join("\n");
        assert!(!all.contains('█'), "{all}");
        assert!(all.contains("May"), "{all}");
        assert!(all.contains("~/Coding/may"), "{all}");
    }

    #[test]
    fn long_cjk_path_keeps_its_tail_within_the_width() {
        for width in [20u16, 30, 46, 60, 80] {
            let rows = screen("~/代码/项目/日本語のフォルダ/한국어폴더/最后一层", width);
            for row in &rows {
                assert!(
                    unicode_width::UnicodeWidthStr::width(row.as_str()) <= usize::from(width),
                    "{row}"
                );
            }
            let all = rows.join("\n");
            assert!(all.contains("最后一层"), "width {width}: {all}");
        }
    }

    #[test]
    fn eyes_are_yellow_and_the_cat_uses_the_default_colour() {
        let mut chat = ChatStream::new();
        chat.push_welcome("~".to_owned());
        let lines = super::welcome_lines("~", 80);
        let eye_count = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.style.fg == Some(ratatui::style::Color::Yellow))
            .count();
        assert_eq!(eye_count, 2);
        let body = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.content.contains('█'))
            .all(|span| span.style.fg.is_none());
        assert!(body);
    }
}
