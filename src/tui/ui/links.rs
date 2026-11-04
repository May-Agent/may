//! Hyperlinks in the chat transcript.
//!
//! Detection runs when an element's lines are laid out ([`mark_lines`]):
//! URLs and absolute file paths found in the text of each logical line are
//! tagged on a scratch copy of the line, through the style's underline
//! colour (never drawn), so the scratch render that already produces the
//! plain-text mirror also yields the exact cells of every link, wrapped or
//! not. Only the post-pass [`paint`] touches the frame: it rewrites the
//! first cell of each styled run of a link into an OSC 8 hyperlink with a
//! forced cell width, so ratatui's diff prints the run once and neither
//! counts the escape bytes as columns nor repaints the covered cells.

use std::fmt;
use std::num::NonZeroU16;
use std::path::Path;

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use linkify::{LinkFinder, LinkKind};
use ratatui::buffer::{Buffer, CellDiffOption};
#[cfg(test)]
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;
use url::Url;

/// Longest hyperlink target emitted; terminals drop longer ones.
const MAX_URI_BYTES: usize = 2000;

/// Modifiers that make a click open the link under the pointer. Terminals
/// never report Cmd in mouse events, so Ctrl is the reportable one.
const OPEN_MODIFIERS: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::SUPER);

/// A link found in a line's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// Byte range of the visible text.
    pub range: std::ops::Range<usize>,
    /// Hyperlink target: an `http(s)` URL or a `file://` URL.
    pub uri: String,
}

/// Links in `text`: `http`/`https` URLs, then absolute file paths
/// (`/a/b`, optionally followed by `:line[:col]`) outside those URLs.
#[must_use]
pub fn detect(text: &str) -> Vec<Detected> {
    let mut found = Vec::new();
    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);
    finder.url_must_have_scheme(true);
    for link in finder.links(text) {
        let Ok(url) = Url::parse(link.as_str()) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") || url.as_str().len() > MAX_URI_BYTES {
            continue;
        }
        found.push(Detected {
            range: link.start()..link.end(),
            uri: url.into(),
        });
    }
    let urls = found.len();
    detect_paths(text, &mut found, urls);
    found.sort_by_key(|link| link.range.start);
    found
}

