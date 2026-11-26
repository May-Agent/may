//! Structured TUI screen observation: box-drawing pane detection, styled
//! selection spans, transient `<gen>:<idx>` references, and unique substring
//! text targeting.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use askama::Template;
use thiserror::Error;

use super::emulator::{Cell, Color, ScreenSnapshot};

/// Maximum characters shown for a quoted element or pane label in `tui observe`.
const MAX_LABEL_CHARS: usize = 80;

/// Transient element reference scoped to a single screen generation (`<gen>:<idx>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TuiElementRef {
    /// Screen generation at which this reference was observed.
    pub generation: u64,
    /// 1-based element index within that generation's listing.
    pub index: u32,
}

impl fmt::Display for TuiElementRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.generation, self.index)
    }
}

/// Failure parsing a `<gen>:<idx>` element reference string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`{input}` is not a `<gen>:<idx>` element reference")]
pub struct ParseElementRefError {
    /// Raw text that failed to parse.
    pub input: String,
}

impl FromStr for TuiElementRef {
    type Err = ParseElementRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((gen_part, idx_part)) = s.split_once(':') else {
            return Err(ParseElementRefError {
                input: s.to_owned(),
            });
        };
        if gen_part.is_empty()
            || idx_part.is_empty()
            || !gen_part.bytes().all(|b| b.is_ascii_digit())
            || !idx_part.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(ParseElementRefError {
                input: s.to_owned(),
            });
        }
        let Ok(generation) = gen_part.parse::<u64>() else {
            return Err(ParseElementRefError {
                input: s.to_owned(),
            });
        };
        let Ok(index) = idx_part.parse::<u32>() else {
            return Err(ParseElementRefError {
                input: s.to_owned(),
            });
        };
        if index == 0 {
            return Err(ParseElementRefError {
                input: s.to_owned(),
            });
        }
        Ok(Self { generation, index })
    }
}

/// Axis-aligned rectangle in 0-based terminal cell coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellRect {
    /// Leftmost 0-based column.
    pub col: u16,
    /// Topmost 0-based row.
    pub row: u16,
    /// Width in cells.
    pub width: u16,
    /// Height in cells.
    pub height: u16,
}

impl CellRect {
    /// Returns the 0-based `(col, row)` center cell of this rectangle.
    #[must_use]
    pub const fn center(self) -> (u16, u16) {
        (
            self.col.saturating_add(self.width / 2),
            self.row.saturating_add(self.height / 2),
        )
    }
}

/// Classification of an observed TUI element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementKind {
    /// Box-drawing rectangular pane.
    Pane,
    /// Reverse-video or distinct-background selection span.
    Selection,
    /// Underlined or distinctively styled interactive highlight.
    Highlight,
}

impl ElementKind {
    /// Natural-language label rendered in `tui observe`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pane => "pane",
            Self::Selection => "selection",
            Self::Highlight => "highlight",
        }
    }
}

/// Detected box-drawing rectangular pane on a terminal screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedPane {
    /// Transient `<gen>:<idx>` reference for this pane.
    pub reference: TuiElementRef,
    /// Optional title extracted from the top border line.
    pub title: Option<String>,
    /// Outer border bounds in 0-based cells.
    pub bounds: CellRect,
    /// Center cell `(col, row)`.
    pub center: (u16, u16),
}

/// Observed interactive or structural element with its transient reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedElement {
    /// Transient `<gen>:<idx>` reference.
    pub reference: TuiElementRef,
    /// Classification of the element.
    pub kind: ElementKind,
    /// Human-readable label (pane title or span text).
    pub label: String,
    /// Bounding rectangle in 0-based cells.
    pub bounds: CellRect,
    /// Clickable center coordinate `(col, row)`.
    pub center: (u16, u16),
}

/// Table of transient `<gen>:<idx>` element references valid only for one generation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ElementTable {
    generation: u64,
    elements: Vec<ObservedElement>,
}

impl ElementTable {
    /// Generation to which every entry in this table belongs.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// All observed elements in index order.
    #[must_use]
    pub fn elements(&self) -> &[ObservedElement] {
        &self.elements
    }

