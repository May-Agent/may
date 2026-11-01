//! Terminal image rendering support.
//!
//! This module provides image rendering in terminals that support native graphics protocols
//! (Kitty, iTerm2, Sixel). If native protocols are not supported, images are displayed as links.
//!
//! The Picker must be initialized before entering raw mode and is stored in App state.

use std::num::NonZeroU16;

use ratatui::buffer::CellDiffOption;
use ratatui::prelude::*;

use ratatui::Frame;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use base64::Engine as _;
use ratatui_image::picker::cap_parser::Parser;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};

use super::kitty_diacritics::diacritic;

/// Terminal image support state.
/// Created once at startup, before raw mode, and stored in App.
/// Only created if terminal supports native graphics protocols (Kitty, iTerm2, Sixel).
#[derive(Debug)]
pub struct TerminalImageSupport {
    picker: Picker,
    protocol: ProtocolType,
}

impl TerminalImageSupport {
    /// Detect terminal graphics capabilities.
    /// Must be called before entering raw mode.
    ///
    /// Returns Some only if terminal supports native graphics protocols (Kitty, iTerm2, Sixel).
    /// Returns None if only halfblocks (text-based) rendering is available - in this case,
    /// images should be displayed as links instead.
    pub fn detect() -> Option<Self> {
        // Query terminal for native graphics protocol support
        let picker = match Picker::from_query_stdio() {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("Failed to query terminal graphics support: {}", e);
                return None;
            }
        };

        let protocol = picker.protocol_type();

        // Only enable image support for native graphics protocols
        // Halfblocks is text-based and looks bad - show links instead
        match protocol {
            ProtocolType::Kitty => {
                tracing::info!("Terminal image support enabled: Kitty protocol");
                Some(Self { picker, protocol })
            }
            ProtocolType::Iterm2 => {
                tracing::info!("Terminal image support enabled: iTerm2 protocol");
                Some(Self { picker, protocol })
            }
            ProtocolType::Sixel => {
                tracing::info!("Terminal image support enabled: Sixel protocol");
                Some(Self { picker, protocol })
            }
            ProtocolType::Halfblocks => {
                tracing::debug!("No native image protocol detected, images will be shown as links");
                None
            }
        }
    }

    /// Get the detected protocol type.
    #[must_use]
    pub const fn protocol(&self) -> ProtocolType {
        self.protocol
    }

    /// Get a reference to the picker.
    #[must_use]
    pub const fn picker(&self) -> &Picker {
        &self.picker
    }
}

/// Image state for rendering.
/// Created via `TerminalImageSupport`.
pub struct ImageState {
    /// The protocol state for rendering.
    pub proto: ImageProtocol,
}

impl std::fmt::Debug for ImageState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageState").finish_non_exhaustive()
    }
}

/// Per-terminal render protocol.
pub enum ImageProtocol {
    /// Kitty graphics: the encoded image is transmitted once (`f=100`
    /// PNG) and unicode placeholders scale it at every display size — a
    /// size change only re-puts the virtual placement's grid (`a=p`),
    /// never the image bytes.
    Kitty(KittyImage),
    /// iTerm2/Sixel/halfblocks: ratatui-image resizes+encodes per area.
    Other(StatefulProtocol),
}

impl std::fmt::Debug for ImageProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageProtocol").finish_non_exhaustive()
    }
}

/// Kitty render state.
///
/// A prebuilt one-shot transmission plus the image identity placeholders
/// reference. All costly work (encode + base64) is done on the decode
/// worker; rendering only writes the payload once and placeholder cells
/// afterwards.
pub struct KittyImage {
    /// The `f=100` chunked base64 transmission, emitted on first render.
    transmit: String,
    /// Flipped when the transmission has been written once.
    transmitted: AtomicBool,
    /// The placeholder grid the virtual placement was last put on,
    /// packed as `cols << 16 | rows`; 0 while none has been sent.
    placed: AtomicU32,
    /// The image id the transmission and placements refer to.
    id: u32,
    /// Foreground color escape encoding the image id.
    id_color: String,
    /// Third diacritic slot: the image id's most significant byte.
    id_extra: u16,
    /// Whether graphics commands must be wrapped for tmux passthrough.
    is_tmux: bool,
    /// Source pixel dimensions (for fit math).
    width: u32,
    height: u32,
    /// Terminal cell size in pixels.
    font: (u16, u16),
}

