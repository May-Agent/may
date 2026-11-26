//! Virtual terminal state machine and bounded frame-quiescence waiter.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_io::Timer;
use event_listener::Event;
use futures_lite::future::race;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

/// Default virtual terminal width in character cells.
pub const DEFAULT_COLS: u16 = 120;

/// Default virtual terminal height in character cells.
pub const DEFAULT_ROWS: u16 = 40;

/// Default maximum primary-screen scrollback lines kept in memory.
pub const DEFAULT_SCROLLBACK_LIMIT: usize = 2_000;

/// Default quiet period (in milliseconds) required before a screen update is
/// considered settled.
pub const DEFAULT_SETTLE_QUIET_MS: u64 = 50;

/// Default upper bound (in milliseconds) when waiting for a frame to settle.
pub const DEFAULT_SETTLE_TIMEOUT_MS: u64 = 750;

/// Maximum depth of the Kitty keyboard protocol flag stack per screen buffer.
const MAX_KITTY_STACK_DEPTH: usize = 32;

/// Terminal color value for a cell foreground or background.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// Terminal default foreground or background.
    #[default]
    Default,
    /// 8-bit palette index (`0..=15` ANSI, `16..=231` RGB cube, `232..=255` grayscale).
    Indexed(u8),
    /// 24-bit truecolor RGB.
    Rgb(u8, u8, u8),
}

/// Visual styling attributes of a single terminal cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    /// Foreground color.
    pub fg: Color,
    /// Background color.
    pub bg: Color,
    /// Bold / bright intensity (SGR 1).
    pub bold: bool,
    /// Faint / dim intensity (SGR 2).
    pub dim: bool,
    /// Italic text (SGR 3).
    pub italic: bool,
    /// Underlined text (SGR 4).
    pub underline: bool,
    /// Blinking text (SGR 5 / 6).
    pub blink: bool,
    /// Reverse video (SGR 7).
    pub reverse: bool,
    /// Invisible / concealed text (SGR 8).
    pub hidden: bool,
    /// Strikethrough text (SGR 9).
    pub strikethrough: bool,
}

/// A single cell in the virtual terminal grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// Base Unicode scalar displayed in this cell.
    pub ch: char,
    /// Combining characters attached to this cell (empty in the common case).
    pub combining: Vec<char>,
    /// Display width: `1` for normal cells, `2` for wide leading cells, `0` for
    /// the trailing continuation cell of a wide character.
    pub width: u8,
    /// SGR visual attributes of this cell.
    pub style: CellStyle,
}

impl Default for Cell {
    fn default() -> Self {
        Self::blank(CellStyle::default())
    }
}

impl Cell {
    /// Constructs a blank space cell carrying `style`.
    #[must_use]
    pub const fn blank(style: CellStyle) -> Self {
        Self {
            ch: ' ',
            combining: Vec::new(),
            width: 1,
            style,
        }
    }

    /// Constructs the trailing continuation cell (`width == 0`) of a wide character.
    #[must_use]
    pub const fn continuation(style: CellStyle) -> Self {
        Self {
            ch: ' ',
            combining: Vec::new(),
            width: 0,
            style,
        }
    }

    /// Whether this cell is the trailing half of a double-width character.
    #[must_use]
    pub const fn is_continuation(&self) -> bool {
        self.width == 0
    }

    /// Whether this cell holds only a space with no combining marks.
    #[must_use]
    pub const fn is_blank_glyph(&self) -> bool {
        self.width == 1 && self.ch == ' ' && self.combining.is_empty()
    }

    /// Appends this cell's printable characters to `out`, skipping continuation cells.
    pub fn push_text(&self, out: &mut String) {
        if self.is_continuation() {
            return;
        }
        out.push(self.ch);
        for &mark in &self.combining {
            out.push(mark);
        }
    }
}

/// 0-based cursor coordinates on the visible screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorPosition {
    /// 0-based column index (`0..cols`).
    pub col: u16,
    /// 0-based row index (`0..rows`).
    pub row: u16,
}

/// Active terminal mouse tracking mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseTrackingMode {
    /// Mouse reporting is disabled.
    #[default]
    Off,
    /// Normal X10/VT200 button press and release reporting (`?1000`).
    Normal,
    /// Button-event reporting including drag motion while pressed (`?1002`).
    ButtonEvent,
    /// Any-event reporting including pointer motion without buttons (`?1003`).
    AnyEvent,
}

/// Terminal modes tracked from DEC private sequences and Kitty keyboard stacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalModes {
    /// DECCKM (`?1`): application cursor keys.
    pub cursor_keys_application: bool,
    /// DECOM (`?6`): cursor addressing relative to scroll region.
    pub origin_mode: bool,
    /// DECAWM (`?7`): automatic line wrap at right margin.
    pub autowrap: bool,
    /// DECTCEM (`?25`): cursor visibility.
    pub cursor_visible: bool,
    /// Alternate screen buffer active (`?1049` / `?1047` / `?47`).
    pub alternate_screen: bool,
    /// Bracketed paste mode (`?2004`).
    pub bracketed_paste: bool,
    /// Mouse tracking level (`?1000` / `?1002` / `?1003`).
    pub mouse_tracking: MouseTrackingMode,
    /// SGR mouse coordinate encoding (`?1006`).
    pub mouse_sgr: bool,
    /// Synchronized output mode (`?2026`): redraw in progress.
    pub sync_output_active: bool,
    /// Top flags byte on the active Kitty keyboard protocol stack.
    pub kitty_keyboard_flags: u8,
}

impl Default for TerminalModes {
    fn default() -> Self {
        Self {
            cursor_keys_application: false,
            origin_mode: false,
            autowrap: true,
            cursor_visible: true,
            alternate_screen: false,
            bracketed_paste: false,
            mouse_tracking: MouseTrackingMode::Off,
            mouse_sgr: false,
            sync_output_active: false,
            kitty_keyboard_flags: 0,
        }
    }
}

impl TerminalModes {
    /// Returns `true` when the program has enabled any mouse tracking mode.
    #[must_use]
    pub const fn is_mouse_enabled(&self) -> bool {
        !matches!(self.mouse_tracking, MouseTrackingMode::Off)
    }