    /// Resolves `reference` against the table and the terminal's `current_generation`.
    ///
    /// # Errors
    /// Returns [`RefResolveError::Stale`] if the terminal's generation has
    /// advanced past `reference.generation`, or [`RefResolveError::Unknown`] if
    /// no element with that index exists in this table.
    pub fn resolve(
        &self,
        reference: TuiElementRef,
        current_generation: u64,
    ) -> Result<&ObservedElement, RefResolveError> {
        if reference.generation != current_generation || reference.generation != self.generation {
            return Err(RefResolveError::Stale {
                reference,
                current_generation,
            });
        }
        self.elements
            .iter()
            .find(|el| el.reference == reference)
            .ok_or(RefResolveError::Unknown {
                reference,
                current_generation,
            })
    }
}

/// Failure resolving a `<gen>:<idx>` element reference.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RefResolveError {
    /// The screen has changed since the reference was issued.
    #[error("element {reference} is stale (screen is now at generation {current_generation})")]
    Stale {
        /// Reference supplied by the caller.
        reference: TuiElementRef,
        /// Current live screen generation.
        current_generation: u64,
    },
    /// Reference matched the current generation but its index does not exist.
    #[error("element {reference} does not exist in generation {current_generation}")]
    Unknown {
        /// Reference supplied by the caller.
        reference: TuiElementRef,
        /// Current live screen generation.
        current_generation: u64,
    },
}

#[derive(Template)]
#[template(path = "tui_stale_ref.txt", escape = "none")]
struct StaleRefTemplate<'a> {
    reason: &'a str,
}

impl RefResolveError {
    /// Renders a model-facing refusal message via the Askama template.
    #[must_use]
    pub fn render_for_model(&self) -> String {
        let reason = self.to_string();
        StaleRefTemplate { reason: &reason }
            .render()
            .unwrap_or(reason)
    }
}

/// Failure locating a unique visible text target on the screen.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TextTargetError {
    /// Empty search text.
    #[error("text target cannot be empty")]
    EmptyNeedle,
    /// Text does not appear anywhere on the visible screen.
    #[error("text \"{needle}\" was not found on the current screen")]
    NotFound {
        /// Substring searched for.
        needle: String,
    },
    /// Text appears multiple times on the visible screen.
    #[error(
        "text \"{needle}\" is ambiguous ({count} matches on screen); pass a `<gen>:<idx>` reference or explicit `COL ROW` coordinates"
    )]
    Ambiguous {
        /// Substring searched for.
        needle: String,
        /// Number of occurrences found.
        count: usize,
        /// Center `(col, row)` coordinates of each match.
        positions: Vec<(u16, u16)>,
    },
}

#[derive(Template)]
#[template(path = "tui_text_target_error.txt", escape = "none")]
struct TextTargetErrorTemplate<'a> {
    reason: &'a str,
}

impl TextTargetError {
    /// Renders a model-facing refusal message via the Askama template.
    #[must_use]
    pub fn render_for_model(&self) -> String {
        let reason = self.to_string();
        TextTargetErrorTemplate { reason: &reason }
            .render()
            .unwrap_or(reason)
    }
}

/// Complete structured observation of a [`ScreenSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// Screen width in cells.
    pub cols: u16,
    /// Screen height in cells.
    pub rows: u16,
    /// Screen generation counter.
    pub generation: u64,
    /// Optional window title (OSC 0/2).
    pub title: Option<String>,
    /// Whether the alternate screen buffer is active.
    pub alternate_screen: bool,
    /// Cursor column (0-based).
    pub cursor_col: u16,
    /// Cursor row (0-based).
    pub cursor_row: u16,
    /// Cursor visibility flag.
    pub cursor_visible: bool,
    /// Non-default active terminal mode labels.
    pub active_modes: Vec<String>,
    /// Detected rectangular box-drawing panes.
    pub panes: Vec<ObservedPane>,
    /// Detected selection and highlighted elements (excluding panes).
    pub highlights: Vec<ObservedElement>,
    /// Lookup table holding all transient `<gen>:<idx>` references (panes + highlights).
    pub table: ElementTable,
}