/// Byte that may precede the `/` opening a path.
const fn opens_path(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t' | b'(' | b'[' | b'{' | b'<' | b'"' | b'\'' | b'`' | b'='
    )
}

/// Character that cannot be part of a path in running text.
fn ends_path(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(
            c,
            '"' | '\'' | '`' | '<' | '>' | '|' | '*' | '?' | '\\' | '^'
        )
        || (!c.is_ascii() && !c.is_alphanumeric())
}

/// Append the absolute paths of `text` that do not overlap the first
/// `urls` entries of `found`.
fn detect_paths(text: &str, found: &mut Vec<Detected>, urls: usize) {
    let bytes = text.as_bytes();
    let mut at = 0usize;
    while let Some(offset) = text.get(at..).and_then(|rest| rest.find('/')) {
        let start = at + offset;
        at = start + 1;
        if start > 0 && !opens_path(bytes[start - 1]) {
            continue;
        }
        let rest = text.get(start..).unwrap_or_default();
        let len = rest.find(ends_path).unwrap_or(rest.len());
        let end = start + len;
        at = end.max(at);
        if found[..urls]
            .iter()
            .any(|url| url.range.start < end && start < url.range.end)
        {
            continue;
        }
        let token = text.get(start..end).unwrap_or_default();
        let visible = token.trim_end_matches(is_trailing_punct);
        let target = strip_position(visible);
        let target = target.trim_end_matches(is_trailing_punct);
        // A lone `/word` is a command, not a path; a path has a directory.
        if target.len() < 3 || target.starts_with("//") || target.matches('/').count() < 2 {
            continue;
        }
        let Ok(url) = Url::from_file_path(Path::new(target)) else {
            continue;
        };
        if url.as_str().len() > MAX_URI_BYTES {
            continue;
        }
        found.push(Detected {
            range: start..start + visible.len(),
            uri: url.into(),
        });
    }
}

const fn is_trailing_punct(c: char) -> bool {
    matches!(
        c,
        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"'
    )
}

/// `path` without a trailing `:line` or `:line:col`.
fn strip_position(path: &str) -> &str {
    let mut rest = path;
    for _ in 0..2 {
        match rest.rsplit_once(':') {
            Some((head, digits))
                if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) =>
            {
                rest = head;
            }
            _ => break,
        }
    }
    rest
}

/// The marker colour of link `index` in a scratch render.
#[must_use]
pub const fn marker(index: u32) -> Color {
    let [_, r, g, b] = index.to_be_bytes();
    Color::Rgb(r, g, b)
}

/// The link index a scratch cell's underline colour marks.
#[must_use]
pub const fn marker_index(color: Color) -> Option<u32> {
    match color {
        Color::Rgb(r, g, b) => Some(u32::from_be_bytes([0, r, g, b])),
        _ => None,
    }
}

/// Copy of `lines` with every detected link tagged by [`marker`], and the
/// targets indexed by marker; `None` when the lines hold no link.
///
/// # Panics
///
/// Panics when an element holds more than 16 million links.
#[must_use]
pub fn mark_lines(lines: &[Line<'static>]) -> Option<(Vec<Line<'static>>, Vec<String>)> {
    let mut targets: Vec<String> = Vec::new();
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        let found = if text.contains('/') {
            detect(&text)
        } else {
            Vec::new()
        };
        if found.is_empty() {
            out.push(line.clone());
            continue;
        }
        let first = u32::try_from(targets.len()).expect("link count fits the marker space");
        targets.extend(found.iter().map(|link| link.uri.clone()));
        let mut marked = line.clone();
        marked.spans = tag_spans(&line.spans, &found, first);
        out.push(marked);
    }
    (!targets.is_empty()).then_some((out, targets))
}

/// Split `spans` at the link boundaries and tag the link parts.
fn tag_spans(spans: &[Span<'static>], links: &[Detected], first: u32) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len() + links.len() * 2);
    let mut offset = 0usize;
    for span in spans {
        let content = span.content.as_ref();
        let span_end = offset + content.len();
        let mut cursor = offset;
        for (index, link) in links.iter().enumerate() {
            let from = link.range.start.max(cursor);
            let to = link.range.end.min(span_end);
            if from >= to {
                continue;
            }
            if from > cursor {
                out.push(Span::styled(
                    slice(content, offset, cursor, from),
                    span.style,
                ));
            }
            let id = first + u32::try_from(index).expect("link count fits the marker space");
            let mut style = span.style;
            style.underline_color = Some(marker(id));
            out.push(Span::styled(slice(content, offset, from, to), style));
            cursor = to;
        }
        if cursor < span_end {
            out.push(Span::styled(
                slice(content, offset, cursor, span_end),
                span.style,
            ));
        } else if content.is_empty() {
            out.push(span.clone());
        }
        offset = span_end;
    }
    out
}

/// `content[from - base..to - base]` for absolute byte offsets.
fn slice(content: &str, base: usize, from: usize, to: usize) -> String {
    content
        .get(from - base..to - base)
        .expect("link ranges lie on character boundaries")
        .to_string()
}

/// A link's cells on one wrapped row of a scratch render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRun {
    /// Row within the element.
    pub row: usize,
    /// First cell column.
    pub col: u16,
    /// Cells covered.
    pub width: u16,
    /// Index of the target in the element's link table.
    pub link: u32,
    /// The cells' symbols, to recognise the run on screen.
    pub text: String,
}

/// Symbols of `width` cells from `(x, y)`, a wide grapheme once.
fn cell_text(buf: &Buffer, x: u16, y: u16, width: u16) -> String {
    let mut text = String::new();
    let mut at = x;
    while at < x + width {
        let symbol = buf[(at, y)].symbol();
        text.push_str(symbol);
        at += u16::try_from(symbol.width().max(1)).unwrap_or(1);
    }
    text
}

/// The link runs of row `y` of a scratch buffer rendered from
/// [`mark_lines`] output.
#[must_use]
pub fn scan_row(buf: &Buffer, y: u16, width: u16) -> Vec<LinkRun> {
    let mut runs: Vec<LinkRun> = Vec::new();
    let mut x = 0u16;
    while x < width {
        let cell = &buf[(x, y)];
        let step = u16::try_from(cell.symbol().width().max(1)).unwrap_or(1);
        if let Some(link) = marker_index(cell.underline_color) {
            match runs.last_mut() {
                Some(run) if run.link == link && run.col + run.width == x => run.width += step,
                _ => runs.push(LinkRun {
                    row: usize::from(y),
                    col: x,
                    width: step,
                    link,
                    text: String::new(),
                }),
            }
        }
        x += step;
    }
    for run in &mut runs {
        run.text = cell_text(buf, run.col, y, run.width);
    }
    runs
}

/// Identity of one link in the laid-out transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LinkId {
    /// Element index.
    pub entry: usize,
    /// Index in the element's link table.
    pub link: u32,
}