    /// Returns concise labels describing every non-default active mode for observation.
    #[must_use]
    pub fn active_labels(&self) -> Vec<String> {
        let mut labels = Vec::new();
        if self.cursor_keys_application {
            labels.push("DECCKM".to_owned());
        }
        if self.bracketed_paste {
            labels.push("bracketed-paste".to_owned());
        }
        match self.mouse_tracking {
            MouseTrackingMode::Off => {}
            MouseTrackingMode::Normal => {
                labels.push(if self.mouse_sgr {
                    "mouse:sgr(normal)".to_owned()
                } else {
                    "mouse:normal".to_owned()
                });
            }
            MouseTrackingMode::ButtonEvent => {
                labels.push(if self.mouse_sgr {
                    "mouse:sgr(button)".to_owned()
                } else {
                    "mouse:button".to_owned()
                });
            }
            MouseTrackingMode::AnyEvent => {
                labels.push(if self.mouse_sgr {
                    "mouse:sgr(any)".to_owned()
                } else {
                    "mouse:any".to_owned()
                });
            }
        }
        if self.kitty_keyboard_flags != 0 {
            labels.push(format!("kitty-keyboard({})", self.kitty_keyboard_flags));
        }
        if self.sync_output_active {
            labels.push("sync-output".to_owned());
        }
        labels
    }
}

/// Immutable snapshot of the visible terminal state for observation and rasterization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSnapshot {
    /// Visible width in cells.
    pub cols: u16,
    /// Visible height in cells.
    pub rows: u16,
    /// Row-major cell grid (`rows` rows, each `cols` cells).
    pub cells: Vec<Vec<Cell>>,
    /// Current 0-based cursor position.
    pub cursor: CursorPosition,
    /// Active terminal modes.
    pub modes: TerminalModes,
    /// Window title set via OSC 0 or OSC 2 (empty if unset).
    pub title: String,
    /// Monotonic generation counter advanced on any visible state mutation.
    pub generation: u64,
}

impl ScreenSnapshot {
    /// Returns the cell at 0-based `(col, row)`, or `None` if out of bounds.
    #[must_use]
    pub fn cell(&self, col: u16, row: u16) -> Option<&Cell> {
        self.cells
            .get(usize::from(row))
            .and_then(|line| line.get(usize::from(col)))
    }

    /// Renders clean trimmed screen text with trailing whitespace and trailing
    /// empty lines stripped.
    #[must_use]
    pub fn plain_text(&self) -> String {
        let mut lines: Vec<String> = self
            .cells
            .iter()
            .map(|row| {
                let mut line = String::with_capacity(row.len());
                for cell in row {
                    cell.push_text(&mut line);
                }
                line.trim_end().to_owned()
            })
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SavedCursor {
    cursor: CursorPosition,
    style: CellStyle,
    origin_mode: bool,
    autowrap: bool,
    pending_wrap: bool,
}

#[derive(Debug)]
struct TerminalState {
    cols: u16,
    rows: u16,
    cells: Vec<Vec<Cell>>,
    scrollback: VecDeque<Vec<Cell>>,
    scrollback_limit: usize,
    primary_cells_backup: Option<Vec<Vec<Cell>>>,
    primary_cursor_backup: Option<SavedCursor>,
    saved_cursor_main: Option<SavedCursor>,
    saved_cursor_alt: Option<SavedCursor>,
    kitty_stack_main: Vec<u8>,
    kitty_stack_alt: Vec<u8>,
    cursor: CursorPosition,
    pending_wrap: bool,
    style: CellStyle,
    modes: TerminalModes,
    scroll_top: u16,
    scroll_bottom: u16,
    title: String,
    generation: u64,
    dirty_during_sync: bool,
}

impl TerminalState {
    fn new(cols: u16, rows: u16, scrollback_limit: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            cells: blank_grid(cols, rows, CellStyle::default()),
            scrollback: VecDeque::new(),
            scrollback_limit,
            primary_cells_backup: None,
            primary_cursor_backup: None,
            saved_cursor_main: None,
            saved_cursor_alt: None,
            kitty_stack_main: Vec::new(),
            kitty_stack_alt: Vec::new(),
            cursor: CursorPosition::default(),
            pending_wrap: false,
            style: CellStyle::default(),
            modes: TerminalModes::default(),
            scroll_top: 0,
            scroll_bottom: rows - 1,
            title: String::new(),
            generation: 0,
            dirty_during_sync: false,
        }
    }

    const fn mark_dirty(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if self.modes.sync_output_active {
            self.dirty_during_sync = true;
        }
    }

    fn reset(&mut self) {
        self.cells = blank_grid(self.cols, self.rows, CellStyle::default());
        self.scrollback.clear();
        self.primary_cells_backup = None;
        self.primary_cursor_backup = None;
        self.saved_cursor_main = None;
        self.saved_cursor_alt = None;
        self.kitty_stack_main.clear();
        self.kitty_stack_alt.clear();
        self.cursor = CursorPosition::default();
        self.pending_wrap = false;
        self.style = CellStyle::default();
        self.modes = TerminalModes::default();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.title.clear();
        self.dirty_during_sync = false;
        self.mark_dirty();
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if self.cols == cols && self.rows == rows {
            return;
        }
        resize_grid(&mut self.cells, cols, rows);
        if let Some(primary) = &mut self.primary_cells_backup {
            resize_grid(primary, cols, rows);
        }
        self.cols = cols;
        self.rows = rows;
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.pending_wrap = false;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.mark_dirty();
    }

    const fn active_saved_cursor_mut(&mut self) -> &mut Option<SavedCursor> {
        if self.modes.alternate_screen {
            &mut self.saved_cursor_alt
        } else {
            &mut self.saved_cursor_main
        }
    }

    const fn active_kitty_stack_mut(&mut self) -> &mut Vec<u8> {
        if self.modes.alternate_screen {
            &mut self.kitty_stack_alt
        } else {
            &mut self.kitty_stack_main
        }
    }

    fn sync_kitty_flags(&mut self) {
        let top = if self.modes.alternate_screen {
            self.kitty_stack_alt.last().copied().unwrap_or(0)
        } else {
            self.kitty_stack_main.last().copied().unwrap_or(0)
        };
        self.modes.kitty_keyboard_flags = top;
    }

    const fn snapshot_saved_cursor(&self) -> SavedCursor {
        SavedCursor {
            cursor: self.cursor,
            style: self.style,
            origin_mode: self.modes.origin_mode,
            autowrap: self.modes.autowrap,
            pending_wrap: self.pending_wrap,
        }
    }

    fn restore_saved_cursor(&mut self, saved: SavedCursor) {
        self.cursor.col = saved.cursor.col.min(self.cols - 1);
        self.cursor.row = saved.cursor.row.min(self.rows - 1);
        self.style = saved.style;
        self.modes.origin_mode = saved.origin_mode;
        self.modes.autowrap = saved.autowrap;
        self.pending_wrap = saved.pending_wrap;
        self.mark_dirty();
    }

    const fn save_cursor(&mut self) {
        let saved = self.snapshot_saved_cursor();
        *self.active_saved_cursor_mut() = Some(saved);
    }

    fn restore_cursor(&mut self) {
        let saved = *self.active_saved_cursor_mut();
        if let Some(saved) = saved {
            self.restore_saved_cursor(saved);
        } else {
            self.cursor = CursorPosition::default();
            self.pending_wrap = false;
            self.style = CellStyle::default();
            self.mark_dirty();
        }
    }

    fn enter_alternate_screen(&mut self, save_cursor: bool) {
        if save_cursor {
            self.primary_cursor_backup = Some(self.snapshot_saved_cursor());
        }
        if !self.modes.alternate_screen {
            let previous = std::mem::replace(
                &mut self.cells,
                blank_grid(self.cols, self.rows, CellStyle::default()),
            );
            self.primary_cells_backup = Some(previous);
            self.modes.alternate_screen = true;
            self.cursor = CursorPosition::default();
            self.pending_wrap = false;
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
            self.sync_kitty_flags();
            self.mark_dirty();
        }
    }

    fn leave_alternate_screen(&mut self, restore_cursor: bool) {
        if self.modes.alternate_screen {
            if let Some(primary) = self.primary_cells_backup.take() {
                self.cells = primary;
                resize_grid(&mut self.cells, self.cols, self.rows);
            }
            self.modes.alternate_screen = false;
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
            self.sync_kitty_flags();
            if restore_cursor && let Some(saved) = self.primary_cursor_backup.take() {
                self.restore_saved_cursor(saved);
                return;
            }
            self.mark_dirty();
        }
    }

    fn scroll_up(&mut self, count: u16) {
        let top = usize::from(self.scroll_top);
        let bottom = usize::from(self.scroll_bottom).min(self.cells.len().saturating_sub(1));
        if top > bottom {
            return;
        }
        let region_len = bottom - top + 1;
        let steps = usize::from(count).min(region_len);
        let blank_style = self.erase_style();
        let record_scrollback =
            !self.modes.alternate_screen && top == 0 && self.scrollback_limit > 0;
        for _ in 0..steps {
            let removed = self.cells.remove(top);
            if record_scrollback {
                if self.scrollback.len() >= self.scrollback_limit {
                    self.scrollback.pop_front();
                }
                self.scrollback.push_back(removed);
            }
            self.cells.insert(
                bottom,
                vec![Cell::blank(blank_style); usize::from(self.cols)],
            );
        }
        self.mark_dirty();
    }

    fn scroll_down(&mut self, count: u16) {
        let top = usize::from(self.scroll_top);
        let bottom = usize::from(self.scroll_bottom).min(self.cells.len().saturating_sub(1));
        if top > bottom {
            return;
        }
        let region_len = bottom - top + 1;
        let steps = usize::from(count).min(region_len);
        let blank_style = self.erase_style();
        for _ in 0..steps {
            self.cells.remove(bottom);
            self.cells
                .insert(top, vec![Cell::blank(blank_style); usize::from(self.cols)]);
        }
        self.mark_dirty();
    }

    fn linefeed(&mut self) {
        self.pending_wrap = false;
        if self.cursor.row == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
            self.mark_dirty();
        }
    }

