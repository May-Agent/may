//! ANSI SGR and carriage-return line renderer for terminal and pipeline output.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;

#[derive(Clone)]
struct Cell {
    grapheme: String,
    style: Style,
}

const fn ansi_16_fg(code: u16) -> Option<Color> {
    match code {
        30 => Some(Color::Black),
        31 => Some(Color::Red),
        32 => Some(Color::Green),
        33 => Some(Color::Yellow),
        34 => Some(Color::Blue),
        35 => Some(Color::Magenta),
        36 => Some(Color::Cyan),
        37 => Some(Color::Gray),
        90 => Some(Color::DarkGray),
        91 => Some(Color::LightRed),
        92 => Some(Color::LightGreen),
        93 => Some(Color::LightYellow),
        94 => Some(Color::LightBlue),
        95 => Some(Color::LightMagenta),
        96 => Some(Color::LightCyan),
        97 => Some(Color::White),
        _ => None,
    }
}

const fn ansi_16_bg(code: u16) -> Option<Color> {
    match code {
        40 => Some(Color::Black),
        41 => Some(Color::Red),
        42 => Some(Color::Green),
        43 => Some(Color::Yellow),
        44 => Some(Color::Blue),
        45 => Some(Color::Magenta),
        46 => Some(Color::Cyan),
        47 => Some(Color::Gray),
        100 => Some(Color::DarkGray),
        101 => Some(Color::LightRed),
        102 => Some(Color::LightGreen),
        103 => Some(Color::LightYellow),
        104 => Some(Color::LightBlue),
        105 => Some(Color::LightMagenta),
        106 => Some(Color::LightCyan),
        107 => Some(Color::White),
        _ => None,
    }
}

fn apply_sgr(params: &[u16], mut style: Style) -> Style {
    let defaults = [0u16];
    let codes = if params.is_empty() {
        &defaults[..]
    } else {
        params
    };
    let mut i = 0;
    while i < codes.len() {
        let code = codes[i];
        match code {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            39 => style.fg = None,
            49 => style.bg = None,
            38 | 48 if i + 1 < codes.len() => {
                let is_fg = code == 38;
                match codes[i + 1] {
                    5 if i + 2 < codes.len() => {
                        let idx = u8::try_from(codes[i + 2]).unwrap_or(0);
                        let color = Color::Indexed(idx);
                        if is_fg {
                            style.fg = Some(color);
                        } else {
                            style.bg = Some(color);
                        }
                        i += 2;
                    }
                    2 if i + 4 < codes.len() => {
                        let r = u8::try_from(codes[i + 2].min(255)).unwrap_or(0);
                        let g = u8::try_from(codes[i + 3].min(255)).unwrap_or(0);
                        let b = u8::try_from(codes[i + 4].min(255)).unwrap_or(0);
                        let color = Color::Rgb(r, g, b);
                        if is_fg {
                            style.fg = Some(color);
                        } else {
                            style.bg = Some(color);
                        }
                        i += 4;
                    }
                    _ => {}
                }
            }
            c => {
                if let Some(fg) = ansi_16_fg(c) {
                    style.fg = Some(fg);
                } else if let Some(bg) = ansi_16_bg(c) {
                    style.bg = Some(bg);
                }
            }
        }
        i += 1;
    }
    style
}

fn cells_to_spans(cells: &[Cell], fallback: Style, patch: Style) -> Vec<Span<'static>> {
    if cells.is_empty() {
        return Vec::new();
    }
    let resolve = |s: Style| {
        let base = if s == Style::default() {
            fallback
        } else {
            fallback.patch(s)
        };
        base.patch(patch)
    };
    let mut spans = Vec::new();
    let mut cur_style = resolve(cells[0].style);
    let mut cur_text = cells[0].grapheme.clone();
    for cell in &cells[1..] {
        let next_style = resolve(cell.style);
        if next_style == cur_style {
            cur_text.push_str(&cell.grapheme);
        } else {
            spans.push(Span::styled(cur_text, cur_style));
            cur_style = next_style;
            cur_text = String::new();
            cur_text.clone_from(&cell.grapheme);
        }
    }
    spans.push(Span::styled(cur_text, cur_style));
    spans
}

/// Whether `text` carries ANSI escape sequences or carriage-return overwrites.
#[must_use]
pub fn has_ansi_or_cr(text: &str) -> bool {
    text.contains('\x1b') || text.contains('\r')
}

