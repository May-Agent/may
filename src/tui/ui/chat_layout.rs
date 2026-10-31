//! Incremental layout of the chat transcript.
//!
//! Rendering used to rebuild every element's styled lines (a full markdown
//! re-parse of the whole history), wrap the whole document twice (once for
//! the paragraph, once for the selection mirror, with *different* wrapping
//! rules) and let `Paragraph::scroll` re-wrap everything above the view on
//! every frame. Scrolling and streaming therefore cost O(history) per frame
//! and the selection/click mirror could drift from what was on screen.
//!
//! [`ChatLayout`] keeps one laid-out [`Entry`] per [`ChatElement`], keyed by
//! the element's revision (see [`ChatStream::revision`]) and by everything
//! else its rows depend on (width, image support, selection, open state).
//! A frame only re-lays-out elements that changed — while streaming that is
//! the tail element — and renders just the lines that intersect the view.
//! The plain-text mirror used for selection and click hit-testing is read
//! back from the very same wrapped rows, so it cannot disagree with the
//! screen.

use std::sync::OnceLock;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget, Wrap};
use unicode_width::UnicodeWidthStr;

use super::chat::{
    ChatElement, ChatStream, ClickRegion, ElementState, collapsible_id, element_lines,
};
use super::streaming_markdown::IMAGE_HEIGHT;

/// How far ahead the cache looks for an element that moved because
/// elements before it were removed (`set_todos`, rewind).
const REUSE_LOOKAHEAD: usize = 4;

