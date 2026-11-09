//! The input band: the composer and status line share one background,
//! derived from the terminal's own colours.

use std::time::Duration;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding};
use terminal_colorsaurus::{QueryOptions, color_palette};

/// Columns of padding on each side of the band, and of margin around the
/// chat so its text lines up with the input.
pub const H_PADDING: u16 = 2;

/// Narrowest column that still gets the horizontal padding; below it the
/// content takes the whole width.
const MIN_PADDED_WIDTH: u16 = 9;

/// Share of the way from the terminal background to its foreground that the
/// band sits.
const BAND_SHIFT: f32 = 0.08;

/// How long the startup query waits for the terminal to answer.
const QUERY_TIMEOUT: Duration = Duration::from_millis(500);

/// Horizontal padding for a column `width` cells wide.
#[must_use]
pub const fn padding(width: u16) -> u16 {
    if width >= MIN_PADDED_WIDTH {
        H_PADDING
    } else {
        0
    }
}

/// `area` with its horizontal padding taken off both sides.
#[must_use]
pub const fn inset(area: Rect) -> Rect {
    let pad = padding(area.width);
    Rect {
        x: area.x + pad,
        width: area.width - 2 * pad,
        ..area
    }
}

/// A filled block for a popup, panel or sidebar.
///
/// `bg` is behind every cell, with one column of padding on each side and
/// one row at the bottom. The title, when
/// given, is the block's first row in the `accent` colour; without one the
/// block also pads its top row. The chrome costs two rows and two columns
/// either way, so a layout sized for it never changes with the title.
///
/// `bg` is the terminal-derived band colour; `None` (the terminal did not
/// report its colours) leaves the terminal's own background.
#[must_use]
pub fn panel<'a>(
    title: Option<impl Into<Line<'a>>>,
    accent: Color,
    bg: Option<Color>,
) -> Block<'a> {
    let base = bg.map_or_else(Style::default, |bg| Style::default().bg(bg));
    let block = Block::new().style(base);
    match title {
        Some(title) => {
            let mut title: Line<'a> = title.into();
            title.spans.insert(0, Span::raw(" "));
            block
                .padding(Padding::new(1, 1, 0, 1))
                .title(title.style(Style::default().fg(accent).add_modifier(Modifier::BOLD)))
        }
        None => block.padding(Padding::new(1, 1, 1, 1)),
    }
}

/// The band colour for a terminal drawing `foreground` on `background`: the
/// background shifted slightly toward the foreground, so lighter on a dark
/// theme and darker on a light one.
#[must_use]
pub fn band_color(background: (u8, u8, u8), foreground: (u8, u8, u8)) -> Color {
    let shift = |bg: u8, fg: u8| {
        let (bg, fg) = (f32::from(bg), f32::from(fg));
        // Clamped to a byte, so the cast cannot truncate.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let value = (fg - bg).mul_add(BAND_SHIFT, bg).round().clamp(0.0, 255.0) as u8;
        value
    };
    Color::Rgb(
        shift(background.0, foreground.0),
        shift(background.1, foreground.1),
        shift(background.2, foreground.2),
    )
}

/// Asks the terminal for its colours and derives the band colour from them.
///
/// Blocking: it writes the OSC 10/11 queries and waits for the answer, so
/// callers run it off the event loop and before the terminal is put to other
/// use. `None` when the terminal does not answer; the band then keeps the
/// terminal's default background.
#[must_use]
pub fn query_band_color() -> Option<Color> {
    let mut options = QueryOptions::default();
    options.timeout = QUERY_TIMEOUT;
    match color_palette(options) {
        Ok(palette) => Some(band_color(
            palette.background.scale_to_8bit(),
            palette.foreground.scale_to_8bit(),
        )),
        Err(error) => {
            tracing::debug!(%error, "terminal did not report its colours");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_theme_gets_lighter_band() {
        let Color::Rgb(r, g, b) = band_color((0, 0, 0), (255, 255, 255)) else {
            panic!("expected rgb");
        };
        assert!(r > 0 && r <= 26, "a small step: {r}");
        assert_eq!((r, g), (g, b));
    }

    #[test]
    fn light_theme_gets_darker_band() {
        let Color::Rgb(r, ..) = band_color((255, 255, 255), (0, 0, 0)) else {
            panic!("expected rgb");
        };
        assert!((229..255).contains(&r), "a small step: {r}");
    }

    #[test]
    fn padding_drops_on_narrow_columns() {
        assert_eq!(padding(80), H_PADDING);
        assert_eq!(padding(MIN_PADDED_WIDTH), H_PADDING);
        assert_eq!(padding(MIN_PADDED_WIDTH - 1), 0);
    }
}