/// Analyzes `snapshot` for box-drawing panes and styled selection/highlight
/// regions, issuing transient `<gen>:<idx>` references in [`Observation::table`].
#[must_use]
pub fn observe_screen(snapshot: &ScreenSnapshot) -> Observation {
    let raw_panes = detect_panes(snapshot);
    let raw_spans = detect_styled_spans(snapshot);

    let mut next_idx: u32 = 1;
    let mut panes = Vec::with_capacity(raw_panes.len());
    let mut all_elements = Vec::with_capacity(raw_panes.len() + raw_spans.len());

    for (bounds, title) in raw_panes {
        let reference = TuiElementRef {
            generation: snapshot.generation,
            index: next_idx,
        };
        next_idx = next_idx.saturating_add(1);
        let center = bounds.center();
        let label = title.clone().unwrap_or_default();
        panes.push(ObservedPane {
            reference,
            title,
            bounds,
            center,
        });
        all_elements.push(ObservedElement {
            reference,
            kind: ElementKind::Pane,
            label,
            bounds,
            center,
        });
    }

    let mut highlights = Vec::with_capacity(raw_spans.len());
    for (kind, bounds, label) in raw_spans {
        let reference = TuiElementRef {
            generation: snapshot.generation,
            index: next_idx,
        };
        next_idx = next_idx.saturating_add(1);
        let center = bounds.center();
        let el = ObservedElement {
            reference,
            kind,
            label,
            bounds,
            center,
        };
        highlights.push(el.clone());
        all_elements.push(el);
    }

    let title = (!snapshot.title.is_empty()).then(|| snapshot.title.clone());
    Observation {
        cols: snapshot.cols,
        rows: snapshot.rows,
        generation: snapshot.generation,
        title,
        alternate_screen: snapshot.modes.alternate_screen,
        cursor_col: snapshot.cursor.col,
        cursor_row: snapshot.cursor.row,
        cursor_visible: snapshot.modes.cursor_visible,
        active_modes: snapshot.modes.active_labels(),
        panes,
        highlights,
        table: ElementTable {
            generation: snapshot.generation,
            elements: all_elements,
        },
    }
}

/// Finds an exact unique substring on the visible screen and returns its
/// 0-based `(col, row)` cell center, accounting for double-width CJK cells.
///
/// # Errors
/// Returns [`TextTargetError::EmptyNeedle`], [`TextTargetError::NotFound`], or
/// [`TextTargetError::Ambiguous`].
pub fn find_unique_text(
    snapshot: &ScreenSnapshot,
    needle: &str,
) -> Result<(u16, u16), TextTargetError> {
    if needle.is_empty() {
        return Err(TextTargetError::EmptyNeedle);
    }
    let mut matches: Vec<(u16, u16)> = Vec::new();

    for (row_idx, row_cells) in snapshot.cells.iter().enumerate() {
        let Ok(row_u16) = u16::try_from(row_idx) else {
            break;
        };
        let mut row_text = String::with_capacity(row_cells.len());
        // Maps each character's byte range `[byte_start, byte_end)` to its
        // `[col_start, col_end)` cell span.
        let mut spans: Vec<(usize, usize, u16, u16)> = Vec::with_capacity(row_cells.len());

        for (col_idx, cell) in row_cells.iter().enumerate() {
            if cell.is_continuation() {
                continue;
            }
            let Ok(col_u16) = u16::try_from(col_idx) else {
                break;
            };
            let byte_start = row_text.len();
            cell.push_text(&mut row_text);
            let byte_end = row_text.len();
            let cell_span = u16::from(cell.width.max(1));
            spans.push((
                byte_start,
                byte_end,
                col_u16,
                col_u16.saturating_add(cell_span),
            ));
        }

        for (match_start, matched) in row_text.match_indices(needle) {
            let match_end = match_start + matched.len();
            let start_col = spans
                .iter()
                .find(|&&(_, b_end, _, _)| b_end > match_start)
                .map(|&(_, _, c_start, _)| c_start);
            let end_col = spans
                .iter()
                .rev()
                .find(|&&(b_start, _, _, _)| b_start < match_end)
                .map(|&(_, _, _, c_end)| c_end);
            if let (Some(c0), Some(c1)) = (start_col, end_col) {
                let width = c1.saturating_sub(c0).max(1);
                let center_col = c0.saturating_add((width - 1) / 2);
                matches.push((center_col, row_u16));
            }
        }
    }

    match matches.len() {
        0 => Err(TextTargetError::NotFound {
            needle: needle.to_owned(),
        }),
        1 => Ok(matches[0]),
        count => Err(TextTargetError::Ambiguous {
            needle: needle.to_owned(),
            count,
            positions: matches,
        }),
    }
}