    fn reverse_index(&mut self) {
        self.pending_wrap = false;
        if self.cursor.row == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
            self.mark_dirty();
        }
    }

    const fn erase_style(&self) -> CellStyle {
        CellStyle {
            fg: Color::Default,
            bg: self.style.bg,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            blink: false,
            reverse: false,
            hidden: false,
            strikethrough: false,
        }
    }

    fn clear_cell_safely(&mut self, row: u16, col: u16) {
        let r = usize::from(row);
        let c = usize::from(col);
        let blank = Cell::blank(self.erase_style());
        let Some(line) = self.cells.get_mut(r) else {
            return;
        };
        if c >= line.len() {
            return;
        }
        if line[c].width == 0 && c > 0 {
            line[c - 1] = blank.clone();
        } else if line[c].width == 2 && c + 1 < line.len() {
            line[c + 1] = blank.clone();
        }
        line[c] = blank;
    }

    fn erase_in_line(&mut self, mode: u16) {
        let row = usize::from(self.cursor.row);
        let col = usize::from(self.cursor.col);
        let blank = Cell::blank(self.erase_style());
        let Some(line) = self.cells.get_mut(row) else {
            return;
        };
        match mode {
            0 => {
                if col < line.len() && line[col].width == 0 && col > 0 {
                    line[col - 1] = blank.clone();
                }
                for cell in line.iter_mut().skip(col) {
                    *cell = blank.clone();
                }
            }
            1 => {
                let end = (col + 1).min(line.len());
                if end < line.len() && line[end].width == 0 {
                    line[end] = blank.clone();
                }
                for cell in line.iter_mut().take(end) {
                    *cell = blank.clone();
                }
            }
            2 => {
                for cell in line.iter_mut() {
                    *cell = blank.clone();
                }
            }
            _ => return,
        }
        self.pending_wrap = false;
        self.mark_dirty();
    }

    fn erase_in_display(&mut self, mode: u16) {
        let blank = Cell::blank(self.erase_style());
        let cur_row = usize::from(self.cursor.row);
        match mode {
            0 => {
                self.erase_in_line(0);
                for line in self.cells.iter_mut().skip(cur_row + 1) {
                    for cell in line.iter_mut() {
                        *cell = blank.clone();
                    }
                }
            }
            1 => {
                for line in self.cells.iter_mut().take(cur_row) {
                    for cell in line.iter_mut() {
                        *cell = blank.clone();
                    }
                }
                self.erase_in_line(1);
            }
            2 => {
                for line in &mut self.cells {
                    for cell in line.iter_mut() {
                        *cell = blank.clone();
                    }
                }
            }
            3 => {
                self.scrollback.clear();
            }
            _ => return,
        }
        self.pending_wrap = false;
        self.mark_dirty();
    }

    fn erase_chars(&mut self, count: u16) {
        let start = self.cursor.col;
        let end = start.saturating_add(count).min(self.cols);
        for col in start..end {
            self.clear_cell_safely(self.cursor.row, col);
        }
        self.pending_wrap = false;
        self.mark_dirty();
    }

    fn insert_chars(&mut self, count: u16) {
        let row = usize::from(self.cursor.row);
        let col = usize::from(self.cursor.col);
        let blank = Cell::blank(self.erase_style());
        let Some(line) = self.cells.get_mut(row) else {
            return;
        };
        if col >= line.len() {
            return;
        }
        if line[col].width == 0 && col > 0 {
            line[col - 1] = blank.clone();
            line[col] = blank.clone();
        }
        let width = line.len();
        let n = usize::from(count).min(width - col);
        for _ in 0..n {
            line.pop();
            line.insert(col, blank.clone());
        }
        // Ensure a trailing lead cell isn't orphaned at the right edge.
        if let Some(last) = line.last_mut()
            && last.width == 2
        {
            *last = blank;
        }
        self.pending_wrap = false;
        self.mark_dirty();
    }

    fn delete_chars(&mut self, count: u16) {
        let row = usize::from(self.cursor.row);
        let col = usize::from(self.cursor.col);
        let blank = Cell::blank(self.erase_style());
        let Some(line) = self.cells.get_mut(row) else {
            return;
        };
        if col >= line.len() {
            return;
        }
        if line[col].width == 0 && col > 0 {
            line[col - 1] = blank.clone();
        }
        let width = line.len();
        let n = usize::from(count).min(width - col);
        line.drain(col..col + n);
        if col < line.len() && line[col].width == 0 {
            line[col] = blank.clone();
        }
        line.resize_with(width, || blank.clone());
        self.pending_wrap = false;
        self.mark_dirty();
    }

    fn insert_lines(&mut self, count: u16) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bottom {
            return;
        }
        let cur = usize::from(self.cursor.row);
        let bottom = usize::from(self.scroll_bottom).min(self.cells.len().saturating_sub(1));
        let n = usize::from(count).min(bottom - cur + 1);
        let blank_style = self.erase_style();
        for _ in 0..n {
            self.cells.remove(bottom);
            self.cells
                .insert(cur, vec![Cell::blank(blank_style); usize::from(self.cols)]);
        }
        self.cursor.col = 0;
        self.pending_wrap = false;
        self.mark_dirty();
    }

    fn delete_lines(&mut self, count: u16) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bottom {
            return;
        }
        let cur = usize::from(self.cursor.row);
        let bottom = usize::from(self.scroll_bottom).min(self.cells.len().saturating_sub(1));
        let n = usize::from(count).min(bottom - cur + 1);
        let blank_style = self.erase_style();
        for _ in 0..n {
            self.cells.remove(cur);
            self.cells.insert(
                bottom,
                vec![Cell::blank(blank_style); usize::from(self.cols)],
            );
        }
        self.cursor.col = 0;
        self.pending_wrap = false;
        self.mark_dirty();
    }

    fn set_cursor_position(&mut self, row_1based: u16, col_1based: u16) {
        let col = col_1based.saturating_sub(1).min(self.cols - 1);
        let row = if self.modes.origin_mode {
            self.scroll_top
                .saturating_add(row_1based.saturating_sub(1))
                .min(self.scroll_bottom)
        } else {
            row_1based.saturating_sub(1).min(self.rows - 1)
        };
        if self.cursor.col != col || self.cursor.row != row {
            self.cursor = CursorPosition { col, row };
            self.mark_dirty();
        }
        self.pending_wrap = false;
    }

    fn apply_sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.style = CellStyle::default();
            return;
        }
        let mut iter = params.iter();
        while let Some(sub) = iter.next() {
            let code = sub.first().copied().unwrap_or(0);
            match code {
                0 => self.style = CellStyle::default(),
                1 => self.style.bold = true,
                2 => self.style.dim = true,
                3 => self.style.italic = true,
                4 => self.style.underline = sub.get(1).copied().is_none_or(|v| v != 0),
                5 | 6 => self.style.blink = true,
                7 => self.style.reverse = true,
                8 => self.style.hidden = true,
                9 => self.style.strikethrough = true,
                21 => self.style.underline = true,
                22 => {
                    self.style.bold = false;
                    self.style.dim = false;
                }
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                25 => self.style.blink = false,
                27 => self.style.reverse = false,
                28 => self.style.hidden = false,
                29 => self.style.strikethrough = false,
                30..=37 => {
                    if let Ok(idx) = u8::try_from(code - 30) {
                        self.style.fg = Color::Indexed(idx);
                    }
                }
                38 => {
                    if let Some(color) = parse_extended_color(sub, &mut iter) {
                        self.style.fg = color;
                    }
                }
                39 => self.style.fg = Color::Default,
                40..=47 => {
                    if let Ok(idx) = u8::try_from(code - 40) {
                        self.style.bg = Color::Indexed(idx);
                    }
                }
                48 => {
                    if let Some(color) = parse_extended_color(sub, &mut iter) {
                        self.style.bg = color;
                    }
                }
                49 => self.style.bg = Color::Default,
                90..=97 => {
                    if let Ok(idx) = u8::try_from(code - 90 + 8) {
                        self.style.fg = Color::Indexed(idx);
                    }
                }
                100..=107 => {
                    if let Ok(idx) = u8::try_from(code - 100 + 8) {
                        self.style.bg = Color::Indexed(idx);
                    }
                }
                _ => {}
            }
        }
    }

    fn apply_dec_private_mode(&mut self, mode: u16, set: bool) {
        match mode {
            1 => self.modes.cursor_keys_application = set,
            6 => {
                self.modes.origin_mode = set;
                self.set_cursor_position(1, 1);
            }
            7 => self.modes.autowrap = set,
            25 => {
                if self.modes.cursor_visible != set {
                    self.modes.cursor_visible = set;
                    self.mark_dirty();
                }
            }
            47 | 1047 => {
                if set {
                    self.enter_alternate_screen(false);
                } else {
                    self.leave_alternate_screen(false);
                }
            }
            1049 => {
                if set {
                    self.enter_alternate_screen(true);
                } else {
                    self.leave_alternate_screen(true);
                }
            }
            1000 => {
                if set {
                    self.modes.mouse_tracking = MouseTrackingMode::Normal;
                } else if self.modes.mouse_tracking == MouseTrackingMode::Normal {
                    self.modes.mouse_tracking = MouseTrackingMode::Off;
                }
            }
            1002 => {
                if set {
                    self.modes.mouse_tracking = MouseTrackingMode::ButtonEvent;
                } else if self.modes.mouse_tracking == MouseTrackingMode::ButtonEvent {
                    self.modes.mouse_tracking = MouseTrackingMode::Off;
                }
            }
            1003 => {
                if set {
                    self.modes.mouse_tracking = MouseTrackingMode::AnyEvent;
                } else if self.modes.mouse_tracking == MouseTrackingMode::AnyEvent {
                    self.modes.mouse_tracking = MouseTrackingMode::Off;
                }
            }
            1006 => self.modes.mouse_sgr = set,
            2004 => self.modes.bracketed_paste = set,
            2026 => {
                let was_active = self.modes.sync_output_active;
                self.modes.sync_output_active = set;
                if was_active && !set && self.dirty_during_sync {
                    self.dirty_during_sync = false;
                    self.generation = self.generation.wrapping_add(1);
                }
            }
            _ => {}
        }
    }

    fn apply_kitty_keyboard(&mut self, intermediates: &[u8], params: &Params) {
        let mut iter = params.iter();
        let first = iter.next().and_then(|s| s.first().copied());
        let second = iter.next().and_then(|s| s.first().copied());
        match intermediates {
            b">" => {
                let flags = u8::try_from(first.unwrap_or(0) & 0x1F).unwrap_or(0);
                let stack = self.active_kitty_stack_mut();
                if stack.len() >= MAX_KITTY_STACK_DEPTH {
                    stack.remove(0);
                }
                stack.push(flags);
                self.sync_kitty_flags();
            }
            b"<" => {
                let count = usize::from(first.unwrap_or(1).max(1));
                let stack = self.active_kitty_stack_mut();
                let keep = stack.len().saturating_sub(count);
                stack.truncate(keep);
                self.sync_kitty_flags();
            }
            b"=" => {
                let flags = u8::try_from(first.unwrap_or(0) & 0x1F).unwrap_or(0);
                let mode = second.unwrap_or(1);
                let stack = self.active_kitty_stack_mut();
                if stack.is_empty() {
                    stack.push(0);
                }
                if let Some(top) = stack.last_mut() {
                    match mode {
                        1 => *top = flags,
                        2 => *top |= flags,
                        3 => *top &= !flags,
                        _ => {}
                    }
                }
                self.sync_kitty_flags();
            }
            _ => {}
        }
    }
}