/// A link under a point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkHit {
    /// Which link.
    pub id: LinkId,
    /// Its target.
    pub uri: String,
}

/// A link run located on screen.
#[derive(Debug, Clone)]
pub struct ScreenRun {
    /// Screen column of the first cell.
    pub x: u16,
    /// Screen row.
    pub y: u16,
    /// Cells covered.
    pub width: u16,
    /// Symbols the run must still show for it to be painted.
    pub text: String,
    /// The link.
    pub hit: LinkHit,
}

/// An OSC 8 hyperlink around `text`.
struct Osc8<'a> {
    id: LinkId,
    uri: &'a str,
    text: &'a str,
}

impl fmt::Display for Osc8<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "\x1b]8;id=may{}-{};{}\x1b\\{}\x1b]8;;\x1b\\",
            self.id.entry, self.id.link, self.uri, self.text
        )
    }
}

/// Underline the hovered link and turn every run into a terminal
/// hyperlink. A run whose cells no longer show its text (a popup or an
/// image covers it) is left alone.
///
/// # Panics
///
/// Never in practice: a run always covers at least one cell.
pub fn paint(buf: &mut Buffer, runs: &[ScreenRun], hovered: Option<LinkId>) {
    let area = buf.area;
    for run in runs {
        if run.y >= area.bottom() || run.x + run.width > area.right() {
            continue;
        }
        if cell_text(buf, run.x, run.y, run.width) != run.text {
            continue;
        }
        if hovered == Some(run.hit.id) {
            for x in run.x..run.x + run.width {
                buf[(x, run.y)].modifier.insert(Modifier::UNDERLINED);
            }
        }
        // One hyperlink cell per run of identical styling, so a selection
        // highlight or the hover underline keeps its exact cells.
        let mut x = run.x;
        while x < run.x + run.width {
            let style = style_key(buf, x, run.y);
            let mut end = x;
            let mut symbols = String::new();
            while end < run.x + run.width && style_key(buf, end, run.y) == style {
                let symbol = buf[(end, run.y)].symbol();
                symbols.push_str(symbol);
                end += u16::try_from(symbol.width().max(1)).unwrap_or(1);
            }
            let end = end.min(run.x + run.width);
            let cells = NonZeroU16::new(end - x).expect("a run covers at least one cell");
            let linked = Osc8 {
                id: run.hit.id,
                uri: &run.hit.uri,
                text: &symbols,
            }
            .to_string();
            let cell = &mut buf[(x, run.y)];
            cell.set_symbol(&linked);
            cell.set_diff_option(CellDiffOption::ForcedWidth(cells));
            x = end;
        }
    }
}