struct Quoted<'a>(&'a str);

impl fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (shown, ellipsis) = match self.0.char_indices().nth(MAX_LABEL_CHARS) {
            Some((byte_idx, _)) => (self.0.split_at(byte_idx).0, "…"),
            None => (self.0, ""),
        };
        write!(f, "\"{}{ellipsis}\"", shown.escape_debug())
    }
}

struct PaneView<'a> {
    reference: TuiElementRef,
    title: Option<Quoted<'a>>,
    bounds: CellRect,
    center: (u16, u16),
}

struct ElementView<'a> {
    reference: TuiElementRef,
    kind: &'static str,
    label: Quoted<'a>,
    center: (u16, u16),
}

#[derive(Template)]
#[template(path = "tui_observe.txt", escape = "none")]
struct ObserveTemplate<'a> {
    cols: u16,
    rows: u16,
    generation: u64,
    screen_kind: &'static str,
    title: Option<Quoted<'a>>,
    cursor_col: u16,
    cursor_row: u16,
    cursor_visible: bool,
    active_modes: &'a [String],
    panes: Vec<PaneView<'a>>,
    elements: Vec<ElementView<'a>>,
}

/// Renders an [`Observation`] as natural language via `templates/tui_observe.txt`.
///
/// # Errors
/// Returns an [`askama::Error`] only if template formatting fails.
pub fn render_observation(obs: &Observation) -> Result<String, askama::Error> {
    let panes = obs
        .panes
        .iter()
        .map(|pane| PaneView {
            reference: pane.reference,
            title: pane.title.as_deref().map(Quoted),
            bounds: pane.bounds,
            center: pane.center,
        })
        .collect();
    let elements = obs
        .highlights
        .iter()
        .map(|el| ElementView {
            reference: el.reference,
            kind: el.kind.as_str(),
            label: Quoted(&el.label),
            center: el.center,
        })
        .collect();

    ObserveTemplate {
        cols: obs.cols,
        rows: obs.rows,
        generation: obs.generation,
        screen_kind: if obs.alternate_screen {
            "alternate"
        } else {
            "primary"
        },
        title: obs.title.as_deref().map(Quoted),
        cursor_col: obs.cursor_col,
        cursor_row: obs.cursor_row,
        cursor_visible: obs.cursor_visible,
        active_modes: &obs.active_modes,
        panes,
        elements,
    }
    .render()
}

const fn is_box_drawing(ch: char) -> bool {
    matches!(ch, '\u{2500}'..='\u{257F}')
}

const fn connects_right_and_down(ch: char) -> bool {
    matches!(
        ch,
        '┌' | '┍'
            | '┎'
            | '┏'
            | '╔'
            | '╒'
            | '╓'
            | '╭'
            | '├'
            | '┝'
            | '┞'
            | '┟'
            | '┠'
            | '┡'
            | '┢'
            | '┣'
            | '╠'
            | '╞'
            | '╟'
            | '┬'
            | '┭'
            | '┮'
            | '┯'
            | '┰'
            | '┱'
            | '┲'
            | '┳'
            | '╦'
            | '╤'
            | '╥'
            | '┼'
            | '╬'
            | '╋'
    )
}

const fn connects_left_and_down(ch: char) -> bool {
    matches!(
        ch,
        '┐' | '┑'
            | '┒'
            | '┓'
            | '╗'
            | '╕'
            | '╖'
            | '╮'
            | '┤'
            | '┥'
            | '┦'
            | '┧'
            | '┨'
            | '┩'
            | '┪'
            | '┫'
            | '╣'
            | '╡'
            | '╢'
            | '┬'
            | '┭'
            | '┮'
            | '┯'
            | '┰'
            | '┱'
            | '┲'
            | '┳'
            | '╦'
            | '╤'
            | '╥'
            | '┼'
            | '╬'
            | '╋'
    )
}

const fn connects_right_and_up(ch: char) -> bool {
    matches!(
        ch,
        '└' | '┕'
            | '┖'
            | '┗'
            | '╚'
            | '╘'
            | '╙'
            | '╰'
            | '├'
            | '┝'
            | '┞'
            | '┟'
            | '┠'
            | '┡'
            | '┢'
            | '┣'
            | '╠'
            | '╞'
            | '╟'
            | '┴'
            | '┵'
            | '┶'
            | '┷'
            | '┸'
            | '┹'
            | '┺'
            | '┻'
            | '╩'
            | '╧'
            | '╨'
            | '┼'
            | '╬'
            | '╋'
    )
}