impl Perform for TerminalState {
    fn print(&mut self, c: char) {
        let Some(raw_width) = c.width() else {
            return;
        };
        if raw_width == 0 {
            // Combining mark: attach to the previous non-continuation cell.
            let target_col = if self.pending_wrap {
                Some(self.cursor.col)
            } else {
                self.cursor.col.checked_sub(1)
            };
            if let Some(mut col) = target_col
                && let Some(line) = self.cells.get_mut(usize::from(self.cursor.row))
            {
                if usize::from(col) < line.len() && line[usize::from(col)].width == 0 && col > 0 {
                    col -= 1;
                }
                if let Some(cell) = line.get_mut(usize::from(col)) {
                    cell.combining.push(c);
                    self.mark_dirty();
                }
            }
            return;
        }

        let mut cell_width = if raw_width >= 2 { 2_u8 } else { 1_u8 };
        if self.pending_wrap && self.modes.autowrap {
            self.cursor.col = 0;
            self.linefeed();
        }
        if cell_width == 2 && self.cursor.col + 1 >= self.cols {
            if self.modes.autowrap && self.cols >= 2 {
                self.clear_cell_safely(self.cursor.row, self.cursor.col);
                self.cursor.col = 0;
                self.linefeed();
            } else {
                cell_width = 1;
            }
        }

        let row = self.cursor.row;
        let col = self.cursor.col;
        self.clear_cell_safely(row, col);
        if cell_width == 2 {
            self.clear_cell_safely(row, col + 1);
        }

        if let Some(line) = self.cells.get_mut(usize::from(row)) {
            if let Some(slot) = line.get_mut(usize::from(col)) {
                *slot = Cell {
                    ch: c,
                    combining: Vec::new(),
                    width: cell_width,
                    style: self.style,
                };
            }
            if cell_width == 2
                && let Some(cont) = line.get_mut(usize::from(col + 1))
            {
                *cont = Cell::continuation(self.style);
            }
        }

        let next_col = col.saturating_add(u16::from(cell_width));
        if next_col >= self.cols {
            self.cursor.col = self.cols - 1;
            self.pending_wrap = self.modes.autowrap;
        } else {
            self.cursor.col = next_col;
            self.pending_wrap = false;
        }
        self.mark_dirty();
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => {
                self.pending_wrap = false;
                if self.cursor.col > 0 {
                    self.cursor.col -= 1;
                    self.mark_dirty();
                }
            }
            0x09 => {
                self.pending_wrap = false;
                let next = ((self.cursor.col / 8) + 1) * 8;
                let clamped = next.min(self.cols - 1);
                if clamped != self.cursor.col {
                    self.cursor.col = clamped;
                    self.mark_dirty();
                }
            }
            0x0A..=0x0C => self.linefeed(),
            0x0D => {
                self.pending_wrap = false;
                if self.cursor.col != 0 {
                    self.cursor.col = 0;
                    self.mark_dirty();
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => self.linefeed(),
            b'E' => {
                self.cursor.col = 0;
                self.linefeed();
            }
            b'M' => self.reverse_index(),
            b'c' => self.reset(),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        if intermediates == b"?" {
            if matches!(action, 'h' | 'l') {
                let set = action == 'h';
                for sub in params {
                    if let Some(&mode) = sub.first() {
                        self.apply_dec_private_mode(mode, set);
                    }
                }
            }
            return;
        }
        if action == 'u' && matches!(intermediates, b">" | b"<" | b"=") {
            self.apply_kitty_keyboard(intermediates, params);
            return;
        }
        if !intermediates.is_empty() {
            return;
        }

        let p = |index: usize, default: u16| -> u16 {
            params
                .iter()
                .nth(index)
                .and_then(|s| s.first().copied())
                .filter(|&v| v != 0)
                .unwrap_or(default)
        };
        let p_zero = |index: usize| -> u16 {
            params
                .iter()
                .nth(index)
                .and_then(|s| s.first().copied())
                .unwrap_or(0)
        };

        match action {
            'A' => {
                let n = p(0, 1);
                let top = if self.cursor.row >= self.scroll_top {
                    self.scroll_top
                } else {
                    0
                };
                let target = self.cursor.row.saturating_sub(n).max(top);
                self.pending_wrap = false;
                if target != self.cursor.row {
                    self.cursor.row = target;
                    self.mark_dirty();
                }
            }
            'B' => {
                let n = p(0, 1);
                let bottom = if self.cursor.row <= self.scroll_bottom {
                    self.scroll_bottom
                } else {
                    self.rows - 1
                };
                let target = self.cursor.row.saturating_add(n).min(bottom);
                self.pending_wrap = false;
                if target != self.cursor.row {
                    self.cursor.row = target;
                    self.mark_dirty();
                }
            }
            'C' => {
                let n = p(0, 1);
                let target = self.cursor.col.saturating_add(n).min(self.cols - 1);
                self.pending_wrap = false;
                if target != self.cursor.col {
                    self.cursor.col = target;
                    self.mark_dirty();
                }
            }
            'D' => {
                let n = p(0, 1);
                let target = self.cursor.col.saturating_sub(n);
                self.pending_wrap = false;
                if target != self.cursor.col {
                    self.cursor.col = target;
                    self.mark_dirty();
                }
            }
            'E' => {
                let n = p(0, 1);
                let target = self.cursor.row.saturating_add(n).min(self.rows - 1);
                self.pending_wrap = false;
                if target != self.cursor.row || self.cursor.col != 0 {
                    self.cursor.row = target;
                    self.cursor.col = 0;
                    self.mark_dirty();
                }
            }
            'F' => {
                let n = p(0, 1);
                let target = self.cursor.row.saturating_sub(n);
                self.pending_wrap = false;
                if target != self.cursor.row || self.cursor.col != 0 {
                    self.cursor.row = target;
                    self.cursor.col = 0;
                    self.mark_dirty();
                }
            }
            'G' | '`' => {
                let col = p(0, 1).saturating_sub(1).min(self.cols - 1);
                self.pending_wrap = false;
                if col != self.cursor.col {
                    self.cursor.col = col;
                    self.mark_dirty();
                }
            }
            'd' => {
                let row_1based = p(0, 1);
                let row = if self.modes.origin_mode {
                    self.scroll_top
                        .saturating_add(row_1based.saturating_sub(1))
                        .min(self.scroll_bottom)
                } else {
                    row_1based.saturating_sub(1).min(self.rows - 1)
                };
                self.pending_wrap = false;
                if row != self.cursor.row {
                    self.cursor.row = row;
                    self.mark_dirty();
                }
            }
            'H' | 'f' => {
                self.set_cursor_position(p(0, 1), p(1, 1));
            }
            'J' => self.erase_in_display(p_zero(0)),
            'K' => self.erase_in_line(p_zero(0)),
            'X' => self.erase_chars(p(0, 1)),
            '@' => self.insert_chars(p(0, 1)),
            'P' => self.delete_chars(p(0, 1)),
            'L' => self.insert_lines(p(0, 1)),
            'M' => self.delete_lines(p(0, 1)),
            'S' => self.scroll_up(p(0, 1)),
            'T' => self.scroll_down(p(0, 1)),
            'r' => {
                let top = p(0, 1).saturating_sub(1).min(self.rows - 1);
                let bottom = p(1, self.rows).saturating_sub(1).min(self.rows - 1);
                if top < bottom {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                } else {
                    self.scroll_top = 0;
                    self.scroll_bottom = self.rows - 1;
                }
                self.set_cursor_position(1, 1);
            }
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            'm' => self.apply_sgr(params),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some(&code) = params.first() else {
            return;
        };
        if (code == b"0" || code == b"2")
            && let Some(&raw_title) = params.get(1)
        {
            let title = String::from_utf8_lossy(raw_title).into_owned();
            if self.title != title {
                self.title = title;
                self.mark_dirty();
            }
        }
    }
}

fn parse_extended_color<'a>(
    current: &[u16],
    iter: &mut impl Iterator<Item = &'a [u16]>,
) -> Option<Color> {
    // Colon-separated subparameter form: `38:5:idx` or `38:2[:cs]:r:g:b`.
    if current.len() >= 2 {
        match current[1] {
            5 => {
                let idx = *current.get(2)?;
                return u8::try_from(idx).ok().map(Color::Indexed);
            }
            2 => {
                let (r, g, b) = if current.len() >= 6 {
                    // ITU T.416 includes an optional color-space id at index 2.
                    (current[3], current[4], current[5])
                } else if current.len() >= 5 {
                    (current[2], current[3], current[4])
                } else {
                    return None;
                };
                return Some(Color::Rgb(
                    u8::try_from(r.min(255)).unwrap_or(255),
                    u8::try_from(g.min(255)).unwrap_or(255),
                    u8::try_from(b.min(255)).unwrap_or(255),
                ));
            }
            _ => return None,
        }
    }

    // Semicolon-separated form: `38;5;idx` or `38;2;r;g;b`.
    let mode = iter.next()?.first().copied()?;
    match mode {
        5 => {
            let idx = iter.next()?.first().copied()?;
            u8::try_from(idx).ok().map(Color::Indexed)
        }
        2 => {
            let r = iter.next()?.first().copied()?;
            let g = iter.next()?.first().copied()?;
            let b = iter.next()?.first().copied()?;
            Some(Color::Rgb(
                u8::try_from(r.min(255)).unwrap_or(255),
                u8::try_from(g.min(255)).unwrap_or(255),
                u8::try_from(b.min(255)).unwrap_or(255),
            ))
        }
        _ => None,
    }
}

fn blank_grid(cols: u16, rows: u16, style: CellStyle) -> Vec<Vec<Cell>> {
    vec![vec![Cell::blank(style); usize::from(cols)]; usize::from(rows)]
}

fn resize_grid(grid: &mut Vec<Vec<Cell>>, cols: u16, rows: u16) {
    let target_cols = usize::from(cols);
    let target_rows = usize::from(rows);
    let blank = Cell::blank(CellStyle::default());
    grid.truncate(target_rows);
    for line in grid.iter_mut() {
        line.truncate(target_cols);
        if let Some(last) = line.last_mut()
            && last.width == 2
        {
            *last = blank.clone();
        }
        line.resize_with(target_cols, || blank.clone());
    }
    while grid.len() < target_rows {
        grid.push(vec![blank.clone(); target_cols]);
    }
}

/// Per-job virtual terminal state machine backed by `vte`.
pub struct TerminalEmulator {
    parser: Parser,
    state: TerminalState,
}

impl std::fmt::Debug for TerminalEmulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalEmulator")
            .field("cols", &self.state.cols)
            .field("rows", &self.state.rows)
            .field("cursor", &self.state.cursor)
            .field("modes", &self.state.modes)
            .field("title", &self.state.title)
            .field("generation", &self.state.generation)
            .finish_non_exhaustive()
    }
}