fn style_key(buf: &Buffer, x: u16, y: u16) -> (Color, Color, Modifier, Color) {
    let cell = &buf[(x, y)];
    (cell.fg, cell.bg, cell.modifier, cell.underline_color)
}

/// Opens a link target in the platform's default application.
pub trait LinkOpener: Send + Sync + std::fmt::Debug {
    /// Open `uri` without blocking the caller.
    fn open(&self, uri: &str);
}

/// The platform opener, run off the event loop.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemOpener;

impl LinkOpener for SystemOpener {
    fn open(&self, uri: &str) {
        let uri = uri.to_string();
        blocking::unblock(move || {
            if let Err(error) = open::that_detached(&uri) {
                tracing::warn!(%uri, %error, "could not open link");
            }
        })
        .detach();
    }
}

/// Open the link under a mouse press that carries an open modifier.
///
/// Returns whether the event was a link click and so is consumed; every
/// other press, drag, release and wheel event is left to the selection
/// and scroll handling.
pub fn dispatch_click(opener: &dyn LinkOpener, mouse: &MouseEvent, hit: Option<&LinkHit>) -> bool {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left)
        || !mouse.modifiers.intersects(OPEN_MODIFIERS)
    {
        return false;
    }
    let Some(hit) = hit else {
        return false;
    };
    opener.open(&hit.uri);
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn links(text: &str) -> Vec<(&str, String)> {
        detect(text)
            .into_iter()
            .map(|link| (text.get(link.range).unwrap(), link.uri))
            .collect()
    }

    #[test]
    fn url_stops_before_trailing_punctuation_and_parenthesis() {
        let got = links("see (https://example.com/a?b=1), then https://example.org/x.");
        assert_eq!(
            got,
            vec![
                (
                    "https://example.com/a?b=1",
                    "https://example.com/a?b=1".into()
                ),
                ("https://example.org/x", "https://example.org/x".into()),
            ]
        );
    }

    #[test]
    fn non_web_schemes_are_not_linked() {
        assert_eq!(
            links("javascript:alert(1) ftp://host/file mailto:a@b.c").len(),
            0
        );
    }

    #[test]
    fn absolute_path_links_to_a_file_url_without_punctuation_or_position() {
        let got = links("edited /Users/me/src/main.rs:42:7, and /tmp/a b.");
        assert_eq!(
            got,
            vec![
                (
                    "/Users/me/src/main.rs:42:7",
                    "file:///Users/me/src/main.rs".into()
                ),
                ("/tmp/a", "file:///tmp/a".into()),
            ]
        );
    }

    #[test]
    fn slash_commands_and_relative_paths_are_not_links() {
        assert_eq!(
            links("run /help or src/main.rs or a/b/c or // comment").len(),
            0
        );
    }

    #[test]
    fn path_in_url_is_not_linked_twice() {
        let got = links("https://example.com/a/b/c");
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn non_ascii_text_around_links_keeps_exact_ranges() {
        let text = "文件 /Users/李/笔记.md。再见 https://example.com/路径 终";
        let got = links(text);
        assert_eq!(got[0].0, "/Users/李/笔记.md");
        assert_eq!(got[0].1, "file:///Users/%E6%9D%8E/%E7%AC%94%E8%AE%B0.md");
        assert_eq!(got[1].0, "https://example.com/路径");
        assert!(got[1].1.starts_with("https://example.com/%E8%B7%AF"));
    }

    #[test]
    fn mark_lines_tags_exactly_the_link_cells_across_spans() {
        let lines = vec![Line::from(vec![
            Span::raw("open "),
            Span::raw("https://exa"),
            Span::raw("mple.com/x now"),
        ])];
        let (marked, targets) = mark_lines(&lines).unwrap();
        assert_eq!(targets, vec!["https://example.com/x".to_string()]);
        let tagged: String = marked[0]
            .spans
            .iter()
            .filter(|span| span.style.underline_color.is_some())
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(tagged, "https://example.com/x");
        let all: String = marked[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(all, "open https://example.com/x now");
        assert!(mark_lines(&[Line::raw("no links /here")]).is_none());
    }

    #[derive(Debug, Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl LinkOpener for Recorder {
        fn open(&self, uri: &str) {
            self.0.lock().unwrap().push(uri.to_string());
        }
    }

    fn mouse(kind: MouseEventKind, modifiers: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column: 3,
            row: 1,
            modifiers,
        }
    }

    fn hit() -> LinkHit {
        LinkHit {
            id: LinkId { entry: 0, link: 0 },
            uri: "https://example.com/".into(),
        }
    }

    #[test]
    fn ctrl_or_super_press_on_a_link_opens_it() {
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::SUPER,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ] {
            let opener = Recorder::default();
            let event = mouse(MouseEventKind::Down(MouseButton::Left), modifiers);
            assert!(dispatch_click(&opener, &event, Some(&hit())));
            assert_eq!(*opener.0.lock().unwrap(), vec!["https://example.com/"]);
        }
    }

    #[test]
    fn plain_click_drag_release_right_click_and_wheel_never_open() {
        let opener = Recorder::default();
        let cases = [
            mouse(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE),
            mouse(MouseEventKind::Down(MouseButton::Left), KeyModifiers::SHIFT),
            mouse(MouseEventKind::Down(MouseButton::Left), KeyModifiers::ALT),
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                KeyModifiers::CONTROL,
            ),
            mouse(MouseEventKind::Up(MouseButton::Left), KeyModifiers::CONTROL),
            mouse(
                MouseEventKind::Down(MouseButton::Right),
                KeyModifiers::CONTROL,
            ),
            mouse(MouseEventKind::Moved, KeyModifiers::CONTROL),
            mouse(MouseEventKind::ScrollDown, KeyModifiers::CONTROL),
        ];
        for event in &cases {
            assert!(!dispatch_click(&opener, event, Some(&hit())), "{event:?}");
        }
        assert!(opener.0.lock().unwrap().is_empty());
    }

    #[test]
    fn ctrl_click_off_a_link_is_not_consumed() {
        let opener = Recorder::default();
        let event = mouse(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::CONTROL,
        );
        assert!(!dispatch_click(&opener, &event, None));
        assert!(opener.0.lock().unwrap().is_empty());
    }

    #[test]
    fn paint_wraps_the_run_in_one_hyperlink_cell_with_forced_width() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 1));
        buf.set_string(0, 0, "go http://a.b", ratatui::style::Style::default());
        let hit = hit();
        let run = ScreenRun {
            x: 3,
            y: 0,
            width: 9,
            text: "http://a.".into(),
            hit: hit.clone(),
        };
        paint(&mut buf, &[run], Some(hit.id));
        assert_eq!(
            buf[(3, 0)].symbol(),
            "\x1b]8;id=may0-0;https://example.com/\x1b\\http://a.\x1b]8;;\x1b\\"
        );
        assert_eq!(
            buf[(3, 0)].diff_option,
            CellDiffOption::ForcedWidth(NonZeroU16::new(9).unwrap())
        );
        assert!(buf[(5, 0)].modifier.contains(Modifier::UNDERLINED));
        assert!(!buf[(0, 0)].modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn paint_skips_a_run_covered_by_something_else() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        buf.set_string(0, 0, "XXXXXXXX", ratatui::style::Style::default());
        let run = ScreenRun {
            x: 0,
            y: 0,
            width: 4,
            text: "http".into(),
            hit: hit(),
        };
        paint(&mut buf, &[run], None);
        assert_eq!(buf[(0, 0)].symbol(), "X");
    }
}