/// Parses `input` into styled lines prefixed with `prefix`, preserving SGR
/// colors/modifiers and simulating `\r` and `\x1b[K` overwrites.
#[must_use]
pub fn parse_ansi_lines(
    input: &str,
    prefix: &str,
    prefix_style: Style,
    default_text_style: Style,
    selection_style: Style,
) -> Vec<Line<'static>> {
    if input.is_empty() {
        return Vec::new();
    }
    let bytes = input.as_bytes();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut row: Vec<Cell> = Vec::new();
    let mut cursor: usize = 0;
    let mut style = Style::default();

    let flush_row = |row: &mut Vec<Cell>, lines: &mut Vec<Line<'static>>| {
        let mut spans = Vec::new();
        if !prefix.is_empty() {
            spans.push(Span::styled(
                prefix.to_string(),
                prefix_style.patch(selection_style),
            ));
        }
        spans.extend(cells_to_spans(row, default_text_style, selection_style));
        lines.push(Line::from(spans));
        row.clear();
    };

    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => {
                if i + 1 >= bytes.len() {
                    break;
                }
                match bytes[i + 1] {
                    b'[' => {
                        let mut j = i + 2;
                        while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                            j += 1;
                        }
                        if j >= bytes.len() {
                            break;
                        }
                        let final_byte = bytes[j];
                        let raw = bytes
                            .get(i + 2..j)
                            .and_then(|slice| std::str::from_utf8(slice).ok())
                            .unwrap_or_default();
                        let params: Vec<u16> = if raw.is_empty() {
                            Vec::new()
                        } else {
                            raw.split([';', ':'])
                                .map(|part| part.parse::<u16>().unwrap_or(0))
                                .collect()
                        };
                        match final_byte {
                            b'm' => style = apply_sgr(&params, style),
                            b'K' => match params.first().copied().unwrap_or(0) {
                                0 => row.truncate(cursor.min(row.len())),
                                1 => {
                                    let end = cursor.min(row.len().saturating_sub(1));
                                    for cell in row.iter_mut().take(end + 1) {
                                        cell.grapheme.clear();
                                        cell.grapheme.push(' ');
                                        cell.style = Style::default();
                                    }
                                }
                                2 => row.clear(),
                                _ => {}
                            },
                            b'G' => {
                                cursor =
                                    usize::from(params.first().copied().unwrap_or(1).max(1)) - 1;
                            }
                            _ => {}
                        }
                        i = j + 1;
                    }
                    b']' => {
                        let mut j = i + 2;
                        while j < bytes.len() {
                            if bytes[j] == 0x07 {
                                j += 1;
                                break;
                            }
                            if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                                j += 2;
                                break;
                            }
                            j += 1;
                        }
                        i = j;
                    }
                    _ => i += 2,
                }
            }
            b'\r' => {
                if bytes.get(i + 1) == Some(&b'\n') {
                    flush_row(&mut row, &mut lines);
                    cursor = 0;
                    i += 2;
                } else {
                    cursor = 0;
                    i += 1;
                }
            }
            b'\n' => {
                flush_row(&mut row, &mut lines);
                cursor = 0;
                i += 1;
            }
            0x08 => {
                cursor = cursor.saturating_sub(1);
                i += 1;
            }
            _ => {
                // Consume a contiguous run of printable UTF-8 up to the next control/ESC byte.
                let start = i;
                while i < bytes.len()
                    && bytes[i] != 0x1b
                    && bytes[i] != b'\r'
                    && bytes[i] != b'\n'
                    && bytes[i] != 0x08
                {
                    i += 1;
                }
                let run = bytes
                    .get(start..i)
                    .map_or_default(|slice| String::from_utf8_lossy(slice));
                for g in run.graphemes(true) {
                    while row.len() < cursor {
                        row.push(Cell {
                            grapheme: " ".to_string(),
                            style: Style::default(),
                        });
                    }
                    let cell = Cell {
                        grapheme: g.to_string(),
                        style,
                    };
                    if cursor < row.len() {
                        row[cursor] = cell;
                    } else {
                        row.push(cell);
                    }
                    cursor += 1;
                }
            }
        }
    }
    if !row.is_empty() {
        flush_row(&mut row, &mut lines);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_sgr_colors_and_cjk_graphemes() {
        let input = "\x1b[1;31m编译错误\x1b[0m \x1b[38;5;208m警告です\x1b[0m \x1b[38;2;16;185;129m통과\x1b[0m\n";
        let lines = parse_ansi_lines(
            input,
            "│ ",
            Style::default().fg(Color::DarkGray),
            Style::default().fg(Color::Gray),
            Style::default(),
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content, "│ ");
        assert_eq!(lines[0].spans[1].content, "编译错误");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Red));
        assert!(
            lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[0].spans[3].content, "警告です");
        assert_eq!(lines[0].spans[3].style.fg, Some(Color::Indexed(208)));
        assert_eq!(lines[0].spans[5].content, "통과");
        assert_eq!(lines[0].spans[5].style.fg, Some(Color::Rgb(16, 185, 129)));
    }

    #[test]
    fn simulates_carriage_return_and_erase_in_line() {
        let input = "Building 1/8\rBuilding 8/8\nChecking...\rDone\x1b[K\n";
        let lines = parse_ansi_lines(
            input,
            "",
            Style::default(),
            Style::default(),
            Style::default(),
        );
        let plain: Vec<String> = lines
            .into_iter()
            .map(|l| {
                l.spans
                    .into_iter()
                    .map(|s| s.content.into_owned())
                    .collect()
            })
            .collect();
        assert_eq!(plain, vec!["Building 8/8", "Done"]);
    }
}