impl Default for TerminalEmulator {
    fn default() -> Self {
        Self::new(DEFAULT_COLS, DEFAULT_ROWS)
    }
}

impl TerminalEmulator {
    /// Creates an emulator with `cols x rows` dimensions and [`DEFAULT_SCROLLBACK_LIMIT`].
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_scrollback(cols, rows, DEFAULT_SCROLLBACK_LIMIT)
    }

    /// Creates an emulator with explicit `scrollback_limit` for the primary screen.
    #[must_use]
    pub fn with_scrollback(cols: u16, rows: u16, scrollback_limit: usize) -> Self {
        Self {
            parser: Parser::new(),
            state: TerminalState::new(cols, rows, scrollback_limit),
        }
    }

    /// Feeds raw PTY output bytes through the VT parser into the screen state.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.state, bytes);
    }

    /// Resizes the virtual terminal grid, clamping cursor and resetting scroll margins.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.state.resize(cols, rows);
    }

    /// Current terminal width in cells.
    #[must_use]
    pub const fn cols(&self) -> u16 {
        self.state.cols
    }

    /// Current terminal height in cells.
    #[must_use]
    pub const fn rows(&self) -> u16 {
        self.state.rows
    }

    /// Current 0-based cursor position.
    #[must_use]
    pub const fn cursor(&self) -> CursorPosition {
        self.state.cursor
    }

    /// Current tracked terminal modes.
    #[must_use]
    pub const fn modes(&self) -> TerminalModes {
        self.state.modes
    }

    /// Current window title set via OSC 0 / OSC 2.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.state.title
    }

    /// Current visible state generation counter.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.state.generation
    }

    /// Number of primary-screen lines currently stored in scrollback.
    #[must_use]
    pub fn scrollback_len(&self) -> usize {
        self.state.scrollback.len()
    }

    /// Captures an immutable [`ScreenSnapshot`] of the visible screen.
    #[must_use]
    pub fn snapshot(&self) -> ScreenSnapshot {
        ScreenSnapshot {
            cols: self.state.cols,
            rows: self.state.rows,
            cells: self.state.cells.clone(),
            cursor: self.state.cursor,
            modes: self.state.modes,
            title: self.state.title.clone(),
            generation: self.state.generation,
        }
    }

    /// Renders the visible screen as clean trimmed text.
    #[must_use]
    pub fn plain_text(&self) -> String {
        let mut lines: Vec<String> = self
            .state
            .cells
            .iter()
            .map(|row| {
                let mut line = String::with_capacity(row.len());
                for cell in row {
                    cell.push_text(&mut line);
                }
                line.trim_end().to_owned()
            })
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }
}