/// Wrapped row count of `line` at `width`, exactly as `Paragraph` with
/// `Wrap { trim: false }` lays it out.
pub(super) fn line_rows(line: &Line<'_>, width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    // A line that fits never wraps; only longer lines need the real
    // word-wrapper (which needs an owned copy).
    if line.width() <= usize::from(width) {
        return 1;
    }
    Paragraph::new(line.clone())
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// Plain text of each wrapped row of `lines`, read back from a scratch
/// render so it matches the screen cell for cell (wide graphemes included).
pub(super) fn plain_rows(lines: &[Line<'_>], width: u16, rows: usize) -> Vec<String> {
    if width == 0 || rows == 0 {
        return Vec::new();
    }
    let height = u16::try_from(rows).unwrap_or(u16::MAX);
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .render(area, &mut buf);
    let mut out = Vec::with_capacity(rows);
    for y in 0..height {
        let mut row = String::new();
        let mut x = 0u16;
        while x < width {
            let symbol = buf[(x, y)].symbol();
            row.push_str(symbol);
            // The trailing cells of a wide grapheme hold blanks that are
            // not part of the text.
            x += u16::try_from(symbol.width().max(1)).unwrap_or(1);
        }
        out.push(row.trim_end().to_string());
    }
    out
}

/// One laid-out element.
#[derive(Debug)]
struct Entry {
    revision: u64,
    state: ElementState,
    lines: Vec<Line<'static>>,
    /// Row offset of each line within the entry.
    line_starts: Vec<usize>,
    rows: usize,
    /// `(key, row within the entry, caption)` of each reserved image block.
    images: Vec<(String, usize, String)>,
    /// Collapsible id a click on this entry toggles.
    collapsible: Option<String>,
    /// The images are markdown references the TUI fetches itself.
    markdown_images: bool,
    /// Plain text per wrapped row, built on first use.
    plain: OnceLock<Vec<String>>,
}

impl Entry {
    fn build(
        elem: &ChatElement,
        revision: u64,
        state: ElementState,
        width: u16,
        images_supported: bool,
    ) -> Self {
        let laid = element_lines(elem, state, images_supported);
        let mut line_starts = Vec::with_capacity(laid.lines.len());
        let mut rows = 0usize;
        for line in &laid.lines {
            line_starts.push(rows);
            rows += line_rows(line, width);
        }
        let images = laid
            .images
            .into_iter()
            .map(|(key, index, caption)| {
                let row = line_starts.get(index).copied().unwrap_or(rows);
                (key, row, caption)
            })
            .collect();
        Self {
            revision,
            state,
            lines: laid.lines,
            line_starts,
            rows,
            images,
            collapsible: collapsible_id(elem).map(str::to_string),
            markdown_images: matches!(elem, ChatElement::AssistantText { .. }),
            plain: OnceLock::new(),
        }
    }

    fn plain(&self, width: u16) -> &[String] {
        self.plain
            .get_or_init(|| plain_rows(&self.lines, width, self.rows))
    }

    /// Index of the line holding entry row `row`.
    fn line_at(&self, row: usize) -> usize {
        self.line_starts
            .partition_point(|&start| start <= row)
            .saturating_sub(1)
    }
}

/// An image block's place in the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAnchor {
    /// Image cache key.
    pub key: String,
    /// First reserved row, in document rows (before top padding).
    pub row: usize,
    /// Path or URL shown under the zoomed image.
    pub caption: String,
}

/// Cached layout of every chat element at one width.
#[derive(Debug, Default)]
pub struct ChatLayout {
    width: u16,
    images_supported: bool,
    hide_todos: bool,
    entries: Vec<Entry>,
    /// First document row of each entry.
    offsets: Vec<usize>,
    /// Rows of all entries together.
    element_rows: usize,
    /// Plain rows of the tail (queue, input, status) drawn last frame.
    tail_plain: Vec<String>,
    /// Blank rows drawn above the content when it underflows the view.
    top_padding: usize,
    /// Elements re-laid-out by the last [`ChatLayout::sync`].
    rebuilt: usize,
}

impl ChatLayout {
    /// Bring the cache in line with `chat` at `width`. Only elements whose
    /// revision or render state changed are laid out again.
    pub fn sync(
        &mut self,
        chat: &ChatStream,
        width: u16,
        images_supported: bool,
        hide_todos: bool,
        selected_tool: Option<&str>,
        now: Instant,
    ) {
        if width != self.width
            || images_supported != self.images_supported
            || hide_todos != self.hide_todos
        {
            self.entries.clear();
            self.width = width;
            self.images_supported = images_supported;
            self.hide_todos = hide_todos;
        }

        let elements = chat.active_elements();
        let mut old = std::mem::take(&mut self.entries)
            .into_iter()
            .map(Some)
            .collect::<Vec<_>>();
        let mut cursor = 0usize;
        let mut entries = Vec::with_capacity(elements.len());
        self.rebuilt = 0;
        for (index, elem) in elements.iter().enumerate() {
            let revision = chat.revision(index);
            let state = ElementState::of(elem, selected_tool, hide_todos, now);
            let end = old.len().min(cursor + REUSE_LOOKAHEAD);
            let found = (cursor..end).find(|&i| {
                old[i]
                    .as_ref()
                    .is_some_and(|entry| entry.revision == revision)
            });
            let reused = found.and_then(|i| {
                cursor = i + 1;
                old[i].take().filter(|entry| entry.state == state)
            });
            let entry = reused.unwrap_or_else(|| {
                self.rebuilt += 1;
                Entry::build(elem, revision, state, width, images_supported)
            });
            entries.push(entry);
        }
        self.entries = entries;
        self.offsets.clear();
        let mut row = 0usize;
        for entry in &self.entries {
            self.offsets.push(row);
            row += entry.rows;
        }
        self.element_rows = row;
    }

    /// Elements re-laid-out by the last sync.
    #[must_use]
    pub const fn rebuilt(&self) -> usize {
        self.rebuilt
    }

    /// Wrapped rows of all elements (the tail excluded).
    #[must_use]
    pub const fn element_rows(&self) -> usize {
        self.element_rows
    }

    /// Entry holding document row `row`.
    fn entry_at(&self, row: usize) -> usize {
        self.offsets
            .partition_point(|&start| start <= row)
            .saturating_sub(1)
    }

    /// The element lines covering document rows `[top, top + height)`
    /// (elements only), plus how many rows of the first line lie above
    /// `top`.
    #[must_use]
    pub fn visible_lines(&self, top: usize, height: usize) -> (Vec<Line<'static>>, usize) {
        let mut out = Vec::new();
        if top >= self.element_rows || height == 0 {
            return (out, 0);
        }
        let bottom = top.saturating_add(height);
        let first = self.entry_at(top);
        let mut skip = 0usize;
        for (index, entry) in self.entries.iter().enumerate().skip(first) {
            let start = self.offsets[index];
            if start >= bottom {
                break;
            }
            let first_line = if index == first {
                let line = entry.line_at(top - start);
                skip = top - start - entry.line_starts.get(line).copied().unwrap_or(0);
                line
            } else {
                0
            };
            for (line_index, line) in entry.lines.iter().enumerate().skip(first_line) {
                if start + entry.line_starts[line_index] >= bottom {
                    break;
                }
                out.push(line.clone());
            }
        }
        (out, skip)
    }

    /// Image blocks of the elements overlapping document rows
    /// `[top, top + height)`.
    #[must_use]
    pub fn images_in(&self, top: usize, height: usize) -> Vec<ImageAnchor> {
        let bottom = top.saturating_add(height);
        let mut out = Vec::new();
        if self.entries.is_empty() {
            return out;
        }
        let first = self.entry_at(top.saturating_sub(IMAGE_HEIGHT));
        for (index, entry) in self.entries.iter().enumerate().skip(first) {
            let start = self.offsets[index];
            if start >= bottom {
                break;
            }
            for (key, row, caption) in &entry.images {
                let row = start + row;
                if row < bottom && row + IMAGE_HEIGHT > top {
                    out.push(ImageAnchor {
                        key: key.clone(),
                        row,
                        caption: caption.clone(),
                    });
                }
            }
        }
        out
    }

    /// Record the plain rows of the tail drawn below the elements.
    pub fn set_tail_plain(&mut self, rows: Vec<String>) {
        self.tail_plain = rows;
    }

    /// Record the blank rows drawn above underflowing content.
    pub const fn set_top_padding(&mut self, rows: usize) {
        self.top_padding = rows;
    }

    /// Blank rows drawn above underflowing content last frame.
    #[must_use]
    pub const fn top_padding(&self) -> usize {
        self.top_padding
    }

    /// Rows of the plain-text mirror: elements, then the tail.
    #[must_use]
    pub const fn plain_len(&self) -> usize {
        self.element_rows + self.tail_plain.len()
    }

    /// Plain text of document row `row`, exactly as drawn.
    #[must_use]
    pub fn plain_line(&self, row: usize) -> Option<&str> {
        if row >= self.element_rows {
            return self
                .tail_plain
                .get(row - self.element_rows)
                .map(String::as_str);
        }
        let index = self.entry_at(row);
        let entry = self.entries.get(index)?;
        entry
            .plain(self.width)
            .get(row - self.offsets[index])
            .map(String::as_str)
    }

    /// Row span of every collapsible element, for click hit-testing.
    #[must_use]
    pub fn collapse_regions(&self) -> Vec<ClickRegion> {
        self.entries
            .iter()
            .zip(&self.offsets)
            .filter_map(|(entry, &start)| {
                entry.collapsible.as_ref().map(|id| ClickRegion {
                    element_id: id.clone(),
                    start,
                    end: start + entry.rows,
                })
            })
            .collect()
    }

    /// URLs/paths of the images assistant messages reference — the ones
    /// the TUI fetches itself (attachments and tool images arrive decoded).
    pub fn markdown_image_keys(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|entry| entry.markdown_images)
            .flat_map(|entry| entry.images.iter().map(|(key, _, _)| key.as_str()))
    }
}