const fn connects_left_and_up(ch: char) -> bool {
    matches!(
        ch,
        '┘' | '┙'
            | '┚'
            | '┛'
            | '╝'
            | '╛'
            | '╜'
            | '╯'
            | '┤'
            | '┥'
            | '┦'
            | '┧'
            | '┨'
            | '┩'
            | '┪'
            | '┫'
            | '╣'
            | '╡'
            | '╢'
            | '┴'
            | '┵'
            | '┶'
            | '┷'
            | '┸'
            | '┹'
            | '┺'
            | '┻'
            | '╩'
            | '╧'
            | '╨'
            | '┼'
            | '╬'
            | '╋'
    )
}

const fn is_horizontal_border(ch: char) -> bool {
    matches!(
        ch,
        '─' | '━'
            | '═'
            | '┄'
            | '┅'
            | '┈'
            | '┉'
            | '╌'
            | '╍'
            | '┬'
            | '┳'
            | '╦'
            | '┴'
            | '┻'
            | '╩'
            | '┼'
            | '╋'
            | '╬'
            | '╤'
            | '╥'
            | '╧'
            | '╨'
    )
}

const fn is_vertical_border(ch: char) -> bool {
    matches!(
        ch,
        '│' | '┃'
            | '║'
            | '┆'
            | '┇'
            | '┊'
            | '┋'
            | '╎'
            | '╏'
            | '├'
            | '┣'
            | '╠'
            | '┤'
            | '┫'
            | '╣'
            | '┼'
            | '╋'
            | '╬'
            | '╞'
            | '╟'
            | '╡'
            | '╢'
    )
}

fn detect_panes(snapshot: &ScreenSnapshot) -> Vec<(CellRect, Option<String>)> {
    let rows = snapshot.rows;
    let cols = snapshot.cols;
    if rows < 3 || cols < 3 {
        return Vec::new();
    }

    let char_at = |c: u16, r: u16| -> char {
        snapshot.cell(c, r).map_or(
            ' ',
            |cell| if cell.is_continuation() { ' ' } else { cell.ch },
        )
    };

    let mut seen = HashSet::new();
    let mut panes = Vec::new();

    for r0 in 0..rows.saturating_sub(2) {
        for c0 in 0..cols.saturating_sub(2) {
            if !connects_right_and_down(char_at(c0, r0)) {
                continue;
            }
            // Find the nearest valid top-right corner along row r0.
            let mut right_edge = None;
            let mut border_chars = 0_usize;
            for c1 in (c0 + 1)..cols {
                let ch = char_at(c1, r0);
                if c1 >= c0 + 2 && connects_left_and_down(ch) && border_chars >= 1 {
                    right_edge = Some(c1);
                    break;
                }
                if is_horizontal_border(ch) {
                    border_chars += 1;
                }
            }
            let Some(c1) = right_edge else {
                continue;
            };

            // Walk down along c0 and c1 to find the nearest closing bottom edge.
            let mut bottom_edge = None;
            for r1 in (r0 + 1)..rows {
                let left = char_at(c0, r1);
                let right = char_at(c1, r1);
                if r1 >= r0 + 2 && connects_right_and_up(left) && connects_left_and_up(right) {
                    // Check bottom edge has horizontal border characters.
                    let bottom_borders = ((c0 + 1)..c1)
                        .filter(|&c| is_horizontal_border(char_at(c, r1)))
                        .count();
                    if bottom_borders >= 1 {
                        bottom_edge = Some(r1);
                        break;
                    }
                }
                if !(is_vertical_border(left) && is_vertical_border(right)) {
                    break;
                }
            }
            let Some(r1) = bottom_edge else {
                continue;
            };

            let rect = CellRect {
                col: c0,
                row: r0,
                width: c1 - c0 + 1,
                height: r1 - r0 + 1,
            };
            if seen.insert(rect) {
                let title = extract_border_title(snapshot, r0, c0 + 1, c1);
                panes.push((rect, title));
            }
        }
    }

    panes
}