/// Result of waiting for a terminal screen to reach frame quiescence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleOutcome {
    /// Output became quiet outside synchronized output (`?2026`).
    Settled {
        /// Latest screen generation observed.
        generation: u64,
    },
    /// The underlying PTY stream closed (EOF / process exited).
    Closed {
        /// Final screen generation observed before close.
        generation: u64,
    },
    /// The bounded maximum timeout elapsed before quiescence.
    TimedOut {
        /// Latest screen generation observed at timeout.
        generation: u64,
        /// Whether synchronized output (`?2026`) was still open at timeout.
        sync_output_active: bool,
    },
}

#[derive(Debug)]
struct FrameProgress {
    generation: u64,
    sync_output_active: bool,
    last_activity: Instant,
    closed: bool,
}

#[derive(Debug)]
struct FrameWaiterInner {
    progress: Mutex<FrameProgress>,
    notify: Event,
}

/// Runtime-agnostic async frame-settle helper paired with a [`TerminalEmulator`].
///
/// The PTY reader task calls [`record_feed`](Self::record_feed) after feeding
/// bytes into the emulator and [`mark_closed`](Self::mark_closed) on EOF; tool
/// actions call [`wait_for_settle`](Self::wait_for_settle) or
/// [`wait_for_action_settle`](Self::wait_for_action_settle) so reads never
/// observe mid-frame torn state during `?2026` synchronized updates or active
/// redraw bursts.
#[derive(Debug, Clone)]
pub struct FrameWaiter {
    inner: Arc<FrameWaiterInner>,
}