impl std::fmt::Debug for KittyImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KittyImage").finish_non_exhaustive()
    }
}

impl KittyImage {
    /// The rect the image paints into when fitted inside `area`,
    /// preserving aspect ratio in whole cells.
    #[must_use]
    pub fn fit_rect(&self, area: Rect) -> Rect {
        let (fw, fh) = (u32::from(self.font.0), u32::from(self.font.1));
        let bw = u64::from(u32::from(area.width) * fw).min(u64::from(self.width));
        let bh = u64::from(u32::from(area.height) * fh).min(u64::from(self.height));
        let (img_w, img_h) = (u64::from(self.width), u64::from(self.height));
        // Scale to the tighter axis, rounding like `round(img * ratio)` —
        // `as` would truncate and never overflow here since img <= u32::MAX.
        let (pw, ph) = if bw * img_h <= bh * img_w {
            (bw, ((bw * img_h + img_w / 2) / img_w).max(1))
        } else {
            (((img_w * bh + img_h / 2) / img_h).max(1), bh)
        };
        Rect::new(
            0,
            0,
            u16::try_from(pw.div_ceil(u64::from(fw))).unwrap_or(u16::MAX),
            u16::try_from(ph.div_ceil(u64::from(fh))).unwrap_or(u16::MAX),
        )
    }
}

/// The placement id the image's single virtual placement is put under.
/// Fixed so a re-`put` replaces the previous grid in place instead of
/// accumulating placements (kitty and ghostty both update on the same
/// `(image id, placement id)` key).
const PLACEMENT_ID: u32 = 1;

/// Build the `a=p` command that (re)creates the image's virtual
/// placement on a `cols`x`rows` grid. The grid is what scales the image:
/// a placement without `c`/`r` falls back to the image's natural size in
/// cells, so drawing fewer placeholder cells than that shows only the
/// top-left part of the image.
fn kitty_place(id: u32, is_tmux: bool, cols: u16, rows: u16) -> String {
    let (start, escape, end) = Parser::escape_tmux(is_tmux);
    format!("{start}{escape}_Gq=2,a=p,U=1,i={id},p={PLACEMENT_ID},c={cols},r={rows}{escape}\\{end}")
}

/// Build the kitty transmission string: `f=100` self-describing image
/// bytes (PNG), chunked at 4 KB of base64 like the protocol requires.
fn kitty_transmit(id: u32, is_tmux: bool, bytes: &[u8]) -> String {
    use std::fmt::Write;

    const CHARS_PER_CHUNK: usize = 4096;
    const CHUNK_SIZE: usize = (CHARS_PER_CHUNK / 4) * 3;

    let (start, escape, end) = Parser::escape_tmux(is_tmux);
    let chunks = bytes.chunks(CHUNK_SIZE);
    let chunk_count = chunks.len();
    let mut data = String::with_capacity(
        chunk_count * (11 + CHARS_PER_CHUNK + escape.len() * 2) + 46 + end.len(),
    );
    for (i, chunk) in chunks.enumerate() {
        data.push_str(start);
        write!(data, "{escape}_Gq=2,").unwrap();
        if i == 0 {
            write!(data, "i={id},a=t,f=100,").unwrap();
        }
        let more = u8::from(chunk_count > (i + 1));
        write!(data, "m={more};").unwrap();
        data.push_str(&base64::engine::general_purpose::STANDARD.encode(chunk));
        write!(data, "{escape}\\").unwrap();
        data.push_str(end);
    }
    data
}