fn extract_border_title(
    snapshot: &ScreenSnapshot,
    row: u16,
    col_start: u16,
    col_end: u16,
) -> Option<String> {
    let line = snapshot.cells.get(usize::from(row))?;
    let mut raw = String::new();
    for c in usize::from(col_start)..usize::from(col_end) {
        if let Some(cell) = line.get(c) {
            if !cell.is_continuation() && is_box_drawing(cell.ch) {
                raw.push(' ');
            } else {
                cell.push_text(&mut raw);
            }
        }
    }
    let cleaned = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!cleaned.is_empty()).then_some(cleaned)
}

const fn classify_cell_emphasis(cell: &Cell) -> Option<ElementKind> {
    if cell.style.reverse || !matches!(cell.style.bg, Color::Default) {
        Some(ElementKind::Selection)
    } else if cell.style.underline || (cell.style.bold && !matches!(cell.style.fg, Color::Default))
    {
        Some(ElementKind::Highlight)
    } else {
        None
    }
}

fn detect_styled_spans(snapshot: &ScreenSnapshot) -> Vec<(ElementKind, CellRect, String)> {
    let mut spans = Vec::new();

    for (row_idx, line) in snapshot.cells.iter().enumerate() {
        let Ok(row_u16) = u16::try_from(row_idx) else {
            break;
        };
        let mut c = 0_usize;
        while c < line.len() {
            let cell = &line[c];
            let Some(kind) = classify_cell_emphasis(cell) else {
                c += 1;
                continue;
            };
            if is_box_drawing(cell.ch) {
                c += 1;
                continue;
            }
            let style_bg = cell.style.bg;
            let style_reverse = cell.style.reverse;
            let run_start = c;
            while c < line.len() {
                let cur = &line[c];
                if cur.is_continuation() {
                    c += 1;
                    continue;
                }
                if is_box_drawing(cur.ch) {
                    break;
                }
                let same_kind = classify_cell_emphasis(cur) == Some(kind)
                    && cur.style.bg == style_bg
                    && cur.style.reverse == style_reverse;
                if !same_kind {
                    break;
                }
                c += 1;
            }
            let run_end = c;

            // Trim leading/trailing blank cells from the visual run.
            let mut trimmed_start = run_start;
            while trimmed_start < run_end && line[trimmed_start].is_blank_glyph() {
                trimmed_start += 1;
            }
            let mut trimmed_end = run_end;
            while trimmed_end > trimmed_start && line[trimmed_end - 1].is_blank_glyph() {
                trimmed_end -= 1;
            }
            if trimmed_start >= trimmed_end {
                continue;
            }

            let mut text = String::new();
            for item in &line[trimmed_start..trimmed_end] {
                item.push_text(&mut text);
            }
            let label = text.trim();
            if label.is_empty()
                || !label
                    .chars()
                    .any(|ch| ch.is_alphanumeric() || !ch.is_ascii_punctuation())
            {
                continue;
            }
            let Ok(col_u16) = u16::try_from(trimmed_start) else {
                continue;
            };
            let Ok(width_u16) = u16::try_from(trimmed_end - trimmed_start) else {
                continue;
            };
            spans.push((
                kind,
                CellRect {
                    col: col_u16,
                    row: row_u16,
                    width: width_u16.max(1),
                    height: 1,
                },
                label.to_owned(),
            ));
        }
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui_driver::emulator::TerminalEmulator;

    #[test]
    fn detects_panes_selections_and_refuses_stale_refs() {
        let mut emu = TerminalEmulator::new(40, 10);
        emu.feed(b"\x1b]2;Workspace\x07\x1b[?1000h\x1b[?1006h");
        emu.feed(
            concat!(
                "\x1b[1;1H┌─ Files ──────────┐┌─ Preview ────────┐",
                "\x1b[2;1H│ \x1b[7mCargo.toml\x1b[0m       ││ package = \"may\"  │",
                "\x1b[3;1H│ src/lib.rs       ││                  │",
                "\x1b[4;1H└──────────────────┘└──────────────────┘",
            )
            .as_bytes(),
        );

        let snap1 = emu.snapshot();
        let obs1 = observe_screen(&snap1);
        assert_eq!(obs1.panes.len(), 2);
        assert_eq!(obs1.panes[0].title.as_deref(), Some("Files"));
        assert_eq!(
            obs1.panes[0].bounds,
            CellRect {
                col: 0,
                row: 0,
                width: 20,
                height: 4
            }
        );
        assert_eq!(obs1.panes[1].title.as_deref(), Some("Preview"));
        assert_eq!(
            obs1.panes[1].bounds,
            CellRect {
                col: 20,
                row: 0,
                width: 20,
                height: 4
            }
        );

        assert_eq!(obs1.highlights.len(), 1);
        assert_eq!(obs1.highlights[0].kind, ElementKind::Selection);
        assert_eq!(obs1.highlights[0].label, "Cargo.toml");
        let sel_ref = obs1.highlights[0].reference;
        assert_eq!(sel_ref.generation, snap1.generation);

        let rendered = render_observation(&obs1).expect("render observe");
        assert!(rendered.contains("40x10 cells"), "{rendered}");
        assert!(rendered.contains("title \"Workspace\""), "{rendered}");
        assert!(rendered.contains("mouse:sgr(normal)"), "{rendered}");
        assert!(rendered.contains("pane \"Files\""), "{rendered}");
        assert!(rendered.contains("selection \"Cargo.toml\""), "{rendered}");

        // Valid resolution at current generation.
        let resolved = obs1.table.resolve(sel_ref, snap1.generation).unwrap();
        assert_eq!(resolved.label, "Cargo.toml");

        // Mutate screen to advance generation; old ref is refused as stale.
        emu.feed(b"\x1b[3;3H\x1b[7msrc/lib.rs\x1b[0m");
        let snap2 = emu.snapshot();
        assert!(snap2.generation > snap1.generation);
        let err = obs1
            .table
            .resolve(sel_ref, snap2.generation)
            .expect_err("old ref must be stale");
        assert!(matches!(err, RefResolveError::Stale { .. }));
        let msg = err.render_for_model();
        assert!(msg.contains("Refused, nothing was acted on:"), "{msg}");
        assert!(msg.contains("Run `tui observe` again"), "{msg}");
    }

    #[test]
    fn finds_unique_text_with_cjk_widths_and_refuses_missing_or_ambiguous() {
        let mut emu = TerminalEmulator::new(60, 6);
        emu.feed(
            concat!(
                "\x1b[1;1H菜单: 保存 / 保存",
                "\x1b[2;1H日本語テスト OK",
                "\x1b[3;1H한국어 확인 완료",
            )
            .as_bytes(),
        );
        let snap = emu.snapshot();

        // Unique Japanese target: "日本語テスト" starts at col 0, 6 wide chars = 12 cols -> center col 5, row 1.
        let (col_ja, row_ja) = find_unique_text(&snap, "日本語テスト").unwrap();
        assert_eq!((col_ja, row_ja), (5, 1));

        // "OK" after "日本語テスト " (12 + 1 = col 13..15) -> center col 13, row 1.
        let (col_ok, row_ok) = find_unique_text(&snap, "OK").unwrap();
        assert_eq!((col_ok, row_ok), (13, 1));

        // Unique Korean target on row 2.
        let (col_ko, row_ko) = find_unique_text(&snap, "한국어 확인").unwrap();
        assert_eq!(row_ko, 2);
        assert_eq!(col_ko, 5);

        // Ambiguous Chinese text ("保存" appears twice).
        let err_amb = find_unique_text(&snap, "保存").unwrap_err();
        assert!(matches!(
            err_amb,
            TextTargetError::Ambiguous { count: 2, .. }
        ));
        assert!(err_amb.render_for_model().contains("ambiguous"));

        // Missing text.
        let err_missing = find_unique_text(&snap, "不存在").unwrap_err();
        assert!(matches!(err_missing, TextTargetError::NotFound { .. }));
    }

    #[test]
    fn element_ref_parse_and_display_round_trip() {
        let parsed: TuiElementRef = "42:3".parse().unwrap();
        assert_eq!(parsed.generation, 42);
        assert_eq!(parsed.index, 3);
        assert_eq!(parsed.to_string(), "42:3");

        assert!("42:0".parse::<TuiElementRef>().is_err());
        assert!("abc:1".parse::<TuiElementRef>().is_err());
        assert!("42".parse::<TuiElementRef>().is_err());
    }
}