impl Default for FrameWaiter {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameWaiter {
    /// Creates a new waiter starting at generation `0`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(FrameWaiterInner {
                progress: Mutex::new(FrameProgress {
                    generation: 0,
                    sync_output_active: false,
                    last_activity: Instant::now(),
                    closed: false,
                }),
                notify: Event::new(),
            }),
        }
    }

    /// Records that PTY output was fed into the emulator.
    pub fn record_feed(&self, generation: u64, sync_output_active: bool) {
        if let Ok(mut guard) = self.inner.progress.lock() {
            guard.generation = generation;
            guard.sync_output_active = sync_output_active;
            guard.last_activity = Instant::now();
        }
        self.inner.notify.notify(usize::MAX);
    }

    /// Marks the PTY stream closed, waking all waiters immediately.
    pub fn mark_closed(&self) {
        if let Ok(mut guard) = self.inner.progress.lock() {
            guard.closed = true;
        }
        self.inner.notify.notify(usize::MAX);
    }

    /// Waits until synchronized output (`?2026`) is inactive and no new PTY
    /// output has arrived for `quiet_window`, bounded by `max_timeout`.
    pub async fn wait_for_settle(
        &self,
        quiet_window: Duration,
        max_timeout: Duration,
    ) -> SettleOutcome {
        self.wait_from_anchor(None, quiet_window, max_timeout).await
    }

    /// Waits for a screen update triggered by an action (such as `open`, `key`,
    /// `type`, `click`, or `resize`) to settle.
    ///
    /// Anchors the initial quiet window at the moment of the call so an already
    /// idle terminal still gives the child process `quiet_window` to emit its
    /// redraw before returning, while still waking immediately on stream close.
    pub async fn wait_for_action_settle(
        &self,
        quiet_window: Duration,
        max_timeout: Duration,
    ) -> SettleOutcome {
        self.wait_from_anchor(Some(Instant::now()), quiet_window, max_timeout)
            .await
    }

    async fn wait_from_anchor(
        &self,
        anchor: Option<Instant>,
        quiet_window: Duration,
        max_timeout: Duration,
    ) -> SettleOutcome {
        let start = anchor.unwrap_or_else(Instant::now);
        let deadline = start + max_timeout;

        loop {
            let (listener, wait_until) = {
                let Ok(guard) = self.inner.progress.lock() else {
                    return SettleOutcome::Closed { generation: 0 };
                };
                if guard.closed {
                    return SettleOutcome::Closed {
                        generation: guard.generation,
                    };
                }
                let now = Instant::now();
                if now >= deadline {
                    return SettleOutcome::TimedOut {
                        generation: guard.generation,
                        sync_output_active: guard.sync_output_active,
                    };
                }
                let effective_activity = anchor.map_or(guard.last_activity, |a| {
                    if guard.last_activity > a {
                        guard.last_activity
                    } else {
                        a
                    }
                });
                if guard.sync_output_active {
                    let listener = self.inner.notify.listen();
                    (listener, deadline)
                } else {
                    let quiet_until = effective_activity + quiet_window;
                    if now >= quiet_until {
                        return SettleOutcome::Settled {
                            generation: guard.generation,
                        };
                    }
                    let listener = self.inner.notify.listen();
                    (listener, quiet_until.min(deadline))
                }
            };

            let () = race(
                async {
                    listener.await;
                },
                async {
                    Timer::at(wait_until).await;
                },
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_modes_sgr_wide_cjk_and_alternate_screen() {
        let mut emu = TerminalEmulator::new(40, 10);
        // Set title, DECCKM, bracketed paste, SGR mouse, Kitty flags, and color text.
        emu.feed(b"\x1b]2;Demo TUI\x07");
        emu.feed(b"\x1b[?1h\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[>5u");
        emu.feed("\x1b[1;38;2;255;128;64;48;5;236m中文 日本語 한국어\x1b[0m".as_bytes());

        let snap = emu.snapshot();
        assert_eq!(snap.title, "Demo TUI");
        assert!(snap.modes.cursor_keys_application);
        assert!(snap.modes.bracketed_paste);
        assert_eq!(snap.modes.mouse_tracking, MouseTrackingMode::ButtonEvent);
        assert!(snap.modes.mouse_sgr);
        assert_eq!(snap.modes.kitty_keyboard_flags, 5);
        assert_eq!(snap.plain_text(), "中文 日本語 한국어");

        // Verify wide CJK cell geometry: '中' occupies cols 0..2, '文' cols 2..4.
        let c0 = snap.cell(0, 0).expect("cell 0");
        assert_eq!(c0.ch, '中');
        assert_eq!(c0.width, 2);
        assert!(c0.style.bold);
        assert_eq!(c0.style.fg, Color::Rgb(255, 128, 64));
        assert_eq!(c0.style.bg, Color::Indexed(236));
        assert!(snap.cell(1, 0).expect("cont 1").is_continuation());
        assert_eq!(snap.cell(2, 0).expect("cell 2").ch, '文');

        // Enter alternate screen (?1049h) with independent Kitty stack and restore on exit.
        emu.feed(b"\x1b[?1049h\x1b[>1u\x1b[?25lAlt Screen");
        assert!(emu.modes().alternate_screen);
        assert!(!emu.modes().cursor_visible);
        assert_eq!(emu.modes().kitty_keyboard_flags, 1);
        assert_eq!(emu.plain_text(), "Alt Screen");

        emu.feed(b"\x1b[?1049l\x1b[?25h");
        assert!(!emu.modes().alternate_screen);
        assert!(emu.modes().cursor_visible);
        assert_eq!(emu.modes().kitty_keyboard_flags, 5);
        assert_eq!(emu.plain_text(), "中文 日本語 한국어");

        // Kitty pop restores 0.
        emu.feed(b"\x1b[<1u");
        assert_eq!(emu.modes().kitty_keyboard_flags, 0);
    }

    #[test]
    fn handles_scroll_margins_editing_and_bounded_scrollback() {
        let mut emu = TerminalEmulator::with_scrollback(20, 4, 3);
        for i in 0..8 {
            emu.feed(format!("line-{i}\r\n").as_bytes());
        }
        assert_eq!(emu.scrollback_len(), 3);

        // DECSTBM scroll region rows 2..3 (1-based).
        emu.feed(b"\x1b[2J\x1b[1;1Hheader\x1b[2;1HrowA\x1b[3;1HrowB\x1b[4;1Hfooter");
        emu.feed(b"\x1b[2;3r\x1b[3;1H\nnext");
        assert_eq!(emu.plain_text(), "header\nrowB\nnext\nfooter");

        // ICH / DCH / ECH / IL / DL.
        emu.feed(b"\x1b[r\x1b[2J\x1b[1;1Habcdef\x1b[1;3H\x1b[2P");
        assert_eq!(emu.plain_text(), "abef");
        emu.feed(b"\x1b[1;3H\x1b[2@XY");
        assert_eq!(emu.plain_text(), "abXYef");
        emu.feed(b"\x1b[1;2H\x1b[2X");
        assert_eq!(emu.plain_text(), "a  Yef");
    }

    #[tokio::test]
    async fn frame_waiter_honors_sync_output_quiescence_and_immediate_close() {
        let mut emu = TerminalEmulator::new(40, 10);
        let waiter = FrameWaiter::new();

        // Begin ?2026 synchronized output; waiter must NOT settle while active.
        emu.feed(b"\x1b[?2026hPartial ");
        waiter.record_feed(emu.generation(), emu.modes().sync_output_active);
        assert!(emu.modes().sync_output_active);

        let waiter_bg = waiter.clone();
        let gen_after_close = {
            let mut emu_after = TerminalEmulator::new(40, 10);
            emu_after.feed(b"\x1b[?2026hPartial Frame\x1b[?2026l");
            emu_after.generation()
        };
        let handle = tokio::spawn(async move {
            Timer::after(Duration::from_millis(45)).await;
            waiter_bg.record_feed(gen_after_close, false);
        });

        let started = Instant::now();
        let outcome = waiter
            .wait_for_settle(Duration::from_millis(30), Duration::from_millis(500))
            .await;
        handle.await.expect("feeder completes");
        assert_eq!(
            outcome,
            SettleOutcome::Settled {
                generation: gen_after_close
            }
        );
        assert!(
            started.elapsed() >= Duration::from_millis(70),
            "should wait for ?2026l plus quiet window, elapsed={:?}",
            started.elapsed()
        );

        // Stream EOF/close wakes immediately even if sync_output_active is true.
        waiter.record_feed(gen_after_close + 1, true);
        let waiter_close = waiter.clone();
        let close_task = tokio::spawn(async move {
            Timer::after(Duration::from_millis(15)).await;
            waiter_close.mark_closed();
        });
        let close_start = Instant::now();
        let closed = waiter
            .wait_for_settle(Duration::from_millis(200), Duration::from_secs(2))
            .await;
        close_task.await.expect("closer completes");
        assert_eq!(
            closed,
            SettleOutcome::Closed {
                generation: gen_after_close + 1
            }
        );
        assert!(
            close_start.elapsed() < Duration::from_millis(300),
            "close should wake immediately, elapsed={:?}",
            close_start.elapsed()
        );
    }
}