/// Decode image bytes into an [`ImageState`] for `picker`. CPU-bound for
/// large images — run on a worker thread, not the render loop.
#[must_use]
pub fn decode_image_state(picker: &Picker, data: &[u8]) -> Option<ImageState> {
    if picker.protocol_type() == ProtocolType::Kitty {
        return kitty_image_state(picker, data).map(|kitty| ImageState {
            proto: ImageProtocol::Kitty(kitty),
        });
    }
    let image = image::load_from_memory(data).ok()?;
    Some(ImageState {
        proto: ImageProtocol::Other(picker.new_resize_protocol(image)),
    })
}

/// Pass PNG bytes through unchanged when reasonable; anything else (or
/// oversized) decodes and re-encodes to a bounded PNG on this worker —
/// the encode never happens on the render path.
fn kitty_image_state(picker: &Picker, data: &[u8]) -> Option<KittyImage> {
    use std::fmt::Write;

    const MAX_PASSTHROUGH: usize = 8 * 1024 * 1024;
    const MAX_DIM: u32 = 4096;

    let is_png = matches!(image::guess_format(data), Ok(image::ImageFormat::Png));
    let (png, width, height) = if is_png && data.len() <= MAX_PASSTHROUGH {
        let dims = image::ImageReader::new(std::io::Cursor::new(data))
            .with_guessed_format()
            .ok()?
            .into_dimensions()
            .ok()?;
        (data.to_vec(), dims.0, dims.1)
    } else {
        let img = image::load_from_memory(data).ok()?;
        let img = if img.width() > MAX_DIM || img.height() > MAX_DIM {
            img.thumbnail(MAX_DIM, MAX_DIM)
        } else {
            img
        };
        let mut cursor = std::io::Cursor::new(Vec::new());
        img.write_to(&mut cursor, image::ImageFormat::Png).ok()?;
        (cursor.into_inner(), img.width(), img.height())
    };

    let id = fastrand::u32(..);
    let is_tmux = std::env::var_os("TMUX").is_some();
    let [id_extra, r, g, b] = id.to_be_bytes();
    let mut id_color = String::new();
    write!(id_color, "\x1b[38;2;{r};{g};{b}m").unwrap();
    Some(KittyImage {
        transmit: kitty_transmit(id, is_tmux, &png),
        transmitted: AtomicBool::new(false),
        placed: AtomicU32::new(0),
        id,
        id_color,
        id_extra: u16::from(id_extra),
        is_tmux,
        width,
        height,
        font: picker.font_size(),
    })
}

/// Render an image using the stateful protocol.
///
/// The protocols put the whole escape sequence of a row (kitty) or of the
/// image (sixel, iTerm2) into one cell. Ratatui sizes a cell by its
/// symbol's display width, and the printable bytes of the escape sequence
/// (base64 data, CSI parameters) count: the buffer diff then skipped that
/// many following cells — past the image, wrapping into the rows below —
/// both when writing the image and on every later frame it stayed. Text
/// under and after the image stopped updating (stale or doubled lines,
/// leftover gray blocks) and kitty rows lost their placeholders. Pinning
/// those cells to a width of one keeps the diff exact; the rest of the
/// image area is already marked skip by the protocol.
pub fn render_image(frame: &mut Frame, area: Rect, state: &mut ImageState) {
    match &mut state.proto {
        ImageProtocol::Kitty(kitty) => render_kitty(frame.buffer_mut(), area, kitty),
        ImageProtocol::Other(protocol) => {
            let image_widget = StatefulImage::default();
            frame.render_stateful_widget(image_widget, area, protocol);
        }
    }
    pin_escape_cells(frame.buffer_mut(), area);
}

impl ImageState {
    /// The rect the image paints into when fitted inside `area`.
    /// Replaces `StatefulProtocol::size_for(Resize::Fit(None), area)`.
    #[must_use]
    pub fn fitted(&self, area: Rect) -> Rect {
        match &self.proto {
            ImageProtocol::Kitty(kitty) => kitty.fit_rect(area),
            ImageProtocol::Other(protocol) => protocol.size_for(Resize::Fit(None), area),
        }
    }
}

