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

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use super::chat::{
    ChatElement, ChatStream, ClickRegion, ElementState, collapsible_id, element_lines,
};
use super::links::{self, LinkHit, LinkId, LinkRun, ScreenRun};
use super::streaming_markdown::IMAGE_HEIGHT;
use super::wrap;

/// How far ahead the cache looks for an element that moved because
/// elements before it were removed (`set_todos`, rewind).
const REUSE_LOOKAHEAD: usize = 4;

/// Wrapped row count of `line` at `width`, exactly as the chat draws it
/// (see [`wrap`]).
pub(super) fn line_rows(line: &Line<'_>, width: u16) -> usize {
    wrap::line_rows(line, width)
}

/// Plain text of each wrapped row of `lines`, read back from a scratch
/// render so it matches the screen cell for cell (wide graphemes included).
pub(super) fn plain_rows(lines: &[Line<'_>], width: u16, rows: usize) -> Vec<String> {
    scratch(lines, width, rows).plain
}

/// What a scratch render of an element reveals.
#[derive(Debug, Default)]
struct Scratch {
    plain: Vec<String>,
    /// Link cells, from lines tagged by [`links::mark_lines`].
    runs: Vec<LinkRun>,
}

fn scratch(lines: &[Line<'_>], width: u16, rows: usize) -> Scratch {
    if width == 0 || rows == 0 {
        return Scratch::default();
    }
    let height = u16::try_from(rows).unwrap_or(u16::MAX);
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    Paragraph::new(wrap::wrap_lines(lines, width)).render(area, &mut buf);
    let mut out = Vec::with_capacity(rows);
    let mut runs = Vec::new();
    for y in 0..height {
        runs.extend(links::scan_row(&buf, y, width));
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
    Scratch { plain: out, runs }
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
    /// `(id, first row, one past the last)` of collapsible elements folded
    /// inside this entry — an expanded "Worked for" group's members, whose
    /// rows toggle themselves rather than the group.
    sub_regions: Vec<(String, usize, usize)>,
    /// The images are markdown references the TUI fetches itself.
    markdown_images: bool,
    /// Link targets, indexed by the markers of `marked`.
    link_targets: Vec<String>,
    /// `lines` with their links tagged, scratch-rendered instead of
    /// `lines` when the element has links.
    marked: Option<Vec<Line<'static>>>,
    /// Scratch render, built on first use (at build for linked elements).
    scratch: OnceLock<Scratch>,
}

impl Entry {
    fn build(
        elem: &ChatElement,
        revision: u64,
        state: ElementState,
        width: u16,
        images_supported: bool,
        tunnel_base: &str,
    ) -> Self {
        let laid = element_lines(elem, state, images_supported, tunnel_base, width);
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
        let (marked, link_targets) = links::mark_lines(&laid.lines).unzip();
        let collapsible = collapsible_id(elem).map(str::to_string);
        let sub_regions = laid
            .regions
            .iter()
            // The element's own collapsible rows are already covered by the
            // entry's `collapsible` — only the ids of elements folded
            // *inside* it (an expanded "Worked for" group's members)
            // become sub-regions.
            .filter(|(id, _, _)| collapsible.as_ref() != Some(id))
            // `element_lines` records a region only around lines it pushed,
            // so `start < end <= laid.lines.len()`.
            .map(|(id, start, end)| {
                let last = end - 1;
                let past = line_starts[last] + line_rows(&laid.lines[last], width);
                (id.clone(), line_starts[*start], past)
            })
            .collect();
        let entry = Self {
            revision,
            state,
            lines: laid.lines,
            line_starts,
            rows,
            images,
            collapsible,
            sub_regions,
            markdown_images: matches!(elem, ChatElement::AssistantText { .. }),
            link_targets: link_targets.unwrap_or_default(),
            marked,
            scratch: OnceLock::new(),
        };
        // Link cells are needed every frame: find them now, with the
        // layout, not in the draw path.
        if entry.marked.is_some() {
            entry.scratch(width);
        }
        entry
    }

    fn scratch(&self, width: u16) -> &Scratch {
        self.scratch.get_or_init(|| {
            scratch(
                self.marked.as_deref().unwrap_or(&self.lines),
                width,
                self.rows,
            )
        })
    }

    fn plain(&self, width: u16) -> &[String] {
        &self.scratch(width).plain
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
    /// Doc-row span of the composer input last frame — click-to-caret
    /// hit testing. `None` when no input was drawn.
    input_span: Option<(usize, usize)>,
    /// Doc row of the New Year greeting last frame — click hit testing.
    greeting_row: Option<usize>,
    /// Blank rows drawn above the content when it underflows the view.
    top_padding: usize,
    /// Elements re-laid-out by the last [`ChatLayout::sync`].
    rebuilt: usize,
}

impl ChatLayout {
    /// Bring the cache in line with `chat` at `width`. Only elements whose
    /// revision or render state changed are laid out again.
    #[allow(clippy::too_many_arguments)]
    pub fn sync(
        &mut self,
        chat: &ChatStream,
        width: u16,
        images_supported: bool,
        hide_todos: bool,
        selected_tool: Option<&str>,
        spinner_frame: usize,
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
            let state = ElementState::of(elem, selected_tool, hide_todos, spinner_frame);
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
                Entry::build(
                    elem,
                    revision,
                    state,
                    width,
                    images_supported,
                    chat.tunnel_base(),
                )
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

    /// Record the composer input's doc-row span for click-to-caret hit
    /// testing; `None` when the frame drew no input.
    pub const fn set_input_span(&mut self, span: Option<(usize, usize)>) {
        self.input_span = span;
    }

    /// Record the greeting line's doc row; `None` when none was drawn.
    pub const fn set_greeting_row(&mut self, row: Option<usize>) {
        self.greeting_row = row;
    }

    /// The greeting line's doc row last frame.
    #[must_use]
    pub const fn greeting_row(&self) -> Option<usize> {
        self.greeting_row
    }

    /// The composer input's doc-row span `(start, rows)` last frame.
    #[must_use]
    pub const fn input_span(&self) -> Option<(usize, usize)> {
        self.input_span
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

    /// Link cells of the elements overlapping document rows
    /// `[top, top + height)`, placed on screen inside `area`.
    #[must_use]
    pub fn link_runs(&self, top: usize, height: usize, area: Rect) -> Vec<ScreenRun> {
        let bottom = top.saturating_add(height);
        let mut out = Vec::new();
        if self.entries.is_empty() || top >= self.element_rows {
            return out;
        }
        for (index, entry) in self.entries.iter().enumerate().skip(self.entry_at(top)) {
            let start = self.offsets[index];
            if start >= bottom {
                break;
            }
            for run in &entry.scratch(self.width).runs {
                let row = start + run.row;
                if row < top || row >= bottom {
                    continue;
                }
                let y = self.top_padding + row - top;
                out.push(ScreenRun {
                    x: area.x + run.col,
                    y: area.y + u16::try_from(y).unwrap_or(u16::MAX),
                    width: run.width,
                    text: run.text.clone(),
                    hit: LinkHit {
                        id: LinkId {
                            entry: index,
                            link: run.link,
                        },
                        uri: entry.link_targets[run.link as usize].clone(),
                    },
                });
            }
        }
        out
    }

    /// The link whose cells include column `col` of document row `row`
    /// (elements only; every wrapped row of a link shares its identity).
    #[must_use]
    pub fn link_at(&self, row: usize, col: u16) -> Option<LinkHit> {
        if row >= self.element_rows {
            return None;
        }
        let index = self.entry_at(row);
        let entry = self.entries.get(index)?;
        let local = row - self.offsets[index];
        entry
            .scratch(self.width)
            .runs
            .iter()
            .find(|run| run.row == local && (run.col..run.col + run.width).contains(&col))
            .map(|run| LinkHit {
                id: LinkId {
                    entry: index,
                    link: run.link,
                },
                uri: entry.link_targets[run.link as usize].clone(),
            })
    }

    /// Row span of every collapsible element, for click hit-testing.
    /// Elements inside an expanded "Worked for" group appear as their own
    /// regions — the innermost span containing a row wins, so a click on a
    /// member toggles the member and anywhere else toggles the group.
    #[must_use]
    pub fn collapse_regions(&self) -> Vec<ClickRegion> {
        let mut out = Vec::new();
        for (entry, &offset) in self.entries.iter().zip(&self.offsets) {
            if let Some(id) = &entry.collapsible {
                out.push(ClickRegion {
                    element_id: id.clone(),
                    start: offset,
                    end: offset + entry.rows,
                });
            }
            out.extend(
                entry
                    .sub_regions
                    .iter()
                    .map(|(id, start, end)| ClickRegion {
                        element_id: id.clone(),
                        start: offset + start,
                        end: offset + end,
                    }),
            );
        }
        out
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::ui::chat::ChatStream;

    const URL: &str = "https://example.com/some/long/path/segment/that/wraps?query=1";

    fn layout_of(chat: &ChatStream, width: u16) -> ChatLayout {
        let mut layout = ChatLayout::default();
        layout.sync(chat, width, false, false, None, 0);
        layout
    }

    /// Every `(row, col)` of the transcript that hits a link.
    fn hits(layout: &ChatLayout, width: u16) -> Vec<(usize, u16, LinkHit)> {
        let mut out = Vec::new();
        for row in 0..layout.element_rows() {
            for col in 0..width {
                if let Some(hit) = layout.link_at(row, col) {
                    out.push((row, col, hit));
                }
            }
        }
        out
    }

    #[test]
    fn url_in_assistant_text_is_a_link_with_its_exact_cells() {
        let mut chat = ChatStream::new();
        chat.push_text(&format!("See {URL} for details."));
        chat.end_text();
        let layout = layout_of(&chat, 120);
        let found = hits(&layout, 120);
        assert_eq!(found.len(), URL.len());
        assert!(found.iter().all(|(_, _, hit)| hit.uri == URL));
        let (row, first, _) = &found[0];
        let line = layout.plain_line(*row).unwrap();
        let tail = line.get(usize::from(*first)..).unwrap();
        assert_eq!(tail.get(..URL.len()), Some(URL));
    }

    #[test]
    fn wrapped_url_is_one_link_across_every_row() {
        let mut chat = ChatStream::new();
        chat.push_text(&format!("See {URL} for details."));
        chat.end_text();
        let layout = layout_of(&chat, 24);
        let found = hits(&layout, 24);
        let rows: std::collections::BTreeSet<usize> = found.iter().map(|(row, ..)| *row).collect();
        assert!(rows.len() >= 3, "link should wrap over rows: {rows:?}");
        let ids: std::collections::HashSet<LinkId> = found.iter().map(|(.., hit)| hit.id).collect();
        assert_eq!(ids.len(), 1, "one logical link");
        assert!(found.iter().all(|(.., hit)| hit.uri == URL));
        // The hit cells, read back from the rows, spell the whole URL.
        let mut spelled = String::new();
        for (row, col, _) in &found {
            let line = layout.plain_line(*row).unwrap();
            spelled.push_str(line.get(usize::from(*col)..=usize::from(*col)).unwrap());
        }
        assert_eq!(spelled, URL);
    }

    #[test]
    fn link_runs_place_wrapped_rows_on_screen() {
        let mut chat = ChatStream::new();
        chat.push_text(&format!("See {URL} for details."));
        chat.end_text();
        let layout = layout_of(&chat, 24);
        let runs = layout.link_runs(0, 40, Rect::new(2, 5, 24, 40));
        assert!(runs.len() >= 3);
        assert!(runs.iter().all(|run| run.x >= 2 && run.y >= 5));
        let joined: String = runs.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(joined, URL);
    }

    #[test]
    fn path_next_to_wide_text_hits_on_cell_columns() {
        let mut chat = ChatStream::new();
        chat.push_notice("文件 /tmp/笔记/a.md 已保存");
        let layout = layout_of(&chat, 60);
        let found = hits(&layout, 60);
        assert!(found.len() > 4);
        assert!(
            found
                .iter()
                .all(|(.., hit)| hit.uri == "file:///tmp/%E7%AC%94%E8%AE%B0/a.md")
        );
        let (row, col, _) = &found[0];
        // Two wide glyphs and a space precede the path.
        assert!(*col >= 5);
        assert!(layout.link_at(*row, 0).is_none());
        assert!(layout.link_at(*row, 59).is_none());
    }

    #[test]
    fn url_and_path_in_user_message_are_links() {
        let mut chat = ChatStream::new();
        chat.push_user("see https://example.com/docs and /etc/hosts", Vec::new(), 0);
        let layout = layout_of(&chat, 120);
        let uris: std::collections::BTreeSet<String> = hits(&layout, 120)
            .into_iter()
            .map(|(.., hit)| hit.uri)
            .collect();
        assert_eq!(
            uris,
            ["file:///etc/hosts", "https://example.com/docs"]
                .map(str::to_string)
                .into()
        );
    }

    #[test]
    fn text_without_links_has_none() {
        let mut chat = ChatStream::new();
        chat.push_text("run /help, then open src/main.rs");
        chat.end_text();
        assert_eq!(hits(&layout_of(&chat, 60), 60).len(), 0);
    }

    /// Click the innermost collapsible region on `row`, as the mouse handler does.
    fn click(chat: &mut ChatStream, row: usize, width: u16) {
        let layout = layout_of(chat, width);
        let region = layout
            .collapse_regions()
            .into_iter()
            .filter(|r| row >= r.start && row < r.end)
            .min_by_key(|r| r.end - r.start)
            .expect("a collapsible region on the clicked row");
        chat.toggle_expanded(&region.element_id);
    }

    fn rows(chat: &ChatStream, width: u16) -> Vec<String> {
        let layout = layout_of(chat, width);
        (0..layout.element_rows())
            .filter_map(|row| layout.plain_line(row).map(str::to_string))
            .collect()
    }

    #[test]
    fn thinking_only_group_expands_its_thought() {
        let mut chat = ChatStream::new();
        chat.push_user("你好 こんにちは 안녕", Vec::new(), 0);
        chat.push_thinking("先に考える 생각 first body");
        chat.collapse_thinking();
        chat.push_thinking("second body");
        chat.fold_turn_work(4);
        assert_eq!(rows(&chat, 60)[2], "✦ Worked for 4s ▸");
        click(&mut chat, 2, 60);
        let grouped = rows(&chat, 60);
        assert_eq!(grouped[3], "◐ Thought for 0ms ▸", "{grouped:?}");
        click(&mut chat, 3, 60);
        let open = rows(&chat, 60);
        assert!(open.iter().any(|l| l.contains("first body")), "{open:?}");
        assert!(!open.iter().any(|l| l.contains("second body")), "{open:?}");
    }

    #[test]
    fn single_thought_turn_is_its_own_row() {
        let mut chat = ChatStream::new();
        chat.push_user("你好 こんにちは 안녕", Vec::new(), 0);
        chat.push_thinking("only body");
        chat.fold_turn_work(4);
        let folded = rows(&chat, 60);
        assert!(
            !folded.iter().any(|l| l.contains("Worked for")),
            "{folded:?}"
        );
        assert_eq!(folded[2], "◐ Thought for 0ms ▸");
        click(&mut chat, 2, 60);
        let open = rows(&chat, 60);
        assert!(open.iter().any(|l| l.contains("only body")), "{open:?}");
    }
}
