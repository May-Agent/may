//! A test backend that models what a real terminal shows.
//!
//! `TestBackend` stores each written cell independently, so a wide glyph
//! written over the halves of two older wide glyphs leaves both of them in
//! its buffer even though a terminal erases them. [`ScreenBackend`] keeps a
//! grid with terminal semantics — writing over any half of a wide glyph
//! erases the whole glyph — so tests can compare the *visible* screen after
//! the buffer diff with the frame that was rendered.

use std::convert::Infallible;

use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Size};
use unicode_width::UnicodeWidthStr;

/// Marker the model shows for a cell holding a graphics escape sequence.
pub const IMAGE_MARK: &str = "▣";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Slot {
    /// A glyph (or blank) starting at this column.
    Lead(String, usize),
    /// Continuation column of the wide glyph to its left.
    Cont,
}

/// Terminal-faithful test backend.
#[derive(Debug)]
pub struct ScreenBackend {
    inner: TestBackend,
    grid: Vec<Vec<Slot>>,
}

fn glyph(cell: &Cell) -> (String, usize) {
    let symbol = cell.symbol();
    if symbol.contains('\x1b') {
        return (IMAGE_MARK.to_string(), 1);
    }
    (symbol.to_string(), symbol.width().max(1))
}

impl ScreenBackend {
    /// A blank `width` x `height` screen.
    #[must_use]
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            inner: TestBackend::new(width, height),
            grid: vec![
                vec![Slot::Lead(" ".to_string(), 1); usize::from(width)];
                usize::from(height)
            ],
        }
    }

    /// Resize the screen (blanking it, as the next draw repaints all).
    pub fn resize(&mut self, width: u16, height: u16) {
        self.inner.resize(width, height);
        self.grid =
            vec![vec![Slot::Lead(" ".to_string(), 1); usize::from(width)]; usize::from(height)];
    }

    fn erase(&mut self, x: usize, y: usize) {
        let row = &mut self.grid[y];
        // Find the lead of the glyph covering column `x`.
        let mut lead = x;
        while lead > 0 && row[lead] == Slot::Cont {
            lead -= 1;
        }
        let width = match &row[lead] {
            Slot::Lead(_, width) => *width,
            Slot::Cont => 1,
        };
        for col in lead..(lead + width).min(row.len()) {
            row[col] = Slot::Lead(" ".to_string(), 1);
        }
    }

    fn put(&mut self, x: u16, y: u16, cell: &Cell) {
        let (x, y) = (usize::from(x), usize::from(y));
        let (symbol, width) = glyph(cell);
        let width = width.min(self.grid[y].len() - x);
        for col in x..x + width {
            self.erase(col, y);
        }
        self.grid[y][x] = Slot::Lead(symbol, width);
        for col in x + 1..x + width {
            self.grid[y][col] = Slot::Cont;
        }
    }

    /// Visible text of each row, trailing blanks trimmed.
    #[must_use]
    pub fn screen(&self) -> Vec<String> {
        self.grid
            .iter()
            .map(|row| {
                let mut text = String::new();
                for slot in row {
                    if let Slot::Lead(symbol, _) = slot {
                        text.push_str(symbol);
                    }
                }
                text.trim_end().to_string()
            })
            .collect()
    }
}

/// Visible text of each row of a rendered frame buffer, as [`ScreenBackend::screen`]
/// reports it.
#[must_use]
pub fn buffer_screen(buf: &Buffer) -> Vec<String> {
    let area = buf.area;
    (area.top()..area.bottom())
        .map(|y| {
            let mut text = String::new();
            let mut x = area.left();
            while x < area.right() {
                let (symbol, width) = glyph(&buf[(x, y)]);
                text.push_str(&symbol);
                x += u16::try_from(width).unwrap_or(1);
            }
            text.trim_end().to_string()
        })
        .collect()
}

impl Backend for ScreenBackend {
    type Error = Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let cells: Vec<(u16, u16, &Cell)> = content.collect();
        for &(x, y, cell) in &cells {
            self.put(x, y, cell);
        }
        self.inner.draw(cells.into_iter())
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        for row in &mut self.grid {
            for slot in row.iter_mut() {
                *slot = Slot::Lead(" ".to_string(), 1);
            }
        }
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        assert!(
            matches!(clear_type, ClearType::All),
            "ScreenBackend models full clears only"
        );
        self.clear()
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }

    fn scroll_region_up(
        &mut self,
        _region: std::ops::Range<u16>,
        _line_count: u16,
    ) -> Result<(), Self::Error> {
        unimplemented!("the chat view never scrolls terminal regions")
    }

    fn scroll_region_down(
        &mut self,
        _region: std::ops::Range<u16>,
        _line_count: u16,
    ) -> Result<(), Self::Error> {
        unimplemented!("the chat view never scrolls terminal regions")
    }
}