/// Paint kitty unicode placeholders for `area`: the prebuilt transmission
/// once, then placeholder cells only — no per-size encode or transmit.
fn render_kitty(buf: &mut Buffer, area: Rect, kitty: &KittyImage) {
    use std::fmt::Write;

    let area = area.intersection(buf.area);
    let fit = kitty.fit_rect(area);
    let full_width = area.width.min(fit.width);
    let height = area.height.min(fit.height).min(297);
    if full_width == 0 || height == 0 {
        return;
    }

    // Re-put the virtual placement whenever the drawn grid changes. The
    // put replaces the placement under the fixed placement id, so it is
    // flicker-free and costs only a short escape — no image data is
    // re-sent.
    let grid = u32::from(full_width) << 16 | u32::from(height);
    let place = (kitty.placed.swap(grid, Ordering::SeqCst) != grid)
        .then(|| kitty_place(kitty.id, kitty.is_tmux, full_width, height));

    let first = !kitty.transmitted.swap(true, Ordering::SeqCst);
    let row_diacritics: String =
        std::iter::repeat_n('\u{10EEEE}', usize::from(full_width) - 1).collect();
    let restore = format!("\x1b[u\x1b[{}C\x1b[{}B", area.width - 1, area.height - 1);
    let mut symbol = String::new();
    for y in 0..height {
        symbol.clear();
        if y == 0 {
            if first {
                symbol.push_str(&kitty.transmit);
            }
            // The placement must exist before the placeholder cells it
            // scales and come after the transmission that creates the
            // image — both hold inside this symbol.
            if let Some(place) = &place {
                symbol.push_str(place);
            }
        }
        write!(
            symbol,
            "\x1b[s{}\u{10EEEE}{}{}{}",
            kitty.id_color,
            diacritic(y),
            diacritic(0),
            diacritic(kitty.id_extra)
        )
        .unwrap();
        symbol.push_str(&row_diacritics);
        for x in 1..full_width {
            if let Some(cell) = buf.cell_mut((area.left() + x, area.top() + y)) {
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }
        symbol.push_str(&restore);
        if let Some(cell) = buf.cell_mut((area.left(), area.top() + y)) {
            cell.set_symbol(&symbol);
        }
    }
}

/// Give every cell of `area` holding an escape sequence a forced width of
/// one (see [`render_image`]).
pub fn pin_escape_cells(buf: &mut Buffer, area: Rect) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            if cell.diff_option == CellDiffOption::None && cell.symbol().contains('\x1b') {
                cell.set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::MIN));
            }
        }
    }
}

/// Fetch an image from a URL or path asynchronously.
/// Returns the image bytes if successful and the data is a valid image format.
pub async fn fetch_image(url: &str) -> Option<Vec<u8>> {
    // HTTP(S) URL
    let bytes = if url.starts_with("http://") || url.starts_with("https://") {
        match fetch_image_http(url).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!("Failed to fetch image: {}", e);
                return None;
            }
        }
    } else {
        // Try to load as local file
        let path = url.strip_prefix("file://").unwrap_or(url);
        match async_fs::read(path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!("Failed to read image file: {}", e);
                return None;
            }
        }
    };

    tracing::debug!(
        "Fetched {} bytes from {}, first 16 bytes: {:02x?}",
        bytes.len(),
        url,
        &bytes[..bytes.len().min(16)]
    );

    // Validate that the data is actually an image by checking magic bytes
    if !is_valid_image_format(&bytes) {
        tracing::warn!(
            "Fetched data is not a valid image format (url: {}, first bytes: {:02x?})",
            url,
            &bytes[..bytes.len().min(16)]
        );
        return None;
    }

    tracing::debug!("Image validation passed: {}", url);
    Some(bytes)
}

#[derive(Debug, thiserror::Error)]
enum ImageFetchError {
    #[error("failed to prepare image request: {0}")]
    Request(String),
    #[error("failed to download image bytes: {0}")]
    Download(String),
}

/// Check if the data starts with valid image format magic bytes.
fn is_valid_image_format(data: &[u8]) -> bool {
    if data.len() < 8 {
        return false;
    }

    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if data.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return true;
    }

    // JPEG: FF D8 FF
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return true;
    }

    // GIF: GIF87a or GIF89a
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return true;
    }

    // WebP: RIFF....WEBP
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return true;
    }

    // BMP: BM
    if data.starts_with(b"BM") {
        return true;
    }

    // TIFF: II or MM (little/big endian)
    if data.starts_with(&[0x49, 0x49, 0x2A, 0x00]) || data.starts_with(&[0x4D, 0x4D, 0x00, 0x2A]) {
        return true;
    }

    false
}

/// Fetch image bytes from HTTP URL using zenwave.
async fn fetch_image_http(url: &str) -> Result<Vec<u8>, ImageFetchError> {
    use zenwave::{Client, client, header};

    let mut backend = client();
    let bytes = backend
        .get(url)
        .map_err(|error| ImageFetchError::Request(error.to_string()))?
        .header(
            header::USER_AGENT.as_str(),
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        )
        .map_err(|error| ImageFetchError::Request(error.to_string()))?
        .header(header::ACCEPT.as_str(), "image/*,*/*")
        .map_err(|error| ImageFetchError::Request(error.to_string()))?
        .bytes()
        .await
        .map_err(|error| ImageFetchError::Download(error.to_string()))?;

    Ok(bytes.to_vec())
}

/// Render image placeholder when actual rendering is not possible.
#[must_use]
pub fn render_placeholder(alt: &str, url: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled(
                alt.to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" <{url}>"), Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(""),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_terminal_image_support_detect() {
        // In test environment, detection may or may not succeed
        // depending on the terminal. Just verify it doesn't panic.
        let _support = TerminalImageSupport::detect();
    }

    #[test]
    fn test_render_placeholder() {
        let lines = render_placeholder("Alt text", "https://example.com/image.png");
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn kitty_transmit_carries_png_once_with_continuation_chunks() {
        let bytes = vec![0xAB; 10_000];
        let transmit = kitty_transmit(42, false, &bytes);

        // First chunk carries the full action set; later chunks only `m=`.
        assert!(transmit.starts_with("\x1b_Gq=2,i=42,a=t,f=100,m=1;"));
        assert!(transmit.contains("\x1b\\\x1b_Gq=2,m="));
        assert!(transmit.ends_with("\x1b\\"));
        assert_eq!(transmit.matches("\x1b_G").count(), 4);
        // Encoded payload is base64 of the raw bytes.
        let payload: String = transmit
            .split("\x1b_G")
            .skip(1)
            .map(|chunk| {
                chunk
                    .split(';')
                    .nth(1)
                    .unwrap_or_default()
                    .replace("\x1b\\", "")
            })
            .collect();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(payload)
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn kitty_place_targets_the_drawn_grid() {
        let place = kitty_place(42, false, 40, 12);
        assert_eq!(place, "\x1b_Gq=2,a=p,U=1,i=42,p=1,c=40,r=12\x1b\\");
    }

    #[test]
    fn kitty_fit_rect_preserves_aspect_in_cells() {
        let kitty = KittyImage {
            transmit: String::new(),
            transmitted: AtomicBool::new(false),
            placed: AtomicU32::new(0),
            id: 0,
            id_color: String::new(),
            id_extra: 0,
            is_tmux: false,
            width: 3248,
            height: 1992,
            font: (10, 20),
        };
        // 80x25 area of 10x20 cells = 800x500px: image fits at 800x491px -> 80x25 cells.
        let fit = kitty.fit_rect(Rect::new(0, 0, 80, 25));
        assert_eq!(fit, Rect::new(0, 0, 80, 25));
        // A 10x10-cell area caps width at 100px: 100x61px -> 10x4 cells.
        let fit = kitty.fit_rect(Rect::new(0, 0, 10, 10));
        assert_eq!(fit.width, 10);
        assert_eq!(fit.height, 4);
        // Zero area renders nothing.
        assert_eq!(kitty.fit_rect(Rect::new(0, 0, 0, 10)).width, 0);
    }
}
